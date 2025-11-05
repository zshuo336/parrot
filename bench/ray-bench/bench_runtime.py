"""Owned local Ray actors and fail-fast, bounded driver operations."""

from __future__ import annotations
import asyncio
import concurrent.futures as cf
from dataclasses import dataclass
import os
from pathlib import Path
import threading
import time

import psutil
import ray

MASK = (1 << 64) - 1
BUSINESS_SECONDS = 3600
CLEANUP_SECONDS = 30
_worker = threading.local()


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def now_ns():
    # A system-wide POSIX epoch, including Python 3.9 on macOS.
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC)


def segment(value, start, end):
    for i in range(start, end):
        value = ((value * 6364136223846793005 + 1442695040888963407) & MASK) ^ i
    return value


def burn_cpu(iterations, salt):
    return segment((salt | 1) & MASK, 0, iterations)


def get(ref, seconds=BUSINESS_SECONDS):
    deadline = time.monotonic() + seconds
    while True:
        cancellation = getattr(_worker, "cancellation", None)
        if cancellation is not None and cancellation.is_set():
            raise cf.CancelledError("driver worker cancelled")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Ray reply deadline")
        ready, _ = ray.wait([ref], num_returns=1, timeout=min(0.05, remaining))
        if ready:
            return ray.get(ready[0], timeout=0)


def all_results(refs, seconds=BUSINESS_SECONDS, observer=None):
    """Collect whichever task finishes first, propagating any failure immediately."""
    pending = {ref: i for i, ref in enumerate(refs)}
    require(len(pending) == len(refs), "duplicate result reference")
    result = [None] * len(refs)
    deadline = time.monotonic() + seconds
    while pending:
        if observer is not None:
            observer()
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Ray group deadline")
        ready, _ = ray.wait(list(pending), num_returns=1, timeout=min(0.05, remaining))
        for ref in ready:
            result[pending.pop(ref)] = ray.get(ref, timeout=0)
    return result


def parallel(workers, task):
    cancellation = threading.Event()
    pool = cf.ThreadPoolExecutor(
        max_workers=workers, thread_name_prefix="ray-bench-driver"
    )

    def run(index):
        _worker.cancellation = cancellation
        try:
            return task(index)
        finally:
            del _worker.cancellation

    futures = [pool.submit(run, i) for i in range(workers)]
    try:
        done, pending = cf.wait(
            futures, timeout=BUSINESS_SECONDS, return_when=cf.FIRST_EXCEPTION
        )
        for future in done:
            future.result()
        require(not pending, "driver deadline")
        return [f.result() for f in futures]
    finally:
        cancellation.set()
        for future in futures:
            future.cancel()
        # All owned tasks use cancellable get() or finite local submission loops.
        pool.shutdown(wait=True, cancel_futures=True)


class Identity:
    def clock(self):
        return now_ns()

    def identity(self):
        return os.getpid(), psutil.Process().create_time()


@ray.remote(num_cpus=0, max_concurrency=1000, max_restarts=0)
class Gate(Identity):
    def __init__(self):
        self.entered = asyncio.Event()
        self.released = asyncio.Event()

    async def enter(self):
        self.entered.set()

    async def wait_entered(self):
        await self.entered.wait()

    async def wait_release(self):
        await self.released.wait()

    async def release(self):
        self.released.set()


@ray.remote(num_cpus=1, max_restarts=0, max_task_retries=0)
class BenchActor(Identity):
    def __init__(self):
        self.ops = 0
        self.checksum = 0
        self.first_cpu_ns = None
        self.last_cpu_ns = None

    def finish(self, value):
        self.ops += 1
        self.checksum = (self.checksum + value) & MASK
        return value

    def echo(self, value):
        return self.finish(value)

    def cpu(self, iterations, salt):
        if self.first_cpu_ns is None:
            self.first_cpu_ns = now_ns()
        value = burn_cpu(iterations, salt)
        self.last_cpu_ns = now_ns()
        return self.finish(value)

    def gate(self, gate):
        get(gate.enter.remote())
        get(gate.wait_release.remote())

    def snapshot(self):
        return self.ops, self.checksum, self.first_cpu_ns, self.last_cpu_ns

    def fail(self):
        raise ValueError("injected actor task failure")


@ray.remote(num_cpus=0, max_concurrency=1000, max_restarts=0)
class IOActor(Identity):
    def __init__(self):
        self.lock = asyncio.Lock()
        self.offered = 0
        self.active = 0
        self.peak = 0
        self.ops = 0
        self.checksum = 0

    async def io(self, milliseconds, value, gate=None):
        self.offered += 1
        async with self.lock:
            self.active += 1
            self.peak = max(self.peak, self.active)
            try:
                if gate is not None:
                    await gate.enter.remote()
                    await gate.wait_release.remote()
                await asyncio.sleep(milliseconds / 1000)
                self.ops += 1
                self.checksum = (self.checksum + value) & MASK
                return value
            finally:
                self.active -= 1

    async def snapshot(self):
        return self.ops, self.checksum, self.offered, self.active, self.peak


@ray.remote(num_cpus=1, max_concurrency=1000, max_restarts=0)
class ChunkActor(Identity):
    def __init__(self):
        self.busy = False
        self.ops = 0
        self.checksum = 0
        self.position = 0
        self.done = False

    def finish(self, value):
        self.ops += 1
        self.checksum = (self.checksum + value) & MASK
        return value

    async def run(self, total, size, salt, gate):
        require(
            not self.busy and total > 0 and size > 0, "invalid/concurrent chunk job"
        )
        self.busy = True
        self.done = False
        value = (salt | 1) & MASK
        self.position = 0
        try:
            while self.position < total:
                end = min(total, self.position + size)
                value = segment(value, self.position, end)
                self.position = end
                if self.position == min(total, size):
                    await gate.enter.remote()
                    await gate.wait_release.remote()
                await asyncio.sleep(0)
            self.done = True
            return self.finish(value)
        finally:
            self.busy = False

    async def probe(self, iterations, salt):
        value = self.finish(burn_cpu(iterations, salt))
        return value, self.position, self.done

    async def snapshot(self):
        return self.ops, self.checksum


def alive(identity):
    pid, created = identity
    try:
        process = psutil.Process(pid)
        return (
            process.create_time() == created
            and process.is_running()
            and process.status() != psutil.STATUS_ZOMBIE
        )
    except psutil.NoSuchProcess:
        return False


def child_identities():
    result = []
    for process in psutil.Process().children(recursive=True):
        try:
            result.append((process.pid, process.create_time()))
        except (psutil.NoSuchProcess, psutil.ZombieProcess):
            # A child that exited during enumeration needs no further wait.
            continue
    return result


@dataclass
class Owned:
    actor: object
    identity: tuple | None = None
    retired: bool = False


class Scope:
    """Only local actors created by this scope may be killed or waited for."""

    def __init__(self):
        self.owned = []
        self.retired = 0

    def create_many(self, count, actor_class=BenchActor, **options):
        handles = []
        for _ in range(count):
            handle = Owned(actor_class.options(**options).remote())
            self.owned.append(handle)
            handles.append(handle)
        identities = all_results([h.actor.identity.remote() for h in handles])
        for handle, identity in zip(handles, identities):
            handle.identity = identity
        return handles

    def create(self, actor_class=BenchActor, **options):
        return self.create_many(1, actor_class, **options)[0]

    def retire(self, handles=None):
        targets = [
            h for h in (self.owned if handles is None else handles) if not h.retired
        ]
        for h in targets:
            ray.kill(h.actor, no_restart=True)
        deadline = time.monotonic() + CLEANUP_SECONDS
        for h in targets:
            # ray.kill is asynchronous. Observe process exit before issuing a NEW
            # request; a reply submitted earlier may already have been buffered.
            while h.identity is not None and alive(h.identity):
                require(time.monotonic() < deadline, "actor process did not exit")
                time.sleep(0.01)
            while True:
                ref = h.actor.identity.remote()
                try:
                    get(ref, max(0.001, deadline - time.monotonic()))
                except ray.exceptions.RayActorError:
                    break
                require(h.identity is None, "physically retired actor replied")
                require(time.monotonic() < deadline, "unready actor did not terminate")
                time.sleep(0.01)
            h.retired = True
            self.retired += 1
        self.owned[:] = [h for h in self.owned if not h.retired]

    def close(self):
        self.retire()

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()


class Session:
    def __init__(self, directory, cpus, object_store):
        self.directory = str(directory)
        self.cpus = cpus
        self.object_store = object_store
        self.processes = []
        self.previous_children = set()

    def __enter__(self):
        require(
            not ray.is_initialized(), "benchmark requires its own new local Ray session"
        )
        if tuple(map(int, ray.__version__.split(".")[:2])) >= (2, 59):
            token_path = os.environ.get("RAY_AUTH_TOKEN_PATH")
            require(
                os.environ.get("RAY_AUTH_MODE") == "token" and token_path is not None,
                "private Ray token configuration required; use run.sh",
            )
            require(
                Path(token_path)
                .resolve()
                .is_relative_to(Path(self.directory).resolve().parent),
                "Ray token must belong to the private session directory",
            )
            require(
                Path(token_path).stat().st_mode & 0o777 == 0o600,
                "Ray token permissions must be 0600",
            )
        self.previous_children = set(child_identities())
        try:
            ray.init(
                address="local",
                num_cpus=self.cpus,
                object_store_memory=self.object_store,
                include_dashboard=False,
                log_to_driver=False,
                _temp_dir=self.directory,
                runtime_env={
                    "env_vars": {
                        "PYTHONPATH": os.pathsep.join(
                            filter(
                                None,
                                [
                                    str(Path(__file__).resolve().parent),
                                    os.environ.get("PYTHONPATH"),
                                ],
                            )
                        )
                    }
                },
            )
            # Capture owned descendants while the driver and its cluster are still alive.
            self.processes = [
                p for p in child_identities() if p not in self.previous_children
            ]
            print(f"RAY_SESSION_READY driver_pid={os.getpid()}", flush=True)
            return self
        except BaseException:
            ray.shutdown()
            raise

    def __exit__(self, *_exc):
        try:
            self.processes.extend(
                p for p in child_identities() if p not in self.previous_children
            )
        finally:
            # Enumeration failure must not bypass shutdown.
            ray.shutdown()
        deadline = time.monotonic() + CLEANUP_SECONDS
        while any(alive(p) for p in self.processes):
            require(
                time.monotonic() < deadline, "owned Ray processes survived shutdown"
            )
            time.sleep(0.02)
        print(
            "RAY_SESSION_RETIRED processes=" + str(len(set(self.processes))), flush=True
        )
