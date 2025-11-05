"""ParrotDispatcher 行为测试（不启 socket——ray init 本地模式）。"""

import struct
import time

import pytest

from parrot_protocol.ray_gw import ParrotDispatcher  # noqa: E402
from parrot_protocol.ray_gw import _kill_and_confirm
from ray_test_cluster import owned_ray_cluster


@pytest.fixture(scope="module")
def ray_ctx():
    ray = pytest.importorskip("ray")
    with owned_ray_cluster(ray, label='adapter'):
        yield ray


def test_dispatch_routes_by_type_key():
    d = ParrotDispatcher()

    @d.handler("bin:t::Ping")
    def ping(_k, p):
        (n,) = struct.unpack("<Q", p)
        return ("bin:t::Pong", struct.pack("<Q", n + 2))

    rk, rp = d.dispatch("bin:t::Ping", struct.pack("<Q", 40))
    assert rk == "bin:t::Pong"
    assert struct.unpack("<Q", rp)[0] == 42


def test_dispatch_unknown_key_raises():
    d = ParrotDispatcher()
    with pytest.raises(KeyError):
        d.dispatch("bin:missing::X", b"")


def test_dispatcher_as_ray_actor(ray_ctx):
    """ParrotDispatcher 在 ray.remote worker 内可用（E4 转正核心形态）。"""

    ray = ray_ctx

    @ray.remote
    class W:
        def __init__(self, d: ParrotDispatcher):
            self._d = d

        def ask(self, k, p):
            return self._d.dispatch(k, p)

    d = ParrotDispatcher()

    @d.handler("bin:t::Add")
    def add(_k, p):
        a, b = struct.unpack("<QQ", p)
        return ("bin:t::AddR", struct.pack("<Q", a + b + 1000))

    w = W.remote(d)
    try:
        rk, rp = ray.get(w.ask.remote("bin:t::Add", struct.pack("<QQ", 3, 4)), timeout=10)
        assert rk == "bin:t::AddR"
        assert struct.unpack("<Q", rp)[0] == 1007
    finally:
        _kill_and_confirm(ray, w, w.__ray_ready__, time.monotonic() + 10)


def test_ray_1000_tasks(ray_ctx):
    """06 §3.4.3 门禁：1000 并行 raw bin 任务派发，总耗时 vs 纯 ray 损耗 <15%。

    公平基线：等形态 actor 调用（网关即 actor——dispatch 层才是被测增量）。
    """
    ray = ray_ctx
    import time
    import statistics

    d = ParrotDispatcher()

    @d.handler("bin:t::N")
    def n(_k, p):
        (v,) = struct.unpack("<Q", p)
        return ("bin:t::NR", struct.pack("<Q", v * 2))

    @ray.remote
    class W:
        def __init__(self, dd: ParrotDispatcher):
            self._dd = dd

        def ask(self, k, p):
            return self._dd.dispatch(k, p)

        def ask_pure(self, _key: str, p: bytes):
            (v,) = struct.unpack("<Q", p)
            return ("bin:t::NR", struct.pack("<Q", v * 2))

    w = W.remote(d)

    expected = [("bin:t::NR", struct.pack("<Q", i * 2)) for i in range(1000)]

    def run(method):
        started = time.perf_counter()
        results = ray.get([method.remote("bin:t::N", struct.pack("<Q", i)) for i in range(1000)], timeout=30)
        elapsed = time.perf_counter() - started
        assert results == expected, "every task must return the right value in input order"
        return elapsed

    try:
        # Both paths warm the same actor before timing; creation/scheduling and
        # different argument/return shapes are not dispatcher overhead.
        run(w.ask)
        run(w.ask_pure)
        ratios = []
        for pair in range(6):
            if pair % 2:
                pure, dispatch = run(w.ask_pure), run(w.ask)
            else:
                dispatch, pure = run(w.ask), run(w.ask_pure)
            ratios.append(dispatch / pure - 1)
        overhead = statistics.median(ratios)
        print(f"\npaired overheads={ratios}, median={overhead:.1%}")
        assert overhead < 0.15, f"dispatch overhead {overhead:.1%} exceeds 15% gate"
    finally:
        _kill_and_confirm(ray, w, w.__ray_ready__, time.monotonic() + 10)
