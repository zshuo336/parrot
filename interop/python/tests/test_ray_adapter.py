"""ParrotDispatcher 行为测试（不启 socket——ray init 本地模式）。"""

import struct

import pytest

ray = pytest.importorskip("ray")

from parrot_protocol.ray_gw import ParrotDispatcher  # noqa: E402


@pytest.fixture(scope="module")
def ray_ctx():
    if not ray.is_initialized():
        ray.init(num_cpus=2, include_dashboard=False, log_to_driver=False)
    yield
    # module 级不 shutdown（跨测试复用）


def test_dispatch_routes_by_type_key(ray_ctx):
    d = ParrotDispatcher()

    @d.handler("bin:t::Ping")
    def ping(_k, p):
        (n,) = struct.unpack("<Q", p)
        return ("bin:t::Pong", struct.pack("<Q", n + 2))

    rk, rp = d.dispatch("bin:t::Ping", struct.pack("<Q", 40))
    assert rk == "bin:t::Pong"
    assert struct.unpack("<Q", rp)[0] == 42


def test_dispatch_unknown_key_raises(ray_ctx):
    d = ParrotDispatcher()
    with pytest.raises(KeyError):
        d.dispatch("bin:missing::X", b"")


def test_dispatcher_as_ray_actor(ray_ctx):
    """ParrotDispatcher 在 ray.remote worker 内可用（E4 转正核心形态）。"""

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
    rk, rp = ray.get(w.ask.remote("bin:t::Add", struct.pack("<QQ", 3, 4)))
    assert rk == "bin:t::AddR"
    assert struct.unpack("<Q", rp)[0] == 1007


def test_ray_1000_tasks(ray_ctx):
    """06 §3.4.3 门禁：1000 并行 pb 任务派发，总耗时 vs 纯 ray 损耗 <15%。

    公平基线：等形态 actor 调用（网关即 actor——dispatch 层才是被测增量）。
    """
    import time

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

        def ask_pure(self, p: bytes):
            (v,) = struct.unpack("<Q", p)
            return struct.pack("<Q", v * 2)

    w = W.remote(d)

    # dispatch 路径（经 TYPE_KEY 分发）
    t0 = time.perf_counter()
    tasks = [w.ask.remote("bin:t::N", struct.pack("<Q", i)) for i in range(1000)]
    for t in tasks:
        ray.get(t)
    t_dispatch = time.perf_counter() - t0

    # 基线：同 actor 无 dispatch（等序列化/等调用形态）
    t0 = time.perf_counter()
    tasks = [w.ask_pure.remote(struct.pack("<Q", i)) for i in range(1000)]
    for t in tasks:
        ray.get(t)
    t_pure = time.perf_counter() - t0

    overhead = (t_dispatch - t_pure) / t_pure
    print(f"\ndispatch={t_dispatch:.3f}s pure={t_pure:.3f}s overhead={overhead:.1%}")
    assert overhead < 0.15, f"dispatch overhead {overhead:.1%} exceeds 15% gate"
