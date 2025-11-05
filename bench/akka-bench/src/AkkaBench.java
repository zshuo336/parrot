import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/** Twenty workloads. Independent Rust answers are loaded before any timed work. */
public final class AkkaBench {
  record Key(long iterations, long salt) {}

  static boolean small;
  static final Map<Key, Long> expected = new HashMap<>();
  static final List<Row> rows = new ArrayList<>();

  static int count(int full, int check) {
    return small ? check : full;
  }

  static long iterations(long full) {
    return small ? Math.min(full, 50_000) : full;
  }

  static long answer(long n, long salt) {
    Long value = expected.get(new Key(n, salt));
    BenchRuntime.require(value != null, "missing independent answer: " + n + "/" + salt);
    return value;
  }

  static long burnCpu(long n, long salt) {
    return BenchRuntime.burn(n, salt);
  }

  static void equal(long got, long wanted) {
    BenchRuntime.require(
        got == wanted,
        "result mismatch: got "
            + Long.toUnsignedString(got)
            + " expected "
            + Long.toUnsignedString(wanted));
  }

  record Lat(long n, long p50, long p90, long p99, long max) {
    static Lat of(List<Long> values) {
      if (values.isEmpty()) return new Lat(0, 0, 0, 0, 0);
      List<Long> s = new ArrayList<>(values);
      Collections.sort(s);
      int n = s.size();
      BenchRuntime.require(s.get(0) >= 0, "negative latency");
      return new Lat(
          n,
          s.get(Math.toIntExact((n * 50L + 99) / 100) - 1),
          s.get(Math.toIntExact((n * 90L + 99) / 100) - 1),
          s.get(Math.toIntExact((n * 99L + 99) / 100) - 1),
          s.get(n - 1));
    }
  }

  record Row(String name, long messages, double wall, Lat lat, String note) {}

  static void row(
      String name, long messages, double wall, List<Long> samples, boolean correct, String note) {
    BenchRuntime.require(correct, "incorrect benchmark row: " + name);
    BenchRuntime.require(
        messages > 0 && Double.isFinite(wall) && wall > 0, "invalid measurement: " + name);
    Row r = new Row(name, messages, wall, Lat.of(samples), note);
    rows.add(r);
    System.out.printf(
        Locale.ROOT,
        "[akka] %s | msgs=%d | wall=%.6fs | tput=%.0f/s | p99=%.3fms | correct=true | %s%n",
        name,
        messages,
        wall,
        messages / wall,
        r.lat.p99 / 1000.0,
        note);
  }

  static double elapsed(long start) {
    return (System.nanoTime() - start) / 1e9;
  }

  static long micros(long start) {
    return (System.nanoTime() - start) / 1000;
  }

  static void snapshot(BenchRuntime.Handle h, long count, long sum) throws Exception {
    BenchRuntime.Snapshot s = h.snapshot();
    equal(s.count(), count);
    equal(s.checksum(), sum);
  }

  static long ask(BenchRuntime.Handle h, long n, long salt, long value) throws Exception {
    long result = BenchRuntime.get(h.ask(n, salt));
    equal(result, value);
    return result;
  }

  static Set<Key> keys() {
    Set<Key> keys = new HashSet<>();
    for (int i = 0; i < count(200, 8); i++) keys.add(new Key(iterations(200_000), i));
    for (int a = 0; a < count(64, 4); a++)
      for (int k = 0; k < count(25, 3); k++) keys.add(new Key(iterations(200_000), a * 1000L + k));
    keys.add(new Key(iterations(50_000_000), 1));
    keys.add(new Key(iterations(2_000_000_000L), 7));
    keys.add(new Key(iterations(1_000_000_000L), 7));
    for (int i = 0; i < 8; i++) keys.add(new Key(iterations(57_200_000_000L), 0xD00DL + i));
    for (int a = 0; a < 4; a++)
      for (int k = 0; k < count(10, 2); k++)
        keys.add(new Key(iterations(2_400_000_000L), 0xBACCL + a * 100 + k));
    keys.add(new Key(iterations(30_000_000_000L), 1));
    keys.add(new Key(iterations(30_000_000_000L), 3));
    for (int k = 0; k < 6; k++) keys.add(new Key(iterations(2_400_000_000L), 100 + k));
    for (int k = 0; k < count(4000, 12); k++) keys.add(new Key(1000, k));
    for (long k : new long[] {14, 5, 1, 2}) keys.add(new Key(1000, k));
    for (int a = 0; a < 8; a++)
      for (int k = 0; k < count(5000, 8); k++)
        keys.add(new Key(iterations(170_000), a * 10_000L + k));
    return keys;
  }

  static void load(Path path) throws Exception {
    for (String line : Files.readAllLines(path)) {
      String[] f = line.split(" ");
      BenchRuntime.require(f.length == 3, "malformed oracle output");
      Key key = new Key(Long.parseLong(f[0]), Long.parseUnsignedLong(f[1]));
      BenchRuntime.require(
          expected.put(key, Long.parseUnsignedLong(f[2])) == null, "duplicate oracle key");
    }
    BenchRuntime.require(expected.keySet().equals(keys()), "oracle key set mismatch");
  }

  static void echo(BenchRuntime rt, String name, int workers, int per) throws Exception {
    BenchRuntime.Handle h = rt.spawn();
    List<Long> lat = Collections.synchronizedList(new ArrayList<>());
    long sum = 0;
    for (int a = 0; a < workers; a++) for (int k = 0; k < per; k++) sum += a + k;
    long start = System.nanoTime();
    BenchRuntime.parallel(
        workers,
        a -> {
          for (int k = 0; k < per; k++) {
            long t = System.nanoTime();
            ask(h, -1, a + k, a + k);
            lat.add(micros(t));
          }
        });
    double wall = elapsed(start);
    snapshot(h, (long) workers * per, sum);
    h.retire();
    row(name, (long) workers * per, wall, lat, true, "concurrency=" + workers);
  }

  static void cpu(BenchRuntime rt, String name, int actors, int per, long n, long stride)
      throws Exception {
    List<BenchRuntime.Handle> hs = new ArrayList<>();
    long[] sums = new long[actors];
    for (int a = 0; a < actors; a++) {
      hs.add(rt.spawn());
      for (int k = 0; k < per; k++) sums[a] += answer(n, a * stride + k);
    }
    List<Long> lat = Collections.synchronizedList(new ArrayList<>());
    long start = System.nanoTime();
    BenchRuntime.parallel(
        actors,
        a -> {
          for (int k = 0; k < per; k++) {
            long t = System.nanoTime();
            ask(hs.get(a), n, a * stride + k, answer(n, a * stride + k));
            lat.add(micros(t));
          }
        });
    double wall = elapsed(start);
    for (int a = 0; a < actors; a++) {
      snapshot(hs.get(a), per, sums[a]);
      hs.get(a).retire();
    }
    row(name, (long) actors * per, wall, lat, true, actors + " actors; iterations/message=" + n);
  }

  static void tell(BenchRuntime rt, String name, int workers, int per, long n, long stride)
      throws Exception {
    BenchRuntime.Handle h = rt.spawn();
    long sum = 0;
    for (int a = 0; a < workers; a++)
      for (int k = 0; k < per; k++) sum += n < 0 ? a * stride + k : answer(n, a * stride + k);
    long start = System.nanoTime();
    BenchRuntime.parallel(
        workers,
        a -> {
          for (int k = 0; k < per; k++) h.tell(n, a * stride + k);
        });
    snapshot(h, (long) workers * per, sum);
    double wall = elapsed(start);
    h.retire();
    row(
        name,
        (long) workers * per,
        wall,
        List.of(),
        true,
        "all producers joined; exact drained count and modulo-u64 checksum");
  }

  static void timeout(BenchRuntime rt) throws Exception {
    BenchRuntime.Handle h = rt.spawn();
    BenchRuntime.Gate gate = h.gate();
    long n = iterations(50_000_000), want = answer(n, 1), start = System.nanoTime();
    CompletableFuture<Long> heavy = h.ask(n, 1), queued = h.ask(-1, 23);
    try {
      expectTimeout(queued, 1);
    } finally {
      gate.release().countDown();
    }
    equal(BenchRuntime.get(heavy), want);
    equal(BenchRuntime.get(queued), 23);
    snapshot(h, 2, want + 23);
    double wall = elapsed(start);
    h.retire();
    row(
        "ask-timeout-short",
        2,
        wall,
        List.of(),
        true,
        "queued behind gate + CPU; caller get(1ms) times out; work later completes exactly once (no"
            + " cancellation)");
  }

  static void expectTimeout(CompletableFuture<?> f, long ms) throws Exception {
    try {
      f.get(ms, TimeUnit.MILLISECONDS);
      throw new IllegalStateException("expected timeout");
    } catch (TimeoutException expectedTimeout) {
      /* only the intended timeout is accepted */
    }
  }

  static void afterStop(BenchRuntime rt) throws Exception {
    BenchRuntime.Handle h = rt.spawn();
    ask(h, -1, 1, 1);
    h.retire();
    CompletableFuture<Long> raw = new CompletableFuture<>();
    long start = System.nanoTime();
    h.ref.tell(new BenchRuntime.Work(-1, 42, raw));
    expectTimeout(raw, 50);
    row(
        "send-after-stop",
        1,
        elapsed(start),
        List.of(),
        true,
        "raw future sent after observed Terminated; no reply in 50ms; dead letters do not"
            + " acknowledge");
  }

  static void herd(BenchRuntime rt) throws Exception {
    int n = count(20000, 40);
    List<BenchRuntime.Handle> hs = new ArrayList<>();
    long start = System.nanoTime();
    for (int i = 0; i < n; i++) hs.add(rt.spawn());
    double spawn = elapsed(start);
    BenchRuntime.parallel(
        8,
        w -> {
          for (int i = w; i < n; i += 8) ask(hs.get(i), -1, i, i);
        });
    double wall = elapsed(start);
    for (int i = 0; i < n; i++) snapshot(hs.get(i), 1, i);
    rt.retireAll();
    row(
        "herd-20000-actors",
        n,
        wall,
        List.of(),
        true,
        "all "
            + n
            + " checked; spawn-only seconds="
            + spawn
            + "; wall includes echo acknowledgements");
  }

  static void longrun(BenchRuntime rt) throws Exception {
    BenchRuntime.Handle h = rt.spawn();
    long n = iterations(2_000_000_000L), want = answer(n, 7), start = System.nanoTime();
    ask(h, n, 7, want);
    double wall = elapsed(start);
    snapshot(h, 1, want);
    h.retire();
    row("longrun-2G-iters", 1, wall, List.of((long) (wall * 1e6)), true, "iterations=" + n);
  }

  record Sample(long begin, long end) {
    long micros() {
      return (end - begin) / 1000;
    }
  }

  static final class Probes implements AutoCloseable {
    final AtomicBoolean stop = new AtomicBoolean();
    final List<Sample> samples = new ArrayList<>();
    final ExecutorService executor = Executors.newSingleThreadExecutor();
    final Future<?> worker;
    final CompletableFuture<Void> ready = new CompletableFuture<>();
    final CompletableFuture<Void> failure = new CompletableFuture<>();

    Probes(BenchRuntime.Handle h, boolean tiny) throws Exception {
      long want = tiny ? answer(1000, 14) : 14;
      worker =
          executor.submit(
              () -> {
                try {
                  do {
                    BenchRuntime.require(samples.size() < 1_000_000, "probe sample limit");
                    long start = System.nanoTime();
                    ask(h, tiny ? 1000 : -1, 14, want);
                    samples.add(new Sample(start, System.nanoTime()));
                    ready.complete(null);
                    Thread.sleep(5);
                  } while (!stop.get());
                } catch (Exception e) {
                  ready.completeExceptionally(e);
                  failure.completeExceptionally(e);
                  throw new CompletionException(e);
                }
              });
      try {
        BenchRuntime.get(ready);
      } catch (Exception e) {
        close();
        throw e;
      }
    }

    void check() throws Exception {
      if (worker.isDone()) worker.get();
    }

    @Override
    public void close() {
      stop.set(true);
      try {
        worker.get(10, TimeUnit.SECONDS);
      } catch (Exception e) {
        throw new IllegalStateException("probe failed", e);
      } finally {
        worker.cancel(true);
        executor.shutdownNow();
        try {
          BenchRuntime.require(
              executor.awaitTermination(10, TimeUnit.SECONDS), "probe did not join");
        } catch (InterruptedException e) {
          Thread.currentThread().interrupt();
          throw new IllegalStateException(e);
        }
      }
    }
  }

  static void monitored(BenchRuntime rt, boolean mixed) throws Exception {
    List<BenchRuntime.Handle> hs = new ArrayList<>();
    List<Key> tasks = new ArrayList<>();
    if (mixed) {
      for (int a = 0; a < 8; a++) {
        hs.add(rt.spawn());
        tasks.add(new Key(iterations(57_200_000_000L), 0xD00DL + a));
      }
      for (int a = 0; a < 4; a++) {
        BenchRuntime.Handle h = rt.spawn();
        for (int k = 0; k < count(10, 2); k++) {
          hs.add(h);
          tasks.add(new Key(iterations(2_400_000_000L), 0xBACCL + a * 100 + k));
        }
      }
    } else {
      hs.add(rt.spawn());
      tasks.add(new Key(iterations(1_000_000_000L), 7));
    }
    BenchRuntime.Handle probe = rt.spawn();
    Probes p = new Probes(probe, mixed);
    long start = System.nanoTime();
    try (p) {
      List<CompletableFuture<Long>> futures = new ArrayList<>();
      for (int i = 0; i < tasks.size(); i++) {
        Key k = tasks.get(i);
        futures.add(hs.get(i).ask(k.iterations, k.salt));
      }
      CompletableFuture<Void> completed = new CompletableFuture<>();
      AtomicInteger remaining = new AtomicInteger(tasks.size());
      p.failure.whenComplete(
          (v, error) -> {
            if (error != null) completed.completeExceptionally(error);
          });
      for (int i = 0; i < tasks.size(); i++) {
        Key k = tasks.get(i);
        futures
            .get(i)
            .thenAccept(v -> equal(v, answer(k.iterations, k.salt)))
            .whenComplete(
                (v, error) -> {
                  if (error != null) completed.completeExceptionally(error);
                  else if (remaining.decrementAndGet() == 0) completed.complete(null);
                });
      }
      BenchRuntime.get(completed);
      p.check();
    }
    long end = System.nanoTime();
    long computeStart = hs.stream().mapToLong(h -> h.firstCpuStart.get()).min().orElseThrow();
    long computeEnd = hs.stream().mapToLong(h -> h.lastCpuEnd.get()).max().orElseThrow();
    long timedProbes = p.samples.stream().filter(s -> s.begin >= start && s.end <= end).count();
    List<Long> overlaps =
        p.samples.stream()
            .filter(s -> s.begin < computeEnd && s.end > computeStart)
            .map(Sample::micros)
            .toList();
    Map<BenchRuntime.Handle, long[]> totals = new IdentityHashMap<>();
    for (int i = 0; i < tasks.size(); i++) {
      Key k = tasks.get(i);
      long[] t = totals.computeIfAbsent(hs.get(i), h -> new long[2]);
      t[0]++;
      t[1] += answer(k.iterations, k.salt);
    }
    for (var e : totals.entrySet()) snapshot(e.getKey(), e.getValue()[0], e.getValue()[1]);
    snapshot(probe, p.samples.size(), p.samples.size() * (mixed ? answer(1000, 14) : 14));
    rt.retireAll();
    row(
        mixed ? "mixed-minute-cpu-plus-incoming" : "starve-echo-during-longrun",
        tasks.size() + timedProbes,
        (end - start) / 1e9,
        overlaps,
        true,
        "CPU jobs="
            + tasks.size()
            + "; timed probes="
            + timedProbes
            + "; all probes="
            + p.samples.size()
            + "; actual CPU interval overlap probes="
            + overlaps.size()
            + "; zero overlap means no latency evidence; joined before reading samples");
  }

  static void io(BenchRuntime rt) throws Exception {
    int actors = count(64, 4), per = count(20, 4);
    List<BenchRuntime.Handle> hs = new ArrayList<>();
    for (int a = 0; a < actors; a++) hs.add(rt.spawn(10));
    List<CompletableFuture<Long>> fs = new ArrayList<>();
    long start = System.nanoTime();
    for (int a = 0; a < actors; a++)
      for (int k = 0; k < per; k++) fs.add(hs.get(a).ask(-1, a * 100L + k));
    BenchRuntime.parallel(
        actors,
        a -> {
          for (int k = 0; k < per; k++) equal(BenchRuntime.get(fs.get(a * per + k)), a * 100L + k);
        });
    double wall = elapsed(start);
    for (int a = 0; a < actors; a++)
      snapshot(hs.get(a), per, per * a * 100L + (long) per * (per - 1) / 2);
    rt.retireAll();
    row(
        "io-async-64actors-10ms",
        (long) actors * per,
        wall,
        List.of(),
        true,
        "10ms asynchronous delay; serialized per actor; every payload validated");
  }

  static void fifo(BenchRuntime rt) throws Exception {
    BenchRuntime.Handle h = rt.spawn();
    long n = iterations(30_000_000_000L), med = iterations(2_400_000_000L);
    int tiny = count(4000, 12);
    long sum = answer(n, 1) + answer(1000, 5);
    for (int k = 0; k < 6; k++) sum += answer(med, 100 + k);
    for (int k = 0; k < tiny; k++) sum += answer(1000, k);
    BenchRuntime.Gate g = h.gate();
    long start = System.nanoTime();
    h.tell(n, 1);
    for (int k = 0; k < 6; k++) h.tell(med, 100 + k);
    for (int k = 0; k < tiny; k++) h.tell(1000, k);
    CompletableFuture<Long> tail = h.ask(1000, 5);
    try {
      BenchRuntime.require(!tail.isDone(), "tail bypassed queue");
    } finally {
      g.release().countDown();
    }
    equal(BenchRuntime.get(tail), answer(1000, 5));
    double wall = elapsed(start);
    long latency = micros(start);
    snapshot(h, 8L + tiny, sum);
    h.retire();
    row(
        "mixed-same-actor-fifo",
        8L + tiny,
        wall,
        List.of(latency),
        true,
        "gate proves tail enqueued; exact completed count/checksum includes tail probe");
  }

  static void chunked(BenchRuntime rt) throws Exception {
    long total = iterations(30_000_000_000L),
        size = small ? total / 20 : 1_500_000_000L,
        want = answer(total, 3);
    BenchRuntime.Handle solid = rt.spawn();
    BenchRuntime.Gate gate = solid.gate();
    long start = System.nanoTime();
    CompletableFuture<Long> job = solid.ask(total, 3), tail = solid.ask(1000, 1);
    gate.release().countDown();
    equal(BenchRuntime.get(tail), answer(1000, 1));
    equal(BenchRuntime.get(job), want);
    double solidWall = elapsed(start);
    long solidUs = micros(start);
    snapshot(solid, 2, want + answer(1000, 1));
    solid.retire();
    BenchRuntime.Handle ch = rt.spawn();
    CompletableFuture<Long> done = ch.track(new CompletableFuture<>());
    BenchRuntime.Gate afterFirst =
        new BenchRuntime.Gate(new CompletableFuture<>(), new CountDownLatch(1));
    long chunkStart = System.nanoTime();
    ch.ref.tell(new BenchRuntime.Chunk(total, size, 3, done, afterFirst));
    BenchRuntime.get(afterFirst.entered());
    long probeStart = System.nanoTime();
    CompletableFuture<Long> probe = ch.ask(1000, 2);
    // Callback runs on completion inside the actor, before it can process a subsequent
    // continuation.
    CompletableFuture<Boolean> interleaved = probe.thenApply(v -> !done.isDone());
    afterFirst.release().countDown();
    equal(BenchRuntime.get(probe), answer(1000, 2));
    BenchRuntime.require(BenchRuntime.get(interleaved), "probe waited for whole chunk job");
    long chunkUs = micros(probeStart);
    equal(BenchRuntime.get(done), want);
    double chunkWall = elapsed(chunkStart);
    snapshot(ch, 2, want + answer(1000, 2));
    ch.retire();
    row(
        "chunked-vs-solid-longrun",
        4,
        solidWall + chunkWall,
        List.of(solidUs, chunkUs),
        true,
        "same continuous recurrence and global index; 20 chunks; chunk seconds="
            + chunkWall
            + "; probe admitted after first chunk, completed before final chunk; includes"
            + " deterministic gate overhead");
  }

  static void pingpong(BenchRuntime rt) throws Exception {
    BenchRuntime.Handle a = rt.spawn(), b = rt.spawn();
    int n = count(10000, 20);
    List<Long> samples = new ArrayList<>();
    long start = System.nanoTime();
    for (int i = 0; i < n; i++) {
      long t = System.nanoTime();
      long v = ask(a, -1, i, i);
      ask(b, -1, v, i);
      if (i % 10 == 0) samples.add(micros(t));
    }
    double wall = elapsed(start);
    long sum = (long) n * (n - 1) / 2;
    snapshot(a, n, sum);
    snapshot(b, n, sum);
    rt.retireAll();
    row(
        "pingpong-rtt-10k",
        2L * n,
        wall,
        samples,
        true,
        "two sequential client asks; not actor-to-actor pingpong; every tenth pair sampled");
  }

  static void chain(BenchRuntime rt) throws Exception {
    BenchRuntime.Handle h = rt.spawn();
    int n = count(20000, 24);
    long start = System.nanoTime();
    for (int i = 0; i < n; i++) {
      ask(h, -1, i, i);
      h.tell(-1, i + 1);
    }
    snapshot(h, 2L * n, (long) n * n);
    double wall = elapsed(start);
    h.retire();
    row(
        "self-chain-ask-tell-20k",
        2L * n,
        wall,
        List.of(),
        true,
        "client ask then tell; exact drain; not self-messaging chain");
  }

  static void storm(BenchRuntime rt) throws Exception {
    int per = count(1000, 8);
    long start = System.nanoTime();
    int before = rt.retired.get();
    for (int w = 0; w < 5; w++) {
      List<BenchRuntime.Handle> hs = new ArrayList<>();
      for (int i = 0; i < per; i++) hs.add(rt.spawn());
      BenchRuntime.parallel(
          8,
          a -> {
            for (int i = a; i < per; i += 8) ask(hs.get(i), -1, i, i);
          });
      for (int i = 0; i < per; i++) snapshot(hs.get(i), 1, i);
      rt.retireAll();
    }
    equal(rt.retired.get() - before, 5L * per);
    row(
        "spawn-stop-storm-5k",
        10L * per,
        elapsed(start),
        List.of(),
        true,
        "all " + 5 * per + " actors echoed and observed Terminated; units echo+stop");
  }

  static void run() throws Exception {
    try (BenchRuntime rt = new BenchRuntime()) {
      BenchRuntime.Handle warm = rt.spawn();
      for (int i = 0; i < count(3000, 30); i++) ask(warm, -1, i, i);
      rt.retireAll();
      int before = rt.retired.get();
      echo(rt, "seq-ask-echo-1k", 1, count(1000, 16));
      rt.retireAll();
      echo(rt, "conc-ask-echo-c8-m1000", 8, count(1000, 8));
      rt.retireAll();
      echo(rt, "conc-ask-echo-c64-m200", count(64, 8), count(200, 6));
      rt.retireAll();
      tell(rt, "tell-echo-100k", 1, count(100000, 100), -1, 0);
      rt.retireAll();
      cpu(rt, "cpu-serial-200x200k", 1, count(200, 8), iterations(200_000), 0);
      rt.retireAll();
      cpu(rt, "cpu-parallel-64actors-200k", count(64, 4), count(25, 3), iterations(200_000), 1000);
      rt.retireAll();
      tell(rt, "flood-500k-tell", 4, count(125000, 25), -1, 1);
      rt.retireAll();
      timeout(rt);
      rt.retireAll();
      afterStop(rt);
      rt.retireAll();
      herd(rt);
      longrun(rt);
      rt.retireAll();
      monitored(rt, false);
      io(rt);
      monitored(rt, true);
      fifo(rt);
      rt.retireAll();
      chunked(rt);
      rt.retireAll();
      pingpong(rt);
      chain(rt);
      rt.retireAll();
      tell(rt, "slow-consumer-8prod-40k", 8, count(5000, 8), iterations(170_000), 10000);
      rt.retireAll();
      storm(rt);
      equal(rows.size(), 20);
      equal(rt.live.get(), 0);
      System.out.println(
          "SCENARIOS=20 RETIRED="
              + (rt.retired.get() - before)
              + " LIVE=0 MODE="
              + (small ? "check" : "full"));
    }
  }

  public static void main(String[] args) throws Exception {
    BenchRuntime.require(args.length == 3, "usage: AkkaBench keys|run check|full PATH");
    BenchRuntime.require(args[1].equals("check") || args[1].equals("full"), "unknown mode");
    small = args[1].equals("check");
    if (args[0].equals("keys")) {
      List<Key> keys = new ArrayList<>(keys());
      keys.sort(Comparator.comparingLong(Key::iterations).thenComparingLong(Key::salt));
      List<String> lines =
          keys.stream().map(k -> k.iterations + " " + Long.toUnsignedString(k.salt)).toList();
      Files.write(Path.of(args[2]), lines);
    } else if (args[0].equals("run")) {
      load(Path.of(args[2]));
      run();
    } else throw new IllegalArgumentException("unknown operation");
  }
}
