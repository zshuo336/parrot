#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Parrot 双引擎基准的 Ray 对等实现 —— 全面降级版（10GB 内存 / 15 核 / ≤5 分钟）。

降级原则：
  - 资源硬限制：ray.init(num_cpus=15, object_store_memory=1.5GB)；
    herd 类场景 actor 数 ≤150（~25MB/进程，峰值 ~5.5GB < 10GB）
  - 时间预算：20 个场景合计目标 ≤240s（+init 20s，总 ≤300s）
  - 速率现实：本机 Ray 单客户端任务提交 ~330/s（8 线程也只有 ~330/s，
    瓶颈在客户端序列化/gcs），所有大规模场景按此速率缩量
  - CPU 场景迭代数按 86× 缩减保持"标称计算时长"与对等基准一致
  - 计算时长标定：Python burn ~10.2M iters/s（LCG wrapping 64 位）
  - 消息语义：ask=ray.get(actor.method.remote())，tell=不 get
  - 预热 500 echo 后正式计时
  - p50/p90/p99/max 分位数算法一致（ceil 索引）
  - **每场景结束 ray.kill 所有 actor**（前两版死锁根因：未 kill 的
    num_cpus=1 actor 持续占用 CPU 配额，累积后 15 核耗尽 → 排队死锁）
  - 所有 ray.get 带超时；wait_count 轮询带 deadline

运行：./run.sh
"""
import os
import sys
import time
import math
import threading
import concurrent.futures as cf

os.environ.setdefault("RAY_DEDUP_LOGS", "0")

import ray

# ======================= 速率标定与迭代缩放 =======================

PY_RATE = 10_240_000.0      # 本机 Python 3.9 burn 标定 iters/s
REF_RATE = 880_000_000.0    # 对等基准（C 级）burn 速率 iters/s
SCALE = REF_RATE / PY_RATE  # ≈ 86：保持"标称计算时长"对等
RAY_TASK_RATE = 330.0       # 本机实测：单客户端任务提交/完成速率上限

def iters_for(ref_iters: int) -> int:
    """时间对等：把对等基准迭代数缩为本语言等效迭代数。"""
    return max(1, round(ref_iters / SCALE))

# ======================= 负载内核 =======================

MASK = (1 << 64) - 1

def burn_cpu(iterations: int, salt: int) -> int:
    x = (salt | 1) & MASK
    for i in range(iterations):
        x = (x * 6364136223846793005 + 1442695040888963407) & MASK
        x ^= i
        if x == 42:
            return x
    return x

# ======================= 统计 =======================

def lat_stats(samples_us):
    if not samples_us:
        return dict(n=0, p50=0, p90=0, p99=0, max=0, mean=0.0)
    s = sorted(samples_us)
    n = len(s)
    def pick(q):
        idx = max(0, min(n - 1, math.ceil(q / 100.0 * n) - 1))
        return s[idx]
    return dict(n=n, p50=pick(50), p90=pick(90), p99=pick(99), max=s[-1],
                mean=sum(s) / n)

ROWS = []
SCALED_NOTE = f"iters scaled 1/{SCALE:.0f} (time-parity)"
LIVE_ACTORS = []

def track(handle):
    LIVE_ACTORS.append(handle)
    return handle

def kill_tracked():
    """杀掉本进程创建的所有 actor —— 防 CPU 配额泄漏（前两版死锁根因）。"""
    for a in LIVE_ACTORS:
        try:
            ray.kill(a, no_restart=True)
        except Exception:
            pass
    LIVE_ACTORS.clear()

def row(name, msgs, wall, lat_us, correct, note=""):
    st = lat_stats(lat_us)
    tput = msgs / wall if wall > 0 else 0.0
    print(f"[ray] {name} | msgs={msgs} | wall={wall:.3f}s | tput={tput:.0f}/s | "
          f"lat p50={st['p50']/1000:.1f}ms p90={st['p90']/1000:.1f}ms p99={st['p99']/1000:.1f}ms "
          f"max={st['max']/1000:.1f}ms | correct={correct}", flush=True)
    if note:
        print(f"        note: {note}", flush=True)
    ROWS.append((name, msgs, wall, tput, st, correct, note))

def wait_count(actor, target, timeout, poll=0.02):
    dl = time.time() + timeout
    while time.time() < dl:
        try:
            if ray.get(actor.count.remote(), timeout=15) >= target:
                return True
        except Exception:
            pass
        time.sleep(poll)
    return False

# ======================= BenchActor =======================

@ray.remote(num_cpus=1)
class BenchActor:
    def __init__(self):
        self.ops = 0
        self.cpu_sink = 0

    def echo(self, v):
        self.ops += 1
        return v

    def cpu(self, iters, salt):
        self.ops += 1
        r = burn_cpu(iters, salt)
        self.cpu_sink += r
        return r

    def tiny(self, salt):
        r = burn_cpu(iters_for(1000), salt)
        self.cpu_sink += r
        return r

    def medium(self, iters, salt):
        self.ops += 1
        r = burn_cpu(iters, salt)
        self.cpu_sink += r
        self.ops += 1
        return r

    def minute(self, iters, salt):
        self.ops += 1
        r = burn_cpu(iters, salt)
        self.cpu_sink += r
        self.ops += 1
        return r

    def longrun(self, iters):
        self.ops += 1
        r = burn_cpu(iters, 7)
        self.cpu_sink += r
        self.ops += 1
        return r

    def io_sleep(self, ms, v):
        time.sleep(ms / 1000.0)
        self.ops += 1
        return v

    def count(self):
        return self.ops

# ======================= main =======================

def main():
    ray.init(ignore_reinit_error=True, logging_level="ERROR",
             num_cpus=15,
             object_store_memory=1_500_000_000,   # 1.5GB object store
             _temp_dir="/tmp/ray_bench_tmp")
    print("==================== RAY BENCH (降级版 15核/10GB/≤5min) ====================", flush=True)
    print(f"cores={os.cpu_count()} SCALE=1/{SCALE:.0f} task_rate≈{RAY_TASK_RATE:.0f}/s", flush=True)

    T_START = time.perf_counter()

    def budget():
        return f"[{time.perf_counter()-T_START:6.1f}s/300s]"

    # warmup（500 echo，~2s）
    w = track(BenchActor.remote())
    ray.get([w.echo.remote(i) for i in range(500)])
    kill_tracked()
    print(f"{budget()} warmup done", flush=True)

    # ---------- 1. seq-ask-echo-500 ----------
    a = track(BenchActor.remote())
    lat = []
    t0 = time.perf_counter()
    for i in range(500):
        s = time.perf_counter()
        v = ray.get(a.echo.remote(i), timeout=30)
        assert v == i
        lat.append((time.perf_counter() - s) * 1e6)
    wall = time.perf_counter() - t0
    row("seq-ask-echo-500", 500, wall, lat, True)
    kill_tracked()

    # ---------- 2. conc-ask-echo-c8-m200 ----------
    # 8 并发对同一 actor：测 actor 串行处理下的排队延迟
    a = track(BenchActor.remote())
    samples = []
    lock = threading.Lock()
    def drive_c8(c):
        local = []
        for i in range(200):
            s = time.perf_counter()
            v = ray.get(a.echo.remote(i + c), timeout=30)
            assert v == i + c
            local.append((time.perf_counter() - s) * 1e6)
        with lock:
            samples.extend(local)
    t0 = time.perf_counter()
    with cf.ThreadPoolExecutor(8) as ex:
        list(ex.map(drive_c8, range(8)))
    wall = time.perf_counter() - t0
    row("conc-ask-echo-c8-m200", 1600, wall, samples, True, "concurrency=8")
    kill_tracked()

    # ---------- 3. conc-ask-echo-c16-m50 ----------
    # 16 并发（c64 在 Ray 单客户端 ~330/s 提交上限下无意义，降为 16）
    a = track(BenchActor.remote())
    samples = []
    def drive_c16(c):
        local = []
        for i in range(50):
            s = time.perf_counter()
            v = ray.get(a.echo.remote(i + c), timeout=60)
            assert v == i + c
            local.append((time.perf_counter() - s) * 1e6)
        with lock:
            samples.extend(local)
    t0 = time.perf_counter()
    with cf.ThreadPoolExecutor(16) as ex:
        list(ex.map(drive_c16, range(16)))
    wall = time.perf_counter() - t0
    row("conc-ask-echo-c16-m50", 800, wall, samples, True, "concurrency=16")
    kill_tracked()
    print(f"{budget()} scenes 1-3 done", flush=True)

    # ---------- 4. tell-echo-5k ----------
    # Ray 任务速率 ~330/s：5k 排空需 ~15s
    a = track(BenchActor.remote())
    N4 = 5_000
    t0 = time.perf_counter()
    for i in range(N4):
        a.echo.remote(i)
    ok = wait_count(a, N4, 60)
    wall = time.perf_counter() - t0
    row("tell-echo-5k", N4, wall, [], ok,
        ("" if ok else "DRAIN TIMEOUT ") + "fire-and-forget drain")
    kill_tracked()

    # ---------- 5. cpu-serial-100x200k ----------
    a = track(BenchActor.remote())
    it5 = iters_for(200_000)   # ~200µs 标称
    lat = []
    t0 = time.perf_counter()
    for i in range(100):
        s = time.perf_counter()
        ray.get(a.cpu.remote(it5, i), timeout=30)
        lat.append((time.perf_counter() - s) * 1e6)
    wall = time.perf_counter() - t0
    row("cpu-serial-100x200k", 100, wall, lat, True,
        f"~200µs work/msg; {SCALED_NOTE}")
    kill_tracked()
    print(f"{budget()} scenes 4-5 done", flush=True)

    # ---------- 6. cpu-parallel-4actors-200k ----------
    # 前两版死锁点：现在 (a) 场景间全 kill（无配额泄漏）(b) 4 actors
    # 4 线程 (c) 每个 ray.get 带超时
    actors = [track(BenchActor.remote()) for _ in range(4)]
    it6 = iters_for(200_000)
    t0 = time.perf_counter()
    def drive_cpu(i):
        out = []
        for k in range(25):
            s = time.perf_counter()
            ray.get(actors[i].cpu.remote(it6, i * 1000 + k), timeout=60)
            out.append((time.perf_counter() - s) * 1e6)
        return out
    with cf.ThreadPoolExecutor(4) as ex:
        all_lat = [x for lst in ex.map(drive_cpu, range(4)) for x in lst]
    wall = time.perf_counter() - t0
    row("cpu-parallel-4actors-200k", 4 * 25, wall, all_lat, True,
        f"4 actors × 25 msgs × ~200µs; {SCALED_NOTE}")
    kill_tracked()
    print(f"{budget()} scene 6 done", flush=True)

    # ---------- 7. flood-4k-tell ----------
    # 4 线程提交 × 1k（提交与排空都在 ~330/s 速率约束下）
    a = track(BenchActor.remote())
    n = 4_000
    t0 = time.perf_counter()
    def flood_quarter(p):
        for i in range(n // 4):
            a.echo.remote(i + p)
    with cf.ThreadPoolExecutor(4) as ex:
        list(ex.map(flood_quarter, range(4)))
    sent = time.perf_counter() - t0
    ok = wait_count(a, n, 60, 0.05)
    wall = time.perf_counter() - t0
    row("flood-4k-tell", n, wall, [], ok,
        f"sent in {sent:.3f}s; {'drained' if ok else 'TIMEOUT'}")
    kill_tracked()

    # ---------- 8. ask-timeout-short ----------
    a = track(BenchActor.remote())
    heavy = a.cpu.remote(iters_for(50_000_000), 1)  # ~50ms 标称
    time.sleep(0.05)
    timed_out = False
    try:
        ray.get(a.echo.remote(1), timeout=0.001)
    except ray.exceptions.GetTimeoutError:
        timed_out = True
    except Exception:
        timed_out = False
    ray.get(heavy, timeout=30)
    row("ask-timeout-short", 2, 0.0, [], timed_out,
        f"short-timeout ask: {'timeout-correct' if timed_out else 'NO-TIMEOUT'}")
    kill_tracked()

    # ---------- 9. send-after-stop ----------
    a = track(BenchActor.remote())
    ray.get(a.echo.remote(0), timeout=30)  # 确认活
    ray.kill(a, no_restart=True)
    LIVE_ACTORS.clear()
    time.sleep(0.3)
    dead = False
    try:
        ray.get(a.echo.remote(1), timeout=30)
    except ray.exceptions.RayActorError:
        dead = True
    except Exception:
        dead = False
    row("send-after-stop", 1, 0.0, [], dead,
        f"send after kill => {'RayActorError(dead-letter)' if dead else 'NO-ERROR'}")
    print(f"{budget()} scenes 7-9 done", flush=True)

    # ---------- 10. herd-150-actors ----------
    # 内存预算：150 × ~25MB = 3.75GB + Ray 底座 ~1.5GB + store 1.5GB
    # ≈ 6.8GB < 10GB ✓；num_cpus=0 无 CPU 预留立即调度
    N10 = 150
    t0 = time.perf_counter()
    actors = [track(BenchActor.options(num_cpus=0).remote()) for _ in range(N10)]
    spawn_wall = time.perf_counter() - t0
    t1 = time.perf_counter()
    for act in actors:
        act.echo.remote(1)
    send_wall = time.perf_counter() - t1
    ok_n = 0
    for act in actors[:30]:
        try:
            if ray.get(act.count.remote(), timeout=30) >= 1:
                ok_n += 1
        except Exception:
            pass
    row(f"herd-{N10}-actors", N10, spawn_wall, [], ok_n == 30,
        f"spawned {N10} in {spawn_wall:.3f}s ({N10/spawn_wall:.0f}/s); "
        f"send-all={send_wall:.3f}s; alive {ok_n}/30; "
        f"NOTE: Ray actor=OS process ~25MB each (mem budget 10GB)")
    kill_tracked()
    time.sleep(3.0)  # 等 150 进程退出，释放内存与 fork 服务

    # ---------- 11. longrun-2G-iters ----------
    a = track(BenchActor.remote())
    t0 = time.perf_counter()
    v = ray.get(a.longrun.remote(iters_for(2_000_000_000)), timeout=120)
    wall = time.perf_counter() - t0
    row("longrun-2G-iters", 1, wall, [wall * 1e6], v != 0,
        f"single 2G-iteration compute ({iters_for(2_000_000_000)} real iters)")
    kill_tracked()

    # ---------- 12. starve-echo-during-longrun ----------
    aa = track(BenchActor.remote())
    ab = track(BenchActor.remote())
    samples = []
    stop_flag = threading.Event()
    def ticker():
        i = 0
        while not stop_flag.is_set():
            s = time.perf_counter()
            try:
                v = ray.get(ab.echo.remote(i), timeout=30)
                assert v == i
                samples.append((time.perf_counter() - s) * 1e6)
            except Exception:
                pass
            i += 1
            time.sleep(0.005)
    th = threading.Thread(target=ticker)
    th.start()
    time.sleep(0.15)
    base_n = len(samples)
    base = lat_stats(samples[:base_n])
    t0 = time.perf_counter()
    ray.get(aa.longrun.remote(iters_for(1_000_000_000)), timeout=120)
    compute = time.perf_counter() - t0
    time.sleep(0.15)
    during = samples[base_n:]
    stop_flag.set()
    th.join()
    dstats = lat_stats(during)
    row("starve-echo-during-longrun", dstats["n"], compute, during, True,
        f"heavy={compute:.2f}s; probes={dstats['n']} "
        f"(p99={dstats['p99']/1000:.2f}ms max={dstats['max']/1000:.2f}ms); "
        f"baseline p99={base['p99']/1000:.2f}ms; "
        f"NOTE: Ray actors are separate processes → no starvation by design")
    kill_tracked()
    print(f"{budget()} scenes 10-12 done", flush=True)

    # ---------- 13. io-async-8actors-10ms ----------
    # 8 个 num_cpus=0 actor 进程并行、每个进程内任务串行
    actors = [track(BenchActor.options(num_cpus=0).remote()) for _ in range(8)]
    t0 = time.perf_counter()
    refs = []
    for act in actors:
        refs.extend(act.io_sleep.remote(10, 1) for _ in range(40))
    ray.get(refs[-1:], timeout=60)
    ok = all(wait_count(actors[i], 40, 60) for i in range(4))
    wall = time.perf_counter() - t0
    total = 8 * 40
    serial = total * 10 / 1000
    row("io-async-8actors-10ms", total, wall, [], ok,
        f"sleep(10ms) in handler; {total} tasks wall={wall:.2f}s "
        f"(serial would be {serial:.2f}s; speedup {serial/max(wall,1e-9):.1f}x)")
    kill_tracked()
    time.sleep(0.5)

    # ---------- 14. mixed-minute-cpu-plus-incoming ----------
    # 4×5s 标称长任务 + 2 actors × 10×0.3s 中任务 + probe
    long_iters = iters_for(4_400_000_000)    # ~5s 标称
    med_iters = iters_for(26_000_000)        # ~0.3s 标称
    long_actors = [track(BenchActor.remote()) for _ in range(4)]
    backup = [track(BenchActor.remote()) for _ in range(2)]
    probe = track(BenchActor.remote())
    probe_lat = []
    t0 = time.perf_counter()
    longs = [a.minute.remote(long_iters, i) for i, a in enumerate(long_actors)]
    meds = []
    for a in backup:
        for k in range(10):
            meds.append(a.medium.remote(med_iters, k))
            time.sleep(0.02)
    stop_probe = threading.Event()
    def probe_loop():
        while not stop_probe.is_set():
            s = time.perf_counter()
            try:
                ray.get(probe.tiny.remote(1), timeout=30)
                probe_lat.append((time.perf_counter() - s) * 1e6)
            except Exception:
                pass
    pt = threading.Thread(target=probe_loop)
    pt.start()
    ray.get(longs + meds, timeout=120)
    wall = time.perf_counter() - t0
    stop_probe.set()
    pt.join()
    st = lat_stats(probe_lat)
    row("mixed-minute-cpu-plus-incoming", 4 + 20, wall, probe_lat, True,
        f"4×5s long + 20×0.3s medium concurrent; probe p99={st['p99']/1000:.1f}ms "
        f"max={st['max']/1000:.1f}ms over {st['n']} probes; {SCALED_NOTE}")
    kill_tracked()

    # ---------- 15. mixed-same-actor-fifo ----------
    a = track(BenchActor.remote())
    t0 = time.perf_counter()
    a.minute.remote(iters_for(3_500_000_000), 1)   # ~4s 标称
    for k in range(4):
        a.medium.remote(iters_for(26_000_000), k)  # 4×0.3s
    for k in range(300):
        a.tiny.remote(k)
    nominal_done = (1 + 4) * 2
    tail = None
    dl = time.time() + 60
    while time.time() < dl:
        if wait_count(a, nominal_done, 5, 0.02):
            s = time.perf_counter()
            ray.get(a.tiny.remote(0), timeout=30)
            tail = time.perf_counter() - s
            break
    wall = time.perf_counter() - t0
    row("mixed-same-actor-fifo", 1 + 4 + 300, wall, [], tail is not None,
        f"1×4s + 4×0.3s + 300 tiny FIFO on ONE actor; "
        f"tail-probe lat={tail*1000 if tail else -1:.0f}ms; {SCALED_NOTE}")
    kill_tracked()
    print(f"{budget()} scenes 13-15 done", flush=True)

    # ---------- 16. chunked-vs-solid-longrun ----------
    a = track(BenchActor.remote())
    b = track(BenchActor.remote())
    t0 = time.perf_counter()
    ray.get(a.longrun.remote(iters_for(6_000_000_000)), timeout=120)  # ~7s 标称
    solid = time.perf_counter() - t0
    t0 = time.perf_counter()
    for _ in range(8):
        ray.get(b.longrun.remote(iters_for(750_000_000)), timeout=120)  # 8×~0.9s
    chunked = time.perf_counter() - t0
    overhead = (chunked / solid - 1) * 100
    row("chunked-vs-solid-longrun", 2, solid + chunked,
        [solid * 1e6, chunked * 1e6], True,
        f"solid={solid:.1f}s vs chunked×8={chunked:.1f}s; overhead {overhead:.1f}%")
    kill_tracked()

    # ---------- 17. pingpong-rtt-1k ----------
    pa = track(BenchActor.remote())
    pb = track(BenchActor.remote())
    lat = []
    t0 = time.perf_counter()
    for i in range(1000):
        s = time.perf_counter()
        va = ray.get(pa.echo.remote(i), timeout=30)
        vb = ray.get(pb.echo.remote(va), timeout=30)
        assert vb == i
        if i % 10 == 0:
            lat.append((time.perf_counter() - s) * 1e6)
    wall = time.perf_counter() - t0
    row("pingpong-rtt-1k", 2000, wall, lat, True,
        "2-hop ask RTT (A→B), sampled every 10th")
    kill_tracked()

    # ---------- 18. self-chain-ask-tell-1k ----------
    a = track(BenchActor.remote())
    N18 = 1_000
    t0 = time.perf_counter()
    for i in range(N18):
        v = ray.get(a.echo.remote(i), timeout=30)
        a.echo.remote(v + 1)
    ok = wait_count(a, N18 * 2, 30, 0.002)
    wall = time.perf_counter() - t0
    row("self-chain-ask-tell-1k", N18 * 2, wall, [], ok,
        "back-to-back ask→tell to same actor")
    kill_tracked()

    # ---------- 19. slow-consumer-4prod-1250 ----------
    # 4 producers × 1250 × ~200µs 标称
    consumer = track(BenchActor.remote())
    per_msg = iters_for(170_000)   # ~200µs 标称
    t0 = time.perf_counter()
    def producer(p):
        for k in range(1250):
            consumer.cpu.remote(per_msg, p * 10_000 + k)
    with cf.ThreadPoolExecutor(4) as ex:
        list(ex.map(producer, range(4)))
    send_t = time.perf_counter() - t0
    target = 4 * 1250
    ok = wait_count(consumer, target * 2, 90, 0.05)
    wall = time.perf_counter() - t0
    row("slow-consumer-4prod-5k", target, wall, [], ok,
        f"4 producers × 1250 × ~200µs msgs; send-all={send_t:.2f}s; "
        f"{'drained' if ok else 'TIMEOUT'}")
    kill_tracked()
    print(f"{budget()} scenes 16-19 done", flush=True)

    # ---------- 20. spawn-stop-storm（3 waves × 100） ----------
    PER_WAVE = 100
    t0 = time.perf_counter()
    alive = 0
    for w in range(3):
        acts = [track(BenchActor.options(num_cpus=0).remote()) for _ in range(PER_WAVE)]
        for i, a in enumerate(acts[:10]):
            try:
                ray.get(a.echo.remote(i), timeout=30)
                alive += 1
            except Exception:
                pass
        kill_tracked()
        time.sleep(0.05)
    wall = time.perf_counter() - t0
    row("spawn-stop-storm-300", 3 * PER_WAVE, wall, [], alive == 30,
        f"3 waves × {PER_WAVE} spawn+ask+kill; alive {alive}/30")

    # ======================= 汇总 =======================
    total_wall = time.perf_counter() - T_START
    print("\n==================== RAY SUMMARY ====================", flush=True)
    for name, msgs, wall, tput, st, ok, note in ROWS:
        print(f"{name:36s} tput={tput:>10.0f}/s ok={ok}", flush=True)
    print(f"\nTOTAL WALL: {total_wall:.1f}s (budget 300s)", flush=True)
    ray.shutdown()

if __name__ == "__main__":
    main()
