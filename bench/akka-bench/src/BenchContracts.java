import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/** Behavioral rejection tests: each case can fail independently of a benchmark row. */
public final class BenchContracts {
  @FunctionalInterface
  interface Check {
    void run() throws Exception;
  }

  static int passed;

  static void check(String name, Check test) throws Exception {
    test.run();
    passed++;
    System.out.println("CONTRACT " + name + " PASS");
  }

  static void reject(Check test) throws Exception {
    try {
      test.run();
    } catch (IllegalStateException expected) {
      return;
    }
    throw new AssertionError("invalid behavior was accepted");
  }

  static void failed(CompletableFuture<?> f) throws Exception {
    try {
      f.get(2, TimeUnit.SECONDS);
    } catch (ExecutionException | CancellationException expected) {
      return;
    }
    throw new AssertionError("request did not fail");
  }

  static void eq(long a, long b) {
    AkkaBench.equal(a, b);
  }

  public static void main(String[] args) throws Exception {
    check(
        "literal-kernel-vectors",
        () -> {
          long[][] vectors = {
            {0, 0, 1},
            {1, 0, 7806831264735756412L},
            {2, 0, Long.parseUnsignedLong("9396908728118811418")},
            {10, 7, 6404011227278016048L},
            {64, 15, 2931105770476240399L},
            {1, -1, Long.parseUnsignedLong("13525302890751722018")}
          };
          for (long[] v : vectors) eq(AkkaBench.burnCpu(v[0], v[1]), v[2]);
        });
    check(
        "42-does-not-end-work",
        () -> {
          eq(AkkaBench.burnCpu(1, 3639755004476521255L), 42);
          eq(
              AkkaBench.burnCpu(2, 3639755004476521255L),
              Long.parseUnsignedLong("10481999410520546992"));
        });
    check(
        "continuous-chunk-kernel",
        () -> {
          long value = 7;
          for (long i = 0; i < 10; i += 2) value = BenchRuntime.segment(value, i, i + 2, null);
          eq(value, 6404011227278016048L);
        });
    check(
        "false-row-rejected",
        () -> reject(() -> AkkaBench.row("bad", 1, .1, List.of(), false, "")));
    check(
        "invalid-wall-rejected",
        () -> {
          for (double w : new double[] {0, -1, Double.NaN, Double.POSITIVE_INFINITY})
            reject(() -> AkkaBench.row("bad", 1, w, List.of(), true, ""));
        });
    check(
        "nearest-rank",
        () -> {
          AkkaBench.Lat lat = AkkaBench.Lat.of(List.of(10L, 20L));
          eq(lat.p50(), 10);
          eq(lat.p90(), 20);
          eq(lat.p99(), 20);
          List<Long> hundred = new ArrayList<>();
          for (long i = 100; i > 0; i--) hundred.add(i);
          lat = AkkaBench.Lat.of(hundred);
          eq(lat.p50(), 50);
          eq(lat.p90(), 90);
          eq(lat.p99(), 99);
          eq(lat.max(), 100);
        });
    check(
        "state-mismatch-rejected",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn();
            AkkaBench.ask(h, -1, 7, 7);
            reject(() -> AkkaBench.snapshot(h, 2, 7));
            reject(() -> AkkaBench.snapshot(h, 1, 8));
            AkkaBench.snapshot(h, 1, 7);
          }
        });
    check(
        "cpu-count-once-and-checksum-wrap",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn();
            AkkaBench.ask(h, 1, 0, 7806831264735756412L);
            AkkaBench.ask(h, 1, 0, 7806831264735756412L);
            AkkaBench.snapshot(h, 2, 7806831264735756412L * 2);
          }
        });
    check(
        "timeout-is-not-cancellation",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn();
            var g = h.gate();
            var f = h.ask(-1, 93);
            try {
              AkkaBench.expectTimeout(f, 1);
            } finally {
              g.release().countDown();
            }
            eq(BenchRuntime.get(f), 93);
            AkkaBench.snapshot(h, 1, 93);
          }
        });
    check(
        "fifo-tail-after-all-work",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn();
            var g = h.gate();
            h.tell(-1, 3);
            h.tell(-1, 8);
            var f = h.ask(-1, 13);
            try {
              BenchRuntime.require(!f.isDone(), "tail jumped gate");
            } finally {
              g.release().countDown();
            }
            eq(BenchRuntime.get(f), 13);
            AkkaBench.snapshot(h, 3, 24);
          }
        });
    check(
        "observed-termination-and-dead-ask",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn();
            h.retire();
            BenchRuntime.require(h.postStop.isDone(), "no PostStop");
            eq(r.live.get(), 0);
            eq(r.retired.get(), 1);
            failed(h.ask(-1, 1));
          }
        });
    check(
        "io-queue-is-serialized",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn(60000);
            var a = h.ask(-1, 4);
            var b = h.ask(-1, 9);
            var c = h.ask(-1, 12);
            var s = h.state();
            BenchRuntime.require(s.active() && s.queued() == 2, "IO queue not admitted");
            BenchRuntime.require(
                !a.isDone() && !b.isDone() && !c.isDone(),
                "IO completed before completion message");
            h.ref.tell(BenchRuntime.Done.INSTANCE);
            eq(BenchRuntime.get(a), 4);
            s = h.state();
            BenchRuntime.require(s.active() && s.queued() == 1 && !b.isDone(), "overlapping IO");
            h.ref.tell(BenchRuntime.Done.INSTANCE);
            eq(BenchRuntime.get(b), 9);
            h.ref.tell(BenchRuntime.Done.INSTANCE);
            eq(BenchRuntime.get(c), 12);
            AkkaBench.snapshot(h, 3, 25);
          }
        });
    check(
        "real-io-timer-and-order",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn(20);
            long start = System.nanoTime();
            var a = h.ask(-1, 51);
            var b = h.ask(-1, 52);
            eq(BenchRuntime.get(a), 51);
            eq(BenchRuntime.get(b), 52);
            BenchRuntime.require(
                System.nanoTime() - start >= TimeUnit.MILLISECONDS.toNanos(40),
                "IO delays overlapped");
            AkkaBench.snapshot(h, 2, 103);
          }
        });
    check(
        "stop-cancels-active-and-queued-io",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn(60000);
            var a = h.ask(-1, 4);
            var b = h.ask(-1, 9);
            BenchRuntime.require(h.state().queued() == 1, "queue not admitted");
            h.retire();
            failed(a);
            failed(b);
            eq(r.live.get(), 0);
          }
        });
    check(
        "actor-failure-propagates-to-queued-requests",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn();
            var g = h.gate();
            h.ref.tell(BenchRuntime.Crash.INSTANCE);
            var f = h.ask(-1, 5);
            g.release().countDown();
            failed(f);
            h.retired.get(2, TimeUnit.SECONDS);
            eq(r.live.get(), 0);
          }
        });
    check(
        "chunk-probe-before-job-completion",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn();
            var g = new BenchRuntime.Gate(new CompletableFuture<>(), new CountDownLatch(1));
            CompletableFuture<Long> job = h.track(new CompletableFuture<>());
            h.ref.tell(new BenchRuntime.Chunk(10, 2, 7, job, g));
            BenchRuntime.get(g.entered());
            var p = h.ask(-1, 71);
            var before = p.thenApply(v -> !job.isDone());
            g.release().countDown();
            eq(BenchRuntime.get(p), 71);
            BenchRuntime.require(BenchRuntime.get(before), "probe waited for whole job");
            eq(BenchRuntime.get(job), 6404011227278016048L);
            AkkaBench.snapshot(h, 2, 6404011227278016048L + 71);
          }
        });
    check(
        "all-storm-actors-retired",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            List<BenchRuntime.Handle> hs = new ArrayList<>();
            for (int i = 0; i < 64; i++) hs.add(r.spawn());
            for (int i = 0; i < 64; i++) AkkaBench.ask(hs.get(i), -1, i, i);
            r.retireAll();
            eq(r.retired.get(), 64);
            eq(r.live.get(), 0);
            for (var h : hs)
              BenchRuntime.require(h.postStop.isDone() && h.retired.isDone(), "retirement missing");
          }
        });
    check(
        "worker-failure-not-hidden-by-blocked-worker",
        () -> {
          AtomicBoolean joined = new AtomicBoolean();
          CountDownLatch entered = new CountDownLatch(1);
          long start = System.nanoTime();
          try {
            BenchRuntime.parallel(
                2,
                i -> {
                  if (i == 0) {
                    entered.countDown();
                    try {
                      new CountDownLatch(1).await();
                    } finally {
                      joined.set(true);
                    }
                  } else {
                    entered.await();
                    throw new IllegalStateException("worker fault");
                  }
                });
            throw new AssertionError("worker fault lost");
          } catch (ExecutionException e) {
            BenchRuntime.require(e.getCause() instanceof IllegalStateException, "wrong failure");
          }
          BenchRuntime.require(
              joined.get() && System.nanoTime() - start < TimeUnit.SECONDS.toNanos(5),
              "worker not promptly joined");
        });
    check(
        "probe-failure-not-swallowed",
        () -> {
          AkkaBench.small = true;
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn();
            h.retire();
            reject(() -> new AkkaBench.Probes(h, false));
          }
        });
    check(
        "owner-cleanup-releases-blocked-actor",
        () -> {
          BenchRuntime.Handle h;
          try (BenchRuntime r = new BenchRuntime()) {
            h = r.spawn();
            h.gate();
          }
          BenchRuntime.require(h.retired.isDone() && h.postStop.isDone(), "owner cleanup leak");
        });
    check(
        "running-probe-failure-signals-owner",
        () -> {
          try (BenchRuntime r = new BenchRuntime()) {
            var h = r.spawn();
            var p = new AkkaBench.Probes(h, false);
            h.retire();
            try {
              failed(p.failure);
            } finally {
              reject(p::close);
            }
          }
        });
    check(
        "owner-cancels-unfinished-chunk-job",
        () -> {
          CompletableFuture<Long> job;
          BenchRuntime.Handle h;
          try (BenchRuntime r = new BenchRuntime()) {
            h = r.spawn();
            var gate = new BenchRuntime.Gate(new CompletableFuture<>(), new CountDownLatch(1));
            job = h.track(new CompletableFuture<>());
            h.ref.tell(new BenchRuntime.Chunk(10, 2, 7, job, gate));
            BenchRuntime.get(gate.entered());
          }
          failed(job);
          BenchRuntime.require(h.retired.isDone(), "chunk actor leaked");
        });
    System.out.println("CONTRACTS=" + passed);
  }
}
