#!/usr/bin/env python3
"""Twenty real Ray workloads, independent answers and owned lifetimes."""
from __future__ import annotations
import argparse
import concurrent.futures as cf
import json
import math
from pathlib import Path
import signal
import threading
import time

import ray
from bench_plan import Plan
import bench_runtime as rt


def equal(got, expected):
    rt.require(got == expected, f"result mismatch: {got!r} != {expected!r}")


def lat_stats(samples):
    if not samples:
        return dict(n=0, p50=0, p90=0, p99=0, max=0)
    s = sorted(samples)
    rt.require(all(math.isfinite(x) and x >= 0 for x in s), "invalid latency")
    n = len(s)
    return dict(
        n=n,
        p50=s[(n * 50 + 99) // 100 - 1],
        p90=s[(n * 90 + 99) // 100 - 1],
        p99=s[(n * 99 + 99) // 100 - 1],
        max=s[-1],
    )


def row(name, messages, wall, samples, correct, note=""):
    rt.require(correct is True, f"incorrect benchmark row: {name}")
    rt.require(messages > 0 and math.isfinite(wall) and wall > 0, "invalid measurement")
    lat = lat_stats(samples)
    print(
        f"[ray] {name} | msgs={messages} | wall={wall:.6f}s | tput={messages/wall:.3f}/s | "
        f'p99={lat["p99"]/1000:.3f}ms | correct=true | {note}',
        flush=True,
    )
    return dict(name=name, messages=messages, seconds=wall, latency_us=lat, note=note)


def elapsed(start):
    return (rt.now_ns() - start) / 1e9


def micros(start):
    return (rt.now_ns() - start) / 1000


def snapshot(actor, count, checksum):
    state = rt.get(actor.snapshot.remote())
    equal(state[:2], (count, checksum & rt.MASK))
    return state


class Probes:
    def __init__(self, actor, iterations, expected):
        self.stop = threading.Event()
        self.abort = threading.Event()
        self.ready = cf.Future()
        self.done = cf.Future()
        self.samples = []

        def run():
            rt._worker.cancellation = self.abort
            try:
                while not self.stop.is_set():
                    rt.require(len(self.samples) < 1_000_000, "probe sample limit")
                    start = rt.now_ns()
                    ref = (
                        actor.echo.remote(1)
                        if iterations is None
                        else actor.cpu.remote(iterations, 1)
                    )
                    equal(rt.get(ref), expected)
                    self.samples.append((start, rt.now_ns()))
                    if not self.ready.done():
                        self.ready.set_result(None)
                    self.stop.wait(0.005)
                self.done.set_result(None)
            except BaseException as error:
                if not self.ready.done():
                    self.ready.set_exception(error)
                self.done.set_exception(error)
            finally:
                del rt._worker.cancellation

        self.thread = threading.Thread(target=run, name="ray-bench-probe")
        self.thread.start()
        try:
            self.ready.result(timeout=30)
        except BaseException:
            self.close()
            raise

    def check(self):
        if self.done.done():
            self.done.result()

    def close(self):
        self.stop.set()
        self.thread.join(10)
        if self.thread.is_alive():
            self.abort.set()
            self.thread.join(10)
            rt.require(not self.thread.is_alive(), "probe did not join")
            raise TimeoutError("probe failed to finish in cleanup budget")
        self.done.result()

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()


class Benchmark:
    def __init__(self, plan, path):
        self.p = plan
        self.answers = {}
        for line in Path(path).read_text().splitlines():
            n, salt, value = map(int, line.split())
            rt.require((n, salt) not in self.answers, "duplicate oracle input")
            self.answers[n, salt] = value
        equal(set(self.answers), self.p.keys())
        self.rows = []
        self.retired = 0

    def answer(self, n, salt):
        return self.answers[n, salt]

    def record(self, *args):
        self.rows.append(row(*args))

    def retire(self, scope):
        before = scope.retired
        scope.retire()
        self.retired += scope.retired - before

    def echo(self, scope, name, workers, per):
        h = scope.create().actor
        expected = sum(a + k for a in range(workers) for k in range(per))
        start = rt.now_ns()

        def drive(a):
            samples = []
            for k in range(per):
                t = rt.now_ns()
                equal(rt.get(h.echo.remote(a + k)), a + k)
                samples.append(micros(t))
            return samples

        samples = [x for part in rt.parallel(workers, drive) for x in part]
        wall = elapsed(start)
        snapshot(h, workers * per, expected)
        self.retire(scope)
        self.record(name, workers * per, wall, samples, True, f"concurrency={workers}")

    def cpu(self, scope, name, actors, per, n, stride):
        hs = [h.actor for h in scope.create_many(actors)]
        sums = [
            sum(self.answer(n, a * stride + k) for k in range(per))
            for a in range(actors)
        ]
        start = rt.now_ns()

        def drive(a):
            samples = []
            for k in range(per):
                t = rt.now_ns()
                equal(
                    rt.get(hs[a].cpu.remote(n, a * stride + k)),
                    self.answer(n, a * stride + k),
                )
                samples.append(micros(t))
            return samples

        samples = [x for part in rt.parallel(actors, drive) for x in part]
        wall = elapsed(start)
        for a, h in enumerate(hs):
            snapshot(h, per, sums[a])
        self.retire(scope)
        self.record(
            name,
            actors * per,
            wall,
            samples,
            True,
            f"actors={actors}; iterations/message={n}",
        )

    def tell(self, scope, name, workers, per, n, stride):
        h = scope.create().actor
        expected = sum(
            a * stride + k if n is None else self.answer(n, a * stride + k)
            for a in range(workers)
            for k in range(per)
        )
        start = rt.now_ns()

        def drive(a):
            for k in range(per):
                if n is None:
                    h.echo.remote(a * stride + k)
                else:
                    h.cpu.remote(n, a * stride + k)

        rt.parallel(workers, drive)
        snapshot(h, workers * per, expected)
        wall = elapsed(start)
        self.retire(scope)
        self.record(
            name,
            workers * per,
            wall,
            [],
            True,
            "all producers joined; exact drained count and modulo-u64 checksum",
        )

    def gate(self, scope, h):
        gate = scope.create(rt.Gate).actor
        admitted = h.gate.remote(gate)
        rt.get(gate.wait_entered.remote())
        return gate, admitted

    def timeout(self, scope):
        h = scope.create().actor
        gate, admitted = self.gate(scope, h)
        n = self.p.iterations(50_000_000)
        want = self.answer(n, 1)
        start = rt.now_ns()
        heavy = h.cpu.remote(n, 1)
        queued = h.echo.remote(23)
        try:
            try:
                ray.get(queued, timeout=0.001)
            except ray.exceptions.GetTimeoutError:
                pass
            else:
                raise RuntimeError("expected queued timeout")
        finally:
            rt.get(gate.release.remote())
        rt.get(admitted)
        equal(rt.get(heavy), want)
        equal(rt.get(queued), 23)
        snapshot(h, 2, want + 23)
        wall = elapsed(start)
        self.retire(scope)
        self.record(
            "ask-timeout-short",
            2,
            wall,
            [],
            True,
            "admitted gate + queued CPU; caller timeout does not cancel later completion",
        )

    def stopped(self, scope):
        h = scope.create()
        equal(rt.get(h.actor.echo.remote(7)), 7)
        self.retire(scope)
        start = rt.now_ns()
        try:
            rt.get(h.actor.echo.remote(8), seconds=10)
        except ray.exceptions.RayActorError:
            pass
        else:
            raise RuntimeError("stopped actor replied")
        self.record(
            "send-after-stop",
            1,
            elapsed(start),
            [],
            True,
            "actor death and matching OS process exit observed before sending",
        )

    def herd(self, scope):
        n = self.p.count(150, 4)
        start = rt.now_ns()
        hs = scope.create_many(n, num_cpus=0)
        spawn = elapsed(start)
        equal(
            rt.all_results([h.actor.echo.remote(i) for i, h in enumerate(hs)]),
            list(range(n)),
        )
        wall = elapsed(start)
        for i, h in enumerate(hs):
            snapshot(h.actor, 1, i)
        self.retire(scope)
        self.record(
            "herd-150-actors",
            n,
            wall,
            [],
            True,
            f"all {n} actor processes validated; ready-spawn seconds={spawn:.6f}; wall includes echoes",
        )

    def longrun(self, scope):
        h = scope.create().actor
        n = self.p.iterations(2_000_000_000)
        want = self.answer(n, 7)
        start = rt.now_ns()
        equal(rt.get(h.cpu.remote(n, 7)), want)
        wall = elapsed(start)
        snapshot(h, 1, want)
        self.retire(scope)
        self.record(
            "longrun-2G-iters", 1, wall, [wall * 1e6], True, f"actual iterations={n}"
        )

    def monitored(self, scope, mixed):
        it, c = self.p.iterations, self.p.count
        if mixed:
            longs = [h.actor for h in scope.create_many(c(4, 2))]
            meds = [h.actor for h in scope.create_many(c(2, 1))]
            tasks = [(h, it(4_400_000_000), a) for a, h in enumerate(longs)]
            tasks += [(h, it(26_000_000), k) for h in meds for k in range(c(10, 2))]
        else:
            tasks = [(scope.create().actor, it(1_000_000_000), 7)]
        probe = scope.create().actor
        probe_n = it(1000) if mixed else None
        probe_value = self.answer(probe_n, 1) if mixed else 1
        p = Probes(probe, probe_n, probe_value)
        start = rt.now_ns()
        with p:
            refs = [h.cpu.remote(n, salt) for h, n, salt in tasks]
            values = rt.all_results(refs, observer=p.check)
            equal(values, [self.answer(n, salt) for _, n, salt in tasks])
            p.check()
        end = rt.now_ns()
        states = []
        distinct = {h for h, _, _ in tasks}
        for h in distinct:
            selected = [(n, s) for actor, n, s in tasks if actor == h]
            states.append(
                snapshot(h, len(selected), sum(self.answer(n, s) for n, s in selected))
            )
        cpu_start = min(s[2] for s in states)
        cpu_end = max(s[3] for s in states)
        samples = [
            (b - a) / 1000 for a, b in p.samples if a < cpu_end and b > cpu_start
        ]
        timed = sum(a >= start and b <= end for a, b in p.samples)
        snapshot(probe, len(p.samples), len(p.samples) * probe_value)
        self.retire(scope)
        self.record(
            "mixed-minute-cpu-plus-incoming" if mixed else "starve-echo-during-longrun",
            len(tasks) + timed,
            (end - start) / 1e9,
            samples,
            True,
            f"CPU jobs={len(tasks)}; timed probes={timed}; all probes={len(p.samples)}; actual CPU interval overlap probes={len(samples)}; zero overlap means no latency evidence",
        )

    def io(self, scope):
        c = self.p.count
        actors, per = c(8, 2), c(40, 3)
        hs = scope.create_many(actors, rt.IOActor)
        start = rt.now_ns()
        refs = [
            h.actor.io.remote(10, a * 100 + k)
            for a, h in enumerate(hs)
            for k in range(per)
        ]
        equal(
            rt.all_results(refs),
            [a * 100 + k for a in range(actors) for k in range(per)],
        )
        wall = elapsed(start)
        for a, h in enumerate(hs):
            state = snapshot(h.actor, per, per * a * 100 + per * (per - 1) // 2)
            equal(state[2:], (per, 0, 1))
        self.retire(scope)
        self.record(
            "io-async-8actors-10ms",
            actors * per,
            wall,
            [],
            True,
            "actual asyncio sleep; one lock serializes each actor; every payload and peak concurrency checked",
        )

    def fifo(self, scope):
        it, c = self.p.iterations, self.p.count
        h = scope.create().actor
        gate, admitted = self.gate(scope, h)
        n, med, tiny = it(3_500_000_000), it(26_000_000), it(1000)
        count = c(300, 8)
        expected = (
            self.answer(n, 1)
            + sum(self.answer(med, k) for k in range(4))
            + sum(self.answer(tiny, k) for k in range(count))
            + self.answer(tiny, 0)
        )
        start = rt.now_ns()
        h.cpu.remote(n, 1)
        for k in range(4):
            h.cpu.remote(med, k)
        for k in range(count):
            h.cpu.remote(tiny, k)
        tail = h.cpu.remote(tiny, 0)
        try:
            rt.require(not ray.wait([tail], timeout=0)[0], "tail bypassed gate")
        finally:
            rt.get(gate.release.remote())
        rt.get(admitted)
        equal(rt.get(tail), self.answer(tiny, 0))
        wall = elapsed(start)
        snapshot(h, 6 + count, expected)
        self.retire(scope)
        self.record(
            "mixed-same-actor-fifo",
            6 + count,
            wall,
            [wall * 1e6],
            True,
            "tail is queued before gate release; exact checksum includes tail; no drain before starting probe",
        )

    def chunked(self, scope):
        total = self.p.iterations(6_000_000_000)
        size = (total + 7) // 8
        tiny = self.p.iterations(1000)
        want = self.answer(total, 7)
        solid = scope.create().actor
        gate, admitted = self.gate(scope, solid)
        start = rt.now_ns()
        job = solid.cpu.remote(total, 7)
        probe = solid.cpu.remote(tiny, 1)
        rt.get(gate.release.remote())
        rt.get(admitted)
        equal(rt.get(probe), self.answer(tiny, 1))
        equal(rt.get(job), want)
        solid_wall = elapsed(start)
        snapshot(solid, 2, want + self.answer(tiny, 1))
        self.retire(scope)
        chunk = scope.create(rt.ChunkActor).actor
        gate = scope.create(rt.Gate).actor
        start = rt.now_ns()
        job = chunk.run.remote(total, size, 7, gate)
        rt.get(gate.wait_entered.remote())
        probe_start = rt.now_ns()
        try:
            value, position, done = rt.get(chunk.probe.remote(tiny, 2))
            equal(value, self.answer(tiny, 2))
            equal((position, done), (min(size, total), False))
            rt.require(
                not ray.wait([job], timeout=0)[0],
                "chunk job completed before admitted probe",
            )
            probe_us = micros(probe_start)
        finally:
            rt.get(gate.release.remote())
        equal(rt.get(job), want)
        chunk_wall = elapsed(start)
        snapshot(chunk, 2, want + self.answer(tiny, 2))
        self.retire(scope)
        self.record(
            "chunked-vs-solid-longrun",
            4,
            solid_wall + chunk_wall,
            [solid_wall * 1e6, probe_us],
            True,
            f"same continuous recurrence/global index; total={total}; chunk_size={size}; probe admitted after first chunk and before completion; gate overhead included",
        )

    def pingpong(self, scope):
        a, b = [h.actor for h in scope.create_many(2)]
        n = self.p.count(1000, 12)
        samples = []
        start = rt.now_ns()
        for i in range(n):
            t = rt.now_ns()
            v = rt.get(a.echo.remote(i))
            equal(v, i)
            equal(rt.get(b.echo.remote(v)), i)
            if i % 10 == 0:
                samples.append(micros(t))
        wall = elapsed(start)
        for h in [a, b]:
            snapshot(h, n, n * (n - 1) // 2)
        self.retire(scope)
        self.record(
            "pingpong-rtt-1k",
            2 * n,
            wall,
            samples,
            True,
            "two sequential client asks, not actor-to-actor pingpong",
        )

    def chain(self, scope):
        h = scope.create().actor
        n = self.p.count(1000, 12)
        start = rt.now_ns()
        for i in range(n):
            equal(rt.get(h.echo.remote(i)), i)
            h.echo.remote(i + 1)
        snapshot(h, 2 * n, n * n)
        wall = elapsed(start)
        self.retire(scope)
        self.record(
            "self-chain-ask-tell-1k",
            2 * n,
            wall,
            [],
            True,
            "client ask followed by tell; exact drain",
        )

    def storm(self, scope):
        n = self.p.count(100, 3)
        start = rt.now_ns()
        before = self.retired
        for _ in range(3):
            hs = scope.create_many(n, num_cpus=0)
            equal(
                rt.all_results([h.actor.echo.remote(i) for i, h in enumerate(hs)]),
                list(range(n)),
            )
            for i, h in enumerate(hs):
                snapshot(h.actor, 1, i)
            self.retire(scope)
        equal(self.retired - before, 3 * n)
        self.record(
            "spawn-stop-storm-300",
            6 * n,
            elapsed(start),
            [],
            True,
            f"all {3*n} actors checked; units echo+kill; every matching process exited",
        )

    def run(self):
        c, it = self.p.count, self.p.iterations
        with rt.Scope() as scope:
            warm = scope.create().actor
            equal(
                rt.all_results([warm.echo.remote(i) for i in range(c(500, 10))]),
                list(range(c(500, 10))),
            )
            snapshot(warm, c(500, 10), c(500, 10) * (c(500, 10) - 1) // 2)
            scope.retire()
            self.echo(scope, "seq-ask-echo-500", 1, c(500, 12))
            self.echo(scope, "conc-ask-echo-c8-m200", 8, c(200, 3))
            self.echo(scope, "conc-ask-echo-c16-m50", c(16, 4), c(50, 3))
            self.tell(scope, "tell-echo-5k", 1, c(5000, 24), None, 0)
            self.cpu(scope, "cpu-serial-100x200k", 1, c(100, 6), it(200_000), 0)
            self.cpu(
                scope, "cpu-parallel-4actors-200k", c(4, 2), c(25, 3), it(200_000), 1000
            )
            self.tell(scope, "flood-4k-tell", 4, c(1000, 6), None, 1)
            self.timeout(scope)
            self.stopped(scope)
            self.herd(scope)
            self.longrun(scope)
            self.monitored(scope, False)
            self.io(scope)
            self.monitored(scope, True)
            self.fifo(scope)
            self.chunked(scope)
            self.pingpong(scope)
            self.chain(scope)
            self.tell(
                scope, "slow-consumer-4prod-5k", 4, c(1250, 8), it(170_000), 10000
            )
            self.storm(scope)
            equal(len(self.rows), 20)
            equal(len(scope.owned), 0)
            print(
                f'SCENARIOS=20 RETIRED={self.retired} LIVE=0 MODE={"check" if self.p.small else "full"} SCALE={self.p.scale}',
                flush=True,
            )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["keys", "run"])
    parser.add_argument("mode", choices=["check", "full"])
    parser.add_argument("scale")
    parser.add_argument("path", type=Path)
    parser.add_argument("--session-dir", type=Path)
    parser.add_argument("--cpus", type=int, default=15)
    parser.add_argument("--object-store-bytes", type=int, default=1_500_000_000)
    args = parser.parse_args()
    plan = Plan(args.mode == "check", args.scale)
    if args.operation == "keys":
        args.path.write_text("".join(f"{n} {s}\n" for n, s in sorted(plan.keys())))
        return
    rt.require(args.session_dir is not None, "private session directory required")
    benchmark = Benchmark(plan, args.path)
    with rt.Session(args.session_dir, args.cpus, args.object_store_bytes):

        def interrupted(number, _frame):
            raise SystemExit(128 + number)

        signal.signal(signal.SIGTERM, interrupted)
        signal.signal(signal.SIGINT, interrupted)
        if plan.small:
            from bench_contracts import run_contracts

            run_contracts()
        benchmark.run()


if __name__ == "__main__":
    main()
