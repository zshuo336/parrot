import akka.actor.typed.*;
import akka.actor.typed.javadsl.*;

import java.time.Duration;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.function.Function;

/**
 * Parrot 双引擎基准的 Akka Typed 对等实现（javadsl）。
 *
 * 对等原则：
 *  - 消息语义/数量/每条 CPU 迭代数与 Rust 版 engine_stress_* 完全一致
 *  - burn_cpu 用同一 LCG 常数（wrapping 语义）
 *  - p50/p90/p99/max 分位数算法一致（ceil 索引）
 *  - JVM warmup 3 轮 echo ask 后才正式计时
 *
 * 运行：见 run.sh（-XX:+UseZGC -Xms1g -Xmx1g）
 */
public final class AkkaBench {

    // ======================= 负载内核（与 Rust 对等） =======================

    /** 与 Rust burn_cpu 相同的 LCG 混合，防止 JIT 折叠。 */
    static long burnCpu(long iterations, long salt) {
        long x = salt | 1L;
        for (long i = 0; i < iterations; i++) {
            x = x * 6364136223846793005L + 1442695040888963407L;
            x = x ^ i;
            if (x == 42L) return x; // 概率≈0，防分支消除
        }
        return x;
    }

    // ======================= 消息（sealed 手写） =======================

    interface Msg {}
    static final class Echo implements Msg { final long v; Echo(long v){this.v=v;} }
    static final class CpuTask implements Msg { final long iters, salt; CpuTask(long i, long s){iters=i;salt=s;} }
    static final class TinyTask implements Msg { final long salt; TinyTask(long s){salt=s;} }
    static final class MediumCpu implements Msg { final long iters, salt; MediumCpu(long i, long s){iters=i;salt=s;} }
    static final class MinuteCpu implements Msg { final long iters, salt; MinuteCpu(long i, long s){iters=i;salt=s;} }
    static final class LongRun implements Msg { final long iters; LongRun(long i){iters=i;} }
    static final class TellDone implements Msg { final long n; TellDone(long n){this.n=n;} }
    static final class TickProbe implements Msg {}
    static final class AskEcho implements Msg {
        final long v; final CompletableFuture<Long> reply;
        AskEcho(long v, CompletableFuture<Long> r){this.v=v;reply=r;}
    }
    static final class AskCpu implements Msg {
        final long iters, salt; final CompletableFuture<Long> reply;
        AskCpu(long i, long s, CompletableFuture<Long> r){iters=i;salt=s;reply=r;}
    }
    static final class AskTiny implements Msg {
        final long salt; final CompletableFuture<Long> reply;
        AskTiny(long s, CompletableFuture<Long> r){salt=s;reply=r;}
    }
    /** 通用 ask 信封：对任意消息执行逻辑并回填 future。 */
    static final class AskEnvelope implements Msg {
        final Msg inner; final CompletableFuture<Long> reply;
        AskEnvelope(Msg inner, CompletableFuture<Long> reply){this.inner=inner;this.reply=reply;}
    }
    /** 外部停止信号（等价 Rust stop_actor）。 */
    enum PoisonPill implements Msg { INSTANCE }

    // ======================= BenchActor（单实现覆盖全部消息） =======================

    static Behavior<Msg> bench(AtomicLong ops, AtomicLong cpuSink) {
        return Behaviors.setup(ctx -> new AbstractBehavior<Msg>(ctx) {
            @Override public Receive<Msg> createReceive() {
                return newReceiveBuilder()
                    .onMessage(AskEcho.class, m -> { ops.incrementAndGet(); m.reply.complete(m.v); return this; })
                    .onMessage(AskCpu.class, m -> {
                        ops.incrementAndGet();
                        long r = burnCpu(m.iters, m.salt);
                        cpuSink.addAndGet(r);
                        ops.incrementAndGet();
                        m.reply.complete(r);
                        return this;
                    })
                    .onMessage(AskTiny.class, m -> {
                        long r = burnCpu(1_000, m.salt);
                        cpuSink.addAndGet(r);
                        m.reply.complete(r);
                        return this;
                    })
                    .onMessage(AskEnvelope.class, m -> {
                        long r = handleInner(m.inner, ops, cpuSink);
                        m.reply.complete(r);
                        return this;
                    })
                    // tell 路径
                    .onMessage(Echo.class, m -> { ops.incrementAndGet(); return this; })
                    .onMessage(CpuTask.class, m -> {
                        ops.incrementAndGet();
                        cpuSink.addAndGet(burnCpu(m.iters, m.salt));
                        ops.incrementAndGet();
                        return this;
                    })
                    .onMessage(TinyTask.class, m -> { cpuSink.addAndGet(burnCpu(1_000, m.salt)); return this; })
                    .onMessage(MediumCpu.class, m -> {
                        ops.incrementAndGet();
                        cpuSink.addAndGet(burnCpu(m.iters, m.salt));
                        ops.incrementAndGet();
                        return this;
                    })
                    .onMessage(MinuteCpu.class, m -> {
                        ops.incrementAndGet();
                        cpuSink.addAndGet(burnCpu(m.iters, m.salt));
                        ops.incrementAndGet();
                        return this;
                    })
                    .onMessage(LongRun.class, m -> {
                        ops.incrementAndGet();
                        cpuSink.addAndGet(burnCpu(m.iters, 7));
                        ops.incrementAndGet();
                        return this;
                    })
                    .onMessage(TellDone.class, m -> { ops.addAndGet(m.n); return this; })
                    .onMessage(TickProbe.class, m -> this)
                    .onMessage(PoisonPill.class, m -> Behaviors.stopped())
                    .build();
            }
        });
    }

    static long handleInner(Msg m, AtomicLong ops, AtomicLong cpuSink) {
        if (m instanceof Echo e) { ops.incrementAndGet(); return e.v; }
        if (m instanceof CpuTask c) { ops.incrementAndGet(); long r=burnCpu(c.iters,c.salt); cpuSink.addAndGet(r); ops.incrementAndGet(); return r; }
        if (m instanceof TinyTask t) { long r=burnCpu(1_000,t.salt); cpuSink.addAndGet(r); return r; }
        if (m instanceof MediumCpu c) { ops.incrementAndGet(); long r=burnCpu(c.iters,c.salt); cpuSink.addAndGet(r); ops.incrementAndGet(); return r; }
        if (m instanceof MinuteCpu c) { ops.incrementAndGet(); long r=burnCpu(c.iters,c.salt); cpuSink.addAndGet(r); ops.incrementAndGet(); return r; }
        if (m instanceof LongRun l) { ops.incrementAndGet(); long r=burnCpu(l.iters,7); cpuSink.addAndGet(r); ops.incrementAndGet(); return r; }
        throw new IllegalStateException("unknown");
    }

    // ======================= 统计（与 Rust latency_stats 对等） =======================

    static final class Lat {
        final long n, p50, p90, p99, max; final double mean;
        Lat(long n, long p50, long p90, long p99, long max, double mean){this.n=n;this.p50=p50;this.p90=p90;this.p99=p99;this.max=max;this.mean=mean;}
        static Lat of(List<Long> micros) {
            if (micros.isEmpty()) return new Lat(0,0,0,0,0,0);
            List<Long> s = new ArrayList<>(micros); Collections.sort(s);
            int n = s.size();
            Function<Double,Integer> pick = q -> (int)Math.min(n-1, Math.max(0, Math.ceil(q/100.0*n)-1));
            double sum=0; for(long v:s) sum+=v;
            return new Lat(n, s.get(pick.apply(50.0)), s.get(pick.apply(90.0)), s.get(pick.apply(99.0)), s.get(n-1), sum/n);
        }
    }

    static final class Row {
        String name; long msgs; double wall; Lat lat; boolean correct=true; String note="";
        Row(String n,long m,double w,Lat l){name=n;msgs=m;wall=w;lat=l;}
        void print() {
            System.out.printf(java.util.Locale.ROOT,
                "[akka] %s | msgs=%d | wall=%.3fs | tput=%.0f/s | lat p50=%.1fms p90=%.1fms p99=%.1fms max=%.1fms | correct=%b%n",
                name, msgs, wall, msgs/Math.max(wall,1e-9), lat.p50/1000.0, lat.p90/1000.0, lat.p99/1000.0, lat.max/1000.0, correct);
            if (!note.isEmpty()) System.out.println("        note: " + note);
        }
    }

    static final List<Row> ROWS = new CopyOnWriteArrayList<>();

    static void row(String name, long msgs, double wall, List<Long> latMicros, boolean correct, String note) {
        Row r = new Row(name, msgs, wall, Lat.of(latMicros));
        r.correct = correct; r.note = note; r.print(); ROWS.add(r);
    }

    // ======================= 驱动（异步 pipeline ask） =======================

    static CompletableFuture<Long> askEcho(ActorRef<Msg> ref, long v) {
        CompletableFuture<Long> f = new CompletableFuture<>();
        ref.tell(new AskEcho(v, f));
        return f;
    }
    static CompletableFuture<Long> askCpu(ActorRef<Msg> ref, long iters, long salt) {
        CompletableFuture<Long> f = new CompletableFuture<>();
        ref.tell(new AskCpu(iters, salt, f));
        return f;
    }
    static CompletableFuture<Long> askTiny(ActorRef<Msg> ref, long salt) {
        CompletableFuture<Long> f = new CompletableFuture<>();
        ref.tell(new AskTiny(salt, f));
        return f;
    }
    static CompletableFuture<Long> askEnvelope(ActorRef<Msg> ref, Msg inner) {
        CompletableFuture<Long> f = new CompletableFuture<>();
        ref.tell(new AskEnvelope(inner, f));
        return f;
    }

    static boolean await(CompletableFuture<?> f, long sec) {
        try { f.get(sec, TimeUnit.SECONDS); return true; }
        catch (Exception e) { return false; }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("==================== AKKA TYPED BENCH (javadsl) ====================");
        int cores = Runtime.getRuntime().availableProcessors();
        System.out.println("cores=" + cores + " akka.parallelism=" + cores);

        // benchActorSystem：根 guardian 即 BenchActor 的 system（无路由直接使用）
        AtomicLong ops0 = new AtomicLong(), sink0 = new AtomicLong();
        ActorSystem<Msg> sys = ActorSystem.create(bench(ops0, sink0), "bench");

        // ---------- warmup（不计时） ----------
        for (int i = 0; i < 3_000; i++) askEcho(sys, i).get(10, TimeUnit.SECONDS);
        System.out.println("warmup done");

        // ---------- 1. seq-ask-echo-1k ----------
        {
            ActorRef<Msg> a = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "seq",
                DispatcherSelector.defaultDispatcher());
            List<Long> lat = new ArrayList<>();
            long t0 = System.nanoTime();
            for (int i = 0; i < 1000; i++) {
                long s = System.nanoTime();
                long v = askEcho(a, i).get(10, TimeUnit.SECONDS);
                if (v != i) throw new IllegalStateException();
                lat.add((System.nanoTime()-s)/1000);
            }
            double wall = (System.nanoTime()-t0)/1e9;
            row("seq-ask-echo-1k", 1000, wall, lat, true, "");
        }

        // ---------- 2. conc-ask-echo-c8-m1000 ----------
        {
            ActorRef<Msg> a = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "c8",
                DispatcherSelector.defaultDispatcher());
            List<Long> samples = Collections.synchronizedList(new ArrayList<>());
            long t0 = System.nanoTime();
            ExecutorService pool = Executors.newFixedThreadPool(8);
            List<Future<?>> fs = new ArrayList<>();
            for (int c = 0; c < 8; c++) {
                final int cc = c;
                fs.add(pool.submit(() -> {
                    for (int i = 0; i < 1000; i++) {
                        long s = System.nanoTime();
                        try {
                            long v = askEcho(a, i + cc).get(10, TimeUnit.SECONDS);
                            if (v != i + cc) throw new IllegalStateException();
                        } catch (Exception e) { throw new RuntimeException(e); }
                        samples.add((System.nanoTime()-s)/1000);
                    }
                }));
            }
            for (Future<?> f : fs) f.get(60, TimeUnit.SECONDS);
            pool.shutdown();
            double wall = (System.nanoTime()-t0)/1e9;
            row("conc-ask-echo-c8-m1000", 8000, wall, samples, true, "concurrency=8");
        }

        // ---------- 3. conc-ask-echo-c64-m200 ----------
        {
            ActorRef<Msg> a = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "c64",
                DispatcherSelector.defaultDispatcher());
            List<Long> samples = Collections.synchronizedList(new ArrayList<>());
            long t0 = System.nanoTime();
            ExecutorService pool = Executors.newFixedThreadPool(64);
            List<Future<?>> fs = new ArrayList<>();
            for (int c = 0; c < 64; c++) {
                final int cc = c;
                fs.add(pool.submit(() -> {
                    for (int i = 0; i < 200; i++) {
                        long s = System.nanoTime();
                        try { askEcho(a, i + cc).get(10, TimeUnit.SECONDS); }
                        catch (Exception e) { throw new RuntimeException(e); }
                        samples.add((System.nanoTime()-s)/1000);
                    }
                }));
            }
            for (Future<?> f : fs) f.get(120, TimeUnit.SECONDS);
            pool.shutdown();
            double wall = (System.nanoTime()-t0)/1e9;
            row("conc-ask-echo-c64-m200", 12800, wall, samples, true, "concurrency=64");
        }

        // ---------- 4. tell-echo-100k ----------
        {
            AtomicLong ops = new AtomicLong();
            ActorRef<Msg> a = sys.systemActorOf(bench(ops, new AtomicLong()), "tell",
                DispatcherSelector.defaultDispatcher());
            long t0 = System.nanoTime();
            for (int i = 0; i < 100_000; i++) a.tell(new Echo(i));
            boolean ok = waitUntil(() -> ops.get() >= 100_000, 120);
            double wall = (System.nanoTime()-t0)/1e9;
            row("tell-echo-100k", 100_000, wall, List.of(), ok, ok?"":"DRAIN TIMEOUT");
        }

        // ---------- 5. cpu-serial-200x200k ----------
        {
            ActorRef<Msg> a = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "cpuser",
                DispatcherSelector.defaultDispatcher());
            List<Long> lat = new ArrayList<>();
            long t0 = System.nanoTime();
            for (int i = 0; i < 200; i++) {
                long s = System.nanoTime();
                long v = askCpu(a, 200_000, i).get(30, TimeUnit.SECONDS);
                if (v == 0 && v == 1) throw new IllegalStateException();
                lat.add((System.nanoTime()-s)/1000);
            }
            double wall = (System.nanoTime()-t0)/1e9;
            row("cpu-serial-200x200k", 200, wall, lat, true, "~200µs work/msg");
        }

        // ---------- 6. cpu-parallel（64 actor × 25 × 200k） ----------
        {
            List<ActorRef<Msg>> refs = new ArrayList<>();
            for (int i = 0; i < 64; i++)
                refs.add(sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "par"+i,
                    DispatcherSelector.defaultDispatcher()));
            List<Long> samples = Collections.synchronizedList(new ArrayList<>());
            long t0 = System.nanoTime();
            ExecutorService pool = Executors.newFixedThreadPool(64);
            List<Future<?>> fs = new ArrayList<>();
            for (int i = 0; i < 64; i++) {
                final int ii = i; final ActorRef<Msg> r = refs.get(i);
                fs.add(pool.submit(() -> {
                    for (int k = 0; k < 25; k++) {
                        long s = System.nanoTime();
                        try { askCpu(r, 200_000, ii*1000L+k).get(30, TimeUnit.SECONDS); }
                        catch (Exception e) { throw new RuntimeException(e); }
                        samples.add((System.nanoTime()-s)/1000);
                    }
                }));
            }
            for (Future<?> f : fs) f.get(120, TimeUnit.SECONDS);
            pool.shutdown();
            double wall = (System.nanoTime()-t0)/1e9;
            row("cpu-parallel-8actors-200k", 64*25, wall, samples, true, "64 actors × 25 msgs × 200k iters");
        }

        // ---------- 7. flood-500k-tell（4 生产者线程） ----------
        {
            AtomicLong ops = new AtomicLong();
            ActorRef<Msg> a = sys.systemActorOf(bench(ops, new AtomicLong()), "flood",
                DispatcherSelector.defaultDispatcher());
            long t0 = System.nanoTime();
            ExecutorService pool = Executors.newFixedThreadPool(4);
            List<Future<?>> fs = new ArrayList<>();
            final long n = 500_000, per = n/4;
            for (int p = 0; p < 4; p++) {
                final int pp = p;
                fs.add(pool.submit(() -> { for (long i = 0; i < per; i++) a.tell(new Echo(i+pp)); }));
            }
            for (Future<?> f : fs) f.get(60, TimeUnit.SECONDS);
            pool.shutdown();
            double sent = (System.nanoTime()-t0)/1e9;
            boolean ok = waitUntil(() -> ops.get() >= n, 300);
            double wall = (System.nanoTime()-t0)/1e9;
            row("flood-500k-tell", n, wall, List.of(), ok,
                String.format("sent in %.3fs; %s", sent, ok?"drained":"TIMEOUT"));
        }

        // ---------- 8. ask-timeout-short（ask 1ms 超时排队语义） ----------
        {
            AtomicLong opsT = new AtomicLong();
            ActorRef<Msg> a = sys.systemActorOf(bench(opsT, new AtomicLong()), "timeout",
                DispatcherSelector.defaultDispatcher());
            CompletableFuture<Long> heavy = askCpu(a, 50_000_000, 1);
            if (!waitUntil(() -> opsT.get() >= 1, 10)) throw new IllegalStateException("heavy never started");
            long s = System.nanoTime();
            boolean timedOut;
            try { askEcho(a, 1).get(1, TimeUnit.MILLISECONDS); timedOut = false; }
            catch (TimeoutException te) { timedOut = true; }
            double lat = (System.nanoTime()-s)/1e6;
            row("ask-timeout-short", 2, 0, List.of(), timedOut,
                String.format("1ms-timeout ask %s; heavy=%.1fms", timedOut?"timed out":"succeeded", lat));
            heavy.get(30, TimeUnit.SECONDS);
        }

        // ---------- 9. send-after-stop ----------
        {
            ActorRef<Msg> a = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "stoppable",
                DispatcherSelector.defaultDispatcher());
            askEcho(a, 1).get(10, TimeUnit.SECONDS); // 确认存活
            a.tell(PoisonPill.INSTANCE);
            Thread.sleep(50);
            boolean dead = false;
            try { askEcho(a, 1).get(300, TimeUnit.MILLISECONDS); }
            catch (TimeoutException te) { dead = true; }
            row("send-after-stop", 1, 0, List.of(), dead,
                dead ? "ask after stop never completes (dead letters)" : "ask after stop UNEXPECTEDLY replied");
        }

        // ---------- 10. herd-20000-actors ----------
        {
            AtomicLong ops = new AtomicLong();
            List<ActorRef<Msg>> refs = new ArrayList<>(20_000);
            long t0 = System.nanoTime();
            for (int i = 0; i < 20_000; i++)
                refs.add(sys.systemActorOf(bench(ops, new AtomicLong()), "h"+i,
                    DispatcherSelector.defaultDispatcher()));
            double spawnWall = (System.nanoTime()-t0)/1e9;
            long ok = refs.stream().parallel().filter(r -> {
                try { return askEcho(r, 1).get(10, TimeUnit.SECONDS) == 1; } catch (Exception e) { return false; }
            }).count();
            row("herd-20000-actors", 20_000, spawnWall, List.of(), ok == 20_000,
                String.format("spawned 20000 in %.3fs; all accepted send", spawnWall));
        }

        // ---------- 11. longrun-2G-iters ----------
        {
            ActorRef<Msg> a = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "longrun",
                DispatcherSelector.defaultDispatcher());
            long t0 = System.nanoTime();
            long v = askEnvelope(a, new LongRun(2_000_000_000L)).get(120, TimeUnit.SECONDS);
            double wall = (System.nanoTime()-t0)/1e9;
            row("longrun-2G-iters", 1, wall, List.of((long)(wall*1e6)), v != 0, "single 2G-iteration compute");
        }

        // ---------- 12. starve-echo-during-longrun ----------
        {
            ActorRef<Msg> aa = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "sa",
                DispatcherSelector.defaultDispatcher());
            ActorRef<Msg> ab = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "sb",
                DispatcherSelector.defaultDispatcher());
            AtomicBoolean stop = new AtomicBoolean(false);
            List<long[]> samples = Collections.synchronizedList(new ArrayList<>()); // [tMs, latUs]
            long origin = System.nanoTime();
            Thread ticker = new Thread(() -> {
                long i = 0;
                while (!stop.get()) {
                    long s = System.nanoTime();
                    try { if (askEcho(ab, i).get(10, TimeUnit.SECONDS) == i)
                        samples.add(new long[]{(System.nanoTime()-origin)/1_000_000, (System.nanoTime()-s)/1000}); }
                    catch (Exception ignored) {}
                    i++;
                    try { Thread.sleep(5); } catch (InterruptedException ie) { return; }
                }
            });
            ticker.start();
            Thread.sleep(150);
            int baseN = samples.size();
            List<Long> baseLat = new ArrayList<>();
            for (int k = 0; k < baseN; k++) baseLat.add(samples.get(k)[1]);
            Lat base = Lat.of(baseLat);

            CompletableFuture<Long> heavy = askEnvelope(aa, new LongRun(1_000_000_000L));
            long computeT0 = System.nanoTime();
            Thread.sleep(1500);
            long computeT1 = System.nanoTime();
            long t0Ms = (origin==0?0:(computeT0-origin)/1_000_000), t1Ms = (computeT1-origin)/1_000_000;
            List<Long> inWin = new ArrayList<>();
            for (long[] s : samples)
                if (s[0] >= t0Ms && s[0] <= t1Ms) inWin.add(s[1]);
            heavy.get(120, TimeUnit.SECONDS);
            stop.set(true); ticker.join();
            List<Long> during = new ArrayList<>();
            for (int k = baseN; k < samples.size(); k++) during.add(samples.get(k)[1]);
            Lat dur = Lat.of(during);
            Lat win = Lat.of(inWin);
            row("starve-echo-during-longrun", dur.n, (System.nanoTime()-computeT0)/1e9, during, true,
                String.format("heavy≈1.1s; probes-in-window=%d (p99=%.2fms max=%.2fms); baseline p99=%.2fms",
                    win.n, win.p99/1000.0, win.max/1000.0, base.p99/1000.0));
        }

        // ---------- 13. io-async（64 actor × 20 × 10ms 异步 sleep） ----------
        {
            Behavior<Msg> ioActor = Behaviors.setup(ctx -> new AbstractBehavior<Msg>(ctx) {
                @Override public Receive<Msg> createReceive() {
                    return newReceiveBuilder()
                        .onMessage(Echo.class, m -> {
                            ctx.scheduleOnce(Duration.ofMillis(10), ctx.getSelf(), new TellDone(1));
                            return this;
                        })
                        .onMessage(TellDone.class, m -> this)
                        .build();
                }
            });
            AtomicLong done = new AtomicLong();
            // 对等语义：actor 内"处理中"状态（busy）期间排队消息不得提前完成，
            // 等价 Rust handler 内 await sleep(10ms) 的独占窗口。
            Behavior<Msg> ioAsk = Behaviors.withStash(64, stash -> Behaviors.setup(ctx -> new AbstractBehavior<Msg>(ctx) {
                boolean busy = false;
                @Override public Receive<Msg> createReceive() {
                    return newReceiveBuilder()
                        .onMessage(AskEnvelope.class, m -> {
                            if (busy) { stash.stash(m); return this; }
                            busy = true;
                            CompletableFuture.delayedExecutor(10, TimeUnit.MILLISECONDS).execute(() -> {
                                busy = false;
                                done.incrementAndGet();
                                m.reply.complete(1L);
                                ctx.getSelf().tell(new TickProbe()); // 唤醒 unstash
                            });
                            return this;
                        })
                        .onMessage(TickProbe.class, m -> stash.unstashAll(this))
                        .build();
                }
            }));
            List<ActorRef<Msg>> refs = new ArrayList<>();
            for (int i = 0; i < 64; i++)
                refs.add(sys.systemActorOf(ioAsk, "io"+i, DispatcherSelector.defaultDispatcher()));
            long t0 = System.nanoTime();
            List<CompletableFuture<Long>> fs = new ArrayList<>();
            for (ActorRef<Msg> r : refs)
                for (int k = 0; k < 20; k++) fs.add(askEnvelope(r, new Echo(1)));
            CompletableFuture.allOf(fs.toArray(new CompletableFuture[0])).get(120, TimeUnit.SECONDS);
            double wall = (System.nanoTime()-t0)/1e9;
            row("io-async-64actors-10ms", 64*20, wall, List.of(), true,
                String.format("async sleep(10ms) in handler; 1280 tasks wall=%.2fs (serial would be 12.80s; speedup %.1fx)",
                    wall, 12.8/wall));
        }

        // ---------- 14. M1 mixed-minute-cpu ----------
        {
            final int LONG_ACTORS = 8, BACKUP = 4, MED_PER = 10;
            final long LONG_ITERS = 57_200_000_000L, MED_ITERS = 2_400_000_000L;
            double nominal = (LONG_ACTORS*(double)LONG_ITERS + BACKUP*(double)MED_PER*MED_ITERS) / BURN_RATE;
            List<ActorRef<Msg>> longs = new ArrayList<>(), meds = new ArrayList<>();
            for (int i = 0; i < LONG_ACTORS; i++)
                longs.add(sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "m1l"+i, DispatcherSelector.defaultDispatcher()));
            for (int i = 0; i < BACKUP; i++)
                meds.add(sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "m1m"+i, DispatcherSelector.defaultDispatcher()));
            ActorRef<Msg> probe = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "m1p", DispatcherSelector.defaultDispatcher());

            List<Long> probeLats = Collections.synchronizedList(new ArrayList<>());
            AtomicBoolean stop = new AtomicBoolean(false);
            Thread probeT = new Thread(() -> {
                while (!stop.get()) {
                    long s = System.nanoTime();
                    try { askTiny(probe, 0xE).get(120, TimeUnit.SECONDS); } catch (Exception ignored) {}
                    probeLats.add((System.nanoTime()-s)/1000);
                    try { Thread.sleep(200); } catch (InterruptedException ie) { return; }
                }
            });
            probeT.start();
            long t0 = System.nanoTime();
            List<CompletableFuture<Long>> tasks = new ArrayList<>();
            for (int i = 0; i < LONG_ACTORS; i++)
                tasks.add(askEnvelope(longs.get(i), new MinuteCpu(LONG_ITERS, 0xD00DL + i)));
            for (int i = 0; i < BACKUP; i++)
                for (int k = 0; k < MED_PER; k++)
                    tasks.add(askEnvelope(meds.get(i), new MediumCpu(MED_ITERS, 0xBACCL + i*100 + k)));
            boolean correct = true;
            for (CompletableFuture<Long> f : tasks) {
                try { f.get(600, TimeUnit.SECONDS); } catch (Exception e) { correct = false; }
            }
            stop.set(true); probeT.join();
            double wall = (System.nanoTime()-t0)/1e9;
            Lat ps = Lat.of(probeLats);
            row("mixed-minute-cpu-plus-incoming", LONG_ACTORS + BACKUP*MED_PER, wall, probeLats, correct,
                String.format(java.util.Locale.ROOT,
                    "8×65s long + 40×2.7s medium concurrent; probe(tiny ask) p99=%.1fms max=%.1fms over %d probes",
                    ps.p99/1000.0, ps.max/1000.0, ps.n));
        }

        // ---------- 15. M2 mixed-same-actor-fifo ----------
        {
            final long LONG_ITERS = 30_000_000_000L; final int MEDIUM = 6; final long MED_ITERS = 2_400_000_000L, TINY = 4_000;
            double nominal = (LONG_ITERS + MEDIUM*(double)MED_ITERS + TINY*1000.0) / BURN_RATE;
            AtomicLong ops = new AtomicLong();
            ActorRef<Msg> shared = sys.systemActorOf(bench(ops, new AtomicLong()), "m2shared",
                DispatcherSelector.defaultDispatcher());
            long t0 = System.nanoTime();
            shared.tell(new MinuteCpu(LONG_ITERS, 1));
            for (int k = 0; k < MEDIUM; k++) shared.tell(new MediumCpu(MED_ITERS, 100+k));
            for (int k = 0; k < TINY; k++) shared.tell(new TinyTask(k));
            long tp = System.nanoTime();
            long v = askEnvelope(shared, new TinyTask(0x5)).get(300, TimeUnit.SECONDS);
            double tailMs = (System.nanoTime()-tp)/1e6;
            double wall = (System.nanoTime()-t0)/1e9;
            row("mixed-same-actor-fifo", 1+MEDIUM+TINY, wall, List.of(), true,
                String.format(java.util.Locale.ROOT,
                    "1×34s + 6×2.7s + 4000 tiny FIFO on ONE actor; tail-probe ask lat=%.0fms (≈queued-behind time)", tailMs));
        }

        // ---------- 16. M3 chunked-vs-solid（分片让出） ----------
        {
            final long TOTAL = 30_000_000_000L, CHUNK = 1_500_000_000L;
            // solid：连续烧
            AtomicLong opsS = new AtomicLong();
            ActorRef<Msg> solid = sys.systemActorOf(bench(opsS, new AtomicLong()), "m3solid",
                DispatcherSelector.defaultDispatcher());
            long t0 = System.nanoTime();
            solid.tell(new MinuteCpu(TOTAL, 3));
            long sp = System.nanoTime();
            askEnvelope(solid, new TinyTask(1)).get(300, TimeUnit.SECONDS);
            double solidWaitMs = (System.nanoTime()-sp)/1e6, solidLong = (System.nanoTime()-t0)/1e9;

            // chunked：Behavior 内分片 + ctx.getSelf() 自发续片（等价 Rust 的 yield 循环）
            Behavior<Msg> chunked = Behaviors.setup(ctx -> new AbstractBehavior<Msg>(ctx) {
                long remaining = TOTAL, chunk = CHUNK, acc = 3;
                final CompletableFuture<Long> done = new CompletableFuture<>();
                @Override public Receive<Msg> createReceive() {
                    return newReceiveBuilder()
                        .onMessage(AskEnvelope.class, m -> {
                            done.whenComplete((r, e) -> m.reply.complete(r));
                            return this;
                        })
                        .onMessage(TinyTask.class, m -> { burnChunk(); return this; })
                        .build();
                }
                void burnChunk() {
                    long take = Math.min(chunk, remaining);
                    acc += burnCpu(take, acc);
                    remaining -= take;
                    if (remaining > 0) ctx.getSelf().tell(new TinyTask(0)); // 续片（让出点）
                    else done.complete(acc);
                }
            });
            ActorRef<Msg> ch = sys.systemActorOf(chunked, "m3chunked", DispatcherSelector.defaultDispatcher());
            long t1 = System.nanoTime();
            ch.tell(new TinyTask(0)); // 启动
            long cp = System.nanoTime();
            askEnvelope(ch, new TinyTask(2)).get(300, TimeUnit.SECONDS);
            double chunkWaitMs = (System.nanoTime()-cp)/1e6, chunkLong = (System.nanoTime()-t1)/1e9;
            double yoh = (chunkLong/solidLong - 1.0) * 100.0;
            row("chunked-vs-solid-longrun", 2, solidLong+chunkLong,
                List.of((long)solidWaitMs*1000, (long)chunkWaitMs*1000), true,
                String.format(java.util.Locale.ROOT,
                    "solid=%.1fs (tiny queued %.0fms) vs chunked×20 (tiny wait %.0fms); yield overhead %.1f%%",
                    solidLong, solidWaitMs, chunkWaitMs, yoh));
        }

        // ---------- 17. E1 pingpong-rtt-10k ----------
        {
            ActorRef<Msg> pa = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "e1a",
                DispatcherSelector.defaultDispatcher());
            ActorRef<Msg> pb = sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()), "e1b",
                DispatcherSelector.defaultDispatcher());
            List<Long> lat = new ArrayList<>();
            long t0 = System.nanoTime();
            for (long i = 0; i < 10_000; i++) {
                long s = System.nanoTime();
                long va = askEcho(pa, i).get(10, TimeUnit.SECONDS);
                long vb = askEcho(pb, va).get(10, TimeUnit.SECONDS);
                if (vb != i) throw new IllegalStateException();
                if (i % 10 == 0) lat.add((System.nanoTime()-s)/1000);
            }
            double wall = (System.nanoTime()-t0)/1e9;
            row("pingpong-rtt-10k", 20_000, wall, lat, true, "2-hop ask RTT (A→B), sampled every 10th");
        }

        // ---------- 18. E2 self-chain-ask-tell-20k ----------
        {
            AtomicLong ops = new AtomicLong();
            ActorRef<Msg> ca = sys.systemActorOf(bench(ops, new AtomicLong()), "e2chain",
                DispatcherSelector.defaultDispatcher());
            final long ticks = 20_000;
            long t0 = System.nanoTime();
            for (long i = 0; i < ticks; i++) {
                askEcho(ca, i).get(10, TimeUnit.SECONDS);
                ca.tell(new Echo(i+1));
            }
            boolean ok = waitUntil(() -> ops.get() >= ticks*2, 60);
            double wall = (System.nanoTime()-t0)/1e9;
            row("self-chain-ask-tell-20k", ticks*2, wall, List.of(), ok,
                "back-to-back ask→tell to same actor (degenerate chain)");
        }

        // ---------- 19. E3 slow-consumer-8prod-40k ----------
        {
            final int P = 8; final long PER = 5_000, ITERS = 170_000;
            AtomicLong ops = new AtomicLong();
            ActorRef<Msg> cons = sys.systemActorOf(bench(ops, new AtomicLong()), "e3slow",
                DispatcherSelector.defaultDispatcher());
            long t0 = System.nanoTime();
            ExecutorService pool = Executors.newFixedThreadPool(P);
            List<Future<?>> fs = new ArrayList<>();
            for (int p = 0; p < P; p++) {
                final int pp = p;
                fs.add(pool.submit(() -> {
                    for (long k = 0; k < PER; k++) cons.tell(new CpuTask(ITERS, pp*10_000L + k));
                }));
            }
            for (Future<?> f : fs) f.get(60, TimeUnit.SECONDS);
            pool.shutdown();
            double sendS = (System.nanoTime()-t0)/1e9;
            boolean ok = waitUntil(() -> ops.get() >= P*PER*2, 300);
            double wall = (System.nanoTime()-t0)/1e9;
            row("slow-consumer-8prod-40k", P*PER, wall, List.of(), ok,
                String.format(java.util.Locale.ROOT,
                    "8 producers × 5000 × ~200µs msgs into ONE consumer; send-all=%.2fs; drain=%s", sendS, ok?"ok":"TIMEOUT"));
        }

        // ---------- 20. E4 spawn-stop-storm-5k ----------
        {
            final int WAVES = 5, PER_WAVE = 1000;
            long t0 = System.nanoTime();
            int alive = 0;
            for (int w = 0; w < WAVES; w++) {
                List<ActorRef<Msg>> refs = new ArrayList<>();
                for (int i = 0; i < PER_WAVE; i++)
                    refs.add(sys.systemActorOf(bench(new AtomicLong(), new AtomicLong()),
                        "e4w"+w+"_"+i, DispatcherSelector.defaultDispatcher()));
                for (int idx = 0; idx < 10; idx++)
                    try { askEcho(refs.get(idx), idx).get(10, TimeUnit.SECONDS); alive++; } catch (Exception ignored) {}
                for (ActorRef<Msg> r : refs) r.tell(PoisonPill.INSTANCE);
                Thread.sleep(30);
            }
            double wall = (System.nanoTime()-t0)/1e9;
            row("spawn-stop-storm-5k", WAVES*PER_WAVE, wall, List.of(), alive == WAVES*10,
                String.format("5 waves × 1000 spawn+ask+stop; alive spot-checks %d/50", alive));
        }

        // ======================= 汇总 =======================
        StringBuilder md = new StringBuilder("| 场景 | msgs | 耗时(s) | 吞吐(/s) | p50(ms) | p90(ms) | p99(ms) | max(ms) | 正确 |\n|---|---|---|---|---|---|---|---|---|\n");
        for (Row r : ROWS) {
            md.append(String.format(java.util.Locale.ROOT, "| %s | %d | %.3f | %.0f | %.2f | %.2f | %.2f | %.2f | %b |\n",
                r.name, r.msgs, r.wall, r.msgs/Math.max(r.wall,1e-9),
                r.lat.p50/1000.0, r.lat.p90/1000.0, r.lat.p99/1000.0, r.lat.max/1000.0, r.correct));
        }
        System.out.println("==================== AKKA REPORT ====================");
        System.out.println(md);
        java.nio.file.Files.write(java.nio.file.Path.of("/tmp/parrot_bench_akka.md"), md.toString().getBytes());
        sys.terminate();
        sys.getWhenTerminated().toCompletableFuture().get(30, TimeUnit.SECONDS);
    }

    static final double BURN_RATE = 880_000_000.0;

    static boolean waitUntil(java.util.function.BooleanSupplier cond, long timeoutSec) {
        long deadline = System.currentTimeMillis() + timeoutSec*1000;
        while (System.currentTimeMillis() < deadline) {
            if (cond.getAsBoolean()) return true;
            try { Thread.sleep(5); } catch (InterruptedException e) { return false; }
        }
        return cond.getAsBoolean();
    }
}
