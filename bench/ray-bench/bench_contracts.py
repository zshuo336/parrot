"""Actual Ray failure/ordering/lifetime contracts, never assert-dependent."""

import concurrent.futures as cf
import threading
import time
import ray
import bench_runtime as rt
from bench_plan import Plan
from ray_bench import equal, row, lat_stats, snapshot, Probes


def expect_error(kind, action):
    try:
        action()
    except kind:
        return
    raise RuntimeError("expected an exception, got a normal return")


def run_contracts():
    passed = []

    def check(name, action):
        action()
        passed.append(name)
        print("CONTRACT " + name + " PASS", flush=True)

    def literals():
        for n, s, v in [
            (0, 0, 1),
            (1, 0, 7806831264735756412),
            (2, 0, 9396908728118811418),
            (10, 7, 6404011227278016048),
            (64, 15, 2931105770476240399),
            (1, rt.MASK, 13525302890751722018),
        ]:
            equal(rt.burn_cpu(n, s), v)

    check("literal-kernel-vectors", literals)

    def early():
        equal(rt.burn_cpu(1, 3639755004476521255), 42)
        equal(rt.burn_cpu(2, 3639755004476521255), 10481999410520546992)

    check("42-does-not-end-work", early)

    def continuous():
        x = 7
        for i in range(0, 10, 2):
            x = rt.segment(x, i, i + 2)
        equal(x, 6404011227278016048)

    check("continuous-chunk-kernel", continuous)
    check(
        "false-row-rejected",
        lambda: expect_error(RuntimeError, lambda: row("bad", 1, 0.1, [], False)),
    )

    def walls():
        for value in [0, -1, float("nan"), float("inf")]:
            expect_error(RuntimeError, lambda: row("bad", 1, value, [], True))

    check("invalid-wall-rejected", walls)

    def ranks():
        s = lat_stats([20, 10])
        equal((s["p50"], s["p99"]), (10, 20))
        s = lat_stats(list(range(100, 0, -1)))
        equal((s["p50"], s["p90"], s["p99"], s["max"]), (50, 90, 99, 100))

    check("integer-nearest-rank", ranks)

    def scaling():
        equal(Plan(False, "85.9375").iterations(200_000), 2327)
        equal(Plan(False, "2").iterations(5), 2)
        equal(Plan(False, "2").iterations(7), 4)
        equal(Plan().iterations(6_000_000_000), 6_000_000_000)
        equal(Plan(True, "1000000").iterations(6_000_000_000), 5000)
        expect_error(ValueError, lambda: Plan(False, "0"))

    check("explicit-exact-scale-default-unscaled", scaling)
    check(
        "exception-object-is-not-a-thrown-error",
        lambda: expect_error(
            RuntimeError,
            lambda: expect_error(ValueError, lambda: ValueError("returned")),
        ),
    )

    def clock():
        with rt.Scope() as scope:
            h = scope.create().actor
            start = rt.now_ns()
            other = rt.get(h.clock.remote())
            end = rt.now_ns()
            rt.require(start <= other <= end, "cross-process clock epochs differ")

    check("system-wide-monotonic-clock", clock)

    def accounting():
        with rt.Scope() as scope:
            h = scope.create().actor
            equal(rt.get(h.cpu.remote(1, 0)), 7806831264735756412)
            equal(rt.get(h.cpu.remote(1, 0)), 7806831264735756412)
            snapshot(h, 2, 2 * 7806831264735756412)
            expect_error(RuntimeError, lambda: snapshot(h, 4, 2 * 7806831264735756412))
            expect_error(RuntimeError, lambda: snapshot(h, 2, 3))

    check("one-count-per-cpu-and-wrapping-checksum", accounting)

    def queued():
        with rt.Scope() as scope:
            h = scope.create().actor
            g = scope.create(rt.Gate).actor
            gate = h.gate.remote(g)
            rt.get(g.wait_entered.remote())
            h.echo.remote(3)
            h.echo.remote(8)
            tail = h.echo.remote(13)
            try:
                expect_error(
                    ray.exceptions.GetTimeoutError, lambda: ray.get(tail, timeout=0.001)
                )
            finally:
                rt.get(g.release.remote())
            rt.get(gate)
            equal(rt.get(tail), 13)
            snapshot(h, 3, 24)

    check("queued-timeout-not-cancellation-and-fifo-tail", queued)

    def stopped():
        with rt.Scope() as scope:
            h = scope.create()
            equal(rt.get(h.actor.echo.remote(19)), 19)
            scope.retire([h])
            rt.require(
                h.retired and not rt.alive(h.identity),
                "retirement not physically observed",
            )
            expect_error(
                ray.exceptions.RayActorError, lambda: rt.get(h.actor.echo.remote(20))
            )

    check("dead-actor-and-worker-process-exit", stopped)

    def io_serial():
        with rt.Scope() as scope:
            h = scope.create(rt.IOActor).actor
            g = scope.create(rt.Gate).actor
            first = h.io.remote(20, 4, g)
            rt.get(g.wait_entered.remote())
            second = h.io.remote(20, 9)
            third = h.io.remote(20, 12)
            deadline = time.monotonic() + 5
            while True:
                state = rt.get(h.snapshot.remote())
                if state[2] == 3:
                    break
                rt.require(time.monotonic() < deadline, "IO admission deadline")
                time.sleep(0.01)
            equal(state, (0, 0, 3, 1, 1))
            rt.require(
                not ray.wait([first, second, third], timeout=0)[0],
                "IO completed through held gate",
            )
            start = rt.now_ns()
            rt.get(g.release.remote())
            equal(rt.all_results([first, second, third]), [4, 9, 12])
            rt.require(rt.now_ns() - start >= 60_000_000, "IO sleeps overlapped")
            equal(snapshot(h, 3, 25), (3, 25, 3, 0, 1))

    check("async-io-admission-exclusive-delay-all-payloads", io_serial)

    def io_kill():
        with rt.Scope() as scope:
            h = scope.create(rt.IOActor)
            g = scope.create(rt.Gate).actor
            a = h.actor.io.remote(10, 4, g)
            rt.get(g.wait_entered.remote())
            b = h.actor.io.remote(10, 9)
            scope.retire([h])
            expect_error(ray.exceptions.RayActorError, lambda: rt.get(a))
            expect_error(ray.exceptions.RayActorError, lambda: rt.get(b))

    check("kill-active-and-queued-io", io_kill)

    def group_error():
        with rt.Scope() as scope:
            h = scope.create().actor
            other = scope.create().actor
            g = scope.create(rt.Gate).actor
            blocked = h.gate.remote(g)
            rt.get(g.wait_entered.remote())
            bad = other.fail.remote()
            start = time.monotonic()
            try:
                expect_error(
                    ray.exceptions.RayTaskError,
                    lambda: rt.all_results([blocked, bad], seconds=5),
                )
            finally:
                rt.get(g.release.remote())
                rt.get(blocked)
            rt.require(
                time.monotonic() - start < 5,
                "later failure hidden behind blocked earlier result",
            )

    check("group-failure-not-hidden-by-blocked-result", group_error)

    def worker_error():
        with rt.Scope() as scope:
            h = scope.create().actor
            g = scope.create(rt.Gate).actor
            blocked = h.gate.remote(g)
            rt.get(g.wait_entered.remote())
            entered = threading.Event()
            joined = threading.Event()

            def task(i):
                if i == 0:
                    entered.set()
                    try:
                        rt.get(blocked)
                    finally:
                        joined.set()
                else:
                    entered.wait()
                    raise ValueError("driver fault")

            try:
                expect_error(ValueError, lambda: rt.parallel(2, task))
                rt.require(joined.is_set(), "worker not joined")
            finally:
                rt.get(g.release.remote())
                rt.get(blocked)

    check("driver-worker-failure-cancels-and-joins", worker_error)

    def chunks():
        with rt.Scope() as scope:
            h = scope.create(rt.ChunkActor).actor
            g = scope.create(rt.Gate).actor
            job = h.run.remote(10, 2, 7, g)
            rt.get(g.wait_entered.remote())
            try:
                equal(rt.get(h.probe.remote(1, 0)), (7806831264735756412, 2, False))
                rt.require(
                    not ray.wait([job], timeout=0)[0], "probe waits for whole job"
                )
            finally:
                rt.get(g.release.remote())
            equal(rt.get(job), 6404011227278016048)
            snapshot(h, 2, 7806831264735756412 + 6404011227278016048)

    check("continuous-chunk-and-admitted-interleaved-probe", chunks)

    def chunk_kill():
        with rt.Scope() as scope:
            h = scope.create(rt.ChunkActor)
            g = scope.create(rt.Gate).actor
            job = h.actor.run.remote(10, 2, 7, g)
            rt.get(g.wait_entered.remote())
            scope.retire([h])
            expect_error(ray.exceptions.RayActorError, lambda: rt.get(job))

    check("kill-unfinished-chunk-job", chunk_kill)

    def probe_value():
        with rt.Scope() as scope:
            h = scope.create().actor
            expect_error(RuntimeError, lambda: Probes(h, None, 2))

    check("wrong-probe-payload-fails-and-joins", probe_value)

    def probe_death():
        with rt.Scope() as scope:
            h = scope.create()
            p = Probes(h.actor, None, 1)
            scope.retire([h])
            try:
                expect_error(
                    ray.exceptions.RayActorError, lambda: p.done.result(timeout=5)
                )
            finally:
                expect_error(ray.exceptions.RayActorError, p.close)

    check("running-probe-death-propagates", probe_death)

    def all_retired():
        with rt.Scope() as scope:
            hs = scope.create_many(6, num_cpus=0)
            equal(
                rt.all_results([h.actor.echo.remote(i) for i, h in enumerate(hs)]),
                list(range(6)),
            )
            scope.retire()
            equal(scope.retired, 6)
            rt.require(
                all(h.retired and not rt.alive(h.identity) for h in hs),
                "unobserved storm worker",
            )

    check("all-actors-physically-retired", all_retired)
    print("CONTRACTS=" + str(len(passed)), flush=True)
