import akka.actor.Cancellable;
import akka.actor.typed.*;
import akka.actor.typed.javadsl.*;
import java.time.Duration;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/** Actual Typed actors; mutable execution and IO state belongs to the actor. */
final class BenchRuntime implements AutoCloseable {
  static final long WAIT_SECONDS = 3600;

  interface Msg {}

  record Work(long iterations, long salt, CompletableFuture<Long> reply) implements Msg {}

  record Snapshot(long count, long checksum) {}

  record Fence(CompletableFuture<Snapshot> reply) implements Msg {}

  record Gate(CompletableFuture<Void> entered, CountDownLatch release) implements Msg {}

  record Chunk(long total, long size, long salt, CompletableFuture<Long> reply, Gate afterFirst)
      implements Msg {}

  record State(boolean active, int queued) {}

  record Inspect(CompletableFuture<State> reply) implements Msg {}

  enum Continue implements Msg {
    INSTANCE
  }

  enum Done implements Msg {
    INSTANCE
  }

  enum Stop implements Msg {
    INSTANCE
  }

  enum Crash implements Msg {
    INSTANCE
  }

  interface Control {}

  record Spawn(Handle handle, long ioMillis, CompletableFuture<Void> reply) implements Control {}

  record Removed(Handle handle) implements Control {}

  final AtomicInteger live = new AtomicInteger();
  final AtomicInteger retired = new AtomicInteger();
  final ActorSystem<Control> system;
  final List<Handle> handles = new ArrayList<>();
  int sequence;

  BenchRuntime() {
    system =
        ActorSystem.create(
            Behaviors.setup(
                ctx ->
                    Behaviors.receive(Control.class)
                        .onMessage(
                            Spawn.class,
                            m -> {
                              m.handle.ref =
                                  ctx.spawn(actor(m.handle, m.ioMillis), "a" + sequence++);
                              live.incrementAndGet();
                              ctx.watchWith(m.handle.ref, new Removed(m.handle));
                              m.reply.complete(null);
                              return Behaviors.same();
                            })
                        .onMessage(
                            Removed.class,
                            m -> {
                              live.decrementAndGet();
                              retired.incrementAndGet();
                              m.handle.retired.complete(null);
                              m.handle.requests.forEach(
                                  f ->
                                      f.completeExceptionally(
                                          new CancellationException("actor terminated")));
                              return Behaviors.same();
                            })
                        .build()),
            "benchmark");
  }

  static final class Handle {
    volatile ActorRef<Msg> ref;
    final AtomicLong firstCpuStart = new AtomicLong();
    final AtomicLong lastCpuEnd = new AtomicLong();
    final AtomicBoolean abort = new AtomicBoolean();
    final CompletableFuture<Void> retired = new CompletableFuture<>();
    final CompletableFuture<Void> postStop = new CompletableFuture<>();
    final Set<CompletableFuture<?>> requests = ConcurrentHashMap.newKeySet();

    <T> CompletableFuture<T> track(CompletableFuture<T> f) {
      requests.add(f);
      f.whenComplete((v, e) -> requests.remove(f));
      if (retired.isDone()) f.completeExceptionally(new CancellationException("actor terminated"));
      return f;
    }

    State state() throws Exception {
      CompletableFuture<State> f = track(new CompletableFuture<>());
      ref.tell(new Inspect(f));
      return get(f);
    }

    CompletableFuture<Long> ask(long iterations, long salt) {
      CompletableFuture<Long> f = track(new CompletableFuture<>());
      if (!retired.isDone()) ref.tell(new Work(iterations, salt, f));
      return f;
    }

    void tell(long iterations, long salt) {
      ref.tell(new Work(iterations, salt, null));
    }

    Snapshot snapshot() throws Exception {
      CompletableFuture<Snapshot> f = track(new CompletableFuture<>());
      ref.tell(new Fence(f));
      return get(f);
    }

    Gate gate() throws Exception {
      Gate g = new Gate(new CompletableFuture<>(), new CountDownLatch(1));
      ref.tell(g);
      get(g.entered);
      return g;
    }

    void retire() throws Exception {
      if (!retired.isDone()) ref.tell(Stop.INSTANCE);
      retired.get(10, TimeUnit.SECONDS);
    }
  }

  Handle spawn() throws Exception {
    return spawn(0);
  }

  Handle spawn(long ioMillis) throws Exception {
    Handle h = new Handle();
    handles.add(h);
    CompletableFuture<Void> f = new CompletableFuture<>();
    system.tell(new Spawn(h, ioMillis, f));
    get(f);
    return h;
  }

  static long burn(long iterations, long salt) {
    return segment(salt | 1, 0, iterations, null);
  }

  static long segment(long value, long begin, long end, AtomicBoolean abort) {
    for (long i = begin; i < end; i++) {
      if ((i & 65535) == 0 && abort != null && abort.get())
        throw new CancellationException("owner cancelled computation");
      value = (value * 6364136223846793005L + 1442695040888963407L) ^ i;
    }
    return value;
  }

  static void require(boolean condition, String reason) {
    if (!condition) throw new IllegalStateException(reason);
  }

  static <T> T get(CompletableFuture<T> f) throws Exception {
    return f.get(WAIT_SECONDS, TimeUnit.SECONDS);
  }

  static void hold(Gate gate, Handle h) throws InterruptedException {
    gate.entered.complete(null);
    while (!gate.release.await(10, TimeUnit.MILLISECONDS))
      if (h.abort.get()) throw new CancellationException("owner cancelled gate");
  }

  static Behavior<Msg> actor(Handle h, long ioMillis) {
    return Behaviors.setup(
        ctx ->
            new AbstractBehavior<Msg>(ctx) {
              long count, checksum;
              Work active;
              Cancellable timer;
              final ArrayDeque<Msg> pending = new ArrayDeque<>();
              Chunk job;
              long position, value;

              void finish(Work m, long result) {
                count++;
                checksum += result;
                if (m.reply != null) m.reply.complete(result);
              }

              Behavior<Msg> accept(Msg msg) throws Exception {
                if (msg instanceof Inspect m) {
                  m.reply.complete(new State(active != null, pending.size()));
                  return this;
                }
                if (msg == Crash.INSTANCE)
                  throw new IllegalStateException("injected actor failure");
                if (msg == Stop.INSTANCE) return Behaviors.stopped();
                if (msg == Done.INSTANCE) {
                  require(active != null, "completion without active IO");
                  finish(active, active.salt);
                  active = null;
                  if (timer != null) timer.cancel();
                  timer = null;
                  while (active == null && !pending.isEmpty()) {
                    Behavior<Msg> next = accept(pending.removeFirst());
                    if (next != this) return next;
                  }
                  return this;
                }
                if (active != null) {
                  pending.addLast(msg);
                  return this;
                }
                if (msg instanceof Work m) {
                  active = m;
                  if (ioMillis > 0) {
                    require(m.iterations < 0, "IO only accepts echo payloads");
                    timer =
                        ctx.scheduleOnce(Duration.ofMillis(ioMillis), ctx.getSelf(), Done.INSTANCE);
                  } else {
                    if (m.iterations >= 0) h.firstCpuStart.compareAndSet(0, System.nanoTime());
                    long result =
                        m.iterations < 0 ? m.salt : segment(m.salt | 1, 0, m.iterations, h.abort);
                    if (m.iterations >= 0) h.lastCpuEnd.set(System.nanoTime());
                    finish(m, result);
                    active = null;
                  }
                } else if (msg instanceof Fence m) {
                  m.reply.complete(new Snapshot(count, checksum));
                } else if (msg instanceof Gate m) {
                  hold(m, h);
                } else if (msg instanceof Chunk m) {
                  require(
                      job == null && m.total > 0 && m.size > 0, "invalid or concurrent chunk job");
                  job = m;
                  position = 0;
                  value = m.salt | 1;
                  ctx.getSelf().tell(Continue.INSTANCE);
                } else if (msg == Continue.INSTANCE) {
                  require(job != null, "continuation without job");
                  long end = position + Math.min(job.size, job.total - position);
                  value = segment(value, position, end, h.abort);
                  position = end;
                  if (position == Math.min(job.size, job.total) && job.afterFirst != null)
                    hold(job.afterFirst, h);
                  if (position == job.total) {
                    count++;
                    checksum += value;
                    job.reply.complete(value);
                    job = null;
                  } else ctx.getSelf().tell(Continue.INSTANCE);
                } else throw new IllegalArgumentException("unrecognized message");
                return this;
              }

              void fail(Msg m) {
                CancellationException e =
                    new CancellationException("actor stopped before completion");
                if (m instanceof Work w && w.reply != null) w.reply.completeExceptionally(e);
                if (m instanceof Fence f) f.reply.completeExceptionally(e);
                if (m instanceof Gate g) g.entered.completeExceptionally(e);
                if (m instanceof Chunk c) c.reply.completeExceptionally(e);
              }

              @Override
              public Receive<Msg> createReceive() {
                return newReceiveBuilder()
                    .onAnyMessage(this::accept)
                    .onSignal(
                        PostStop.class,
                        ignored -> {
                          if (timer != null) timer.cancel();
                          if (active != null) fail(active);
                          if (job != null) fail(job);
                          pending.forEach(this::fail);
                          pending.clear();
                          h.postStop.complete(null);
                          return this;
                        })
                    .build();
              }
            });
  }

  void retireAll() throws Exception {
    for (Handle h : handles) if (!h.retired.isDone()) h.ref.tell(Stop.INSTANCE);
    long start = System.nanoTime();
    for (Handle h : handles)
      h.retired.get(
          Math.max(1, TimeUnit.SECONDS.toNanos(10) - (System.nanoTime() - start)),
          TimeUnit.NANOSECONDS);
    require(live.get() == 0, "actor leak");
    handles.clear();
  }

  @Override
  public void close() {
    for (Handle h : handles) h.abort.set(true);
    try {
      try {
        retireAll();
      } finally {
        system.terminate();
        system.getWhenTerminated().toCompletableFuture().get(10, TimeUnit.SECONDS);
      }
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
      throw new IllegalStateException("cleanup interrupted", e);
    } catch (Exception e) {
      throw new IllegalStateException("cleanup failed", e);
    }
  }

  @FunctionalInterface
  interface Task {
    void run(int index) throws Exception;
  }

  static void parallel(int workers, Task task) throws Exception {
    ExecutorService pool = Executors.newFixedThreadPool(workers);
    List<Future<Void>> futures = new ArrayList<>();
    CompletionService<Void> completed = new ExecutorCompletionService<>(pool);
    try {
      for (int i = 0; i < workers; i++) {
        final int index = i;
        futures.add(
            completed.submit(
                () -> {
                  task.run(index);
                  return null;
                }));
      }
      long start = System.nanoTime();
      for (int i = 0; i < workers; i++) {
        Future<Void> result =
            completed.poll(
                Math.max(1, TimeUnit.SECONDS.toNanos(WAIT_SECONDS) - (System.nanoTime() - start)),
                TimeUnit.NANOSECONDS);
        if (result == null) throw new TimeoutException("worker deadline");
        result.get();
      }
    } finally {
      futures.forEach(f -> f.cancel(true));
      pool.shutdownNow();
      require(pool.awaitTermination(10, TimeUnit.SECONDS), "worker did not join");
    }
  }
}
