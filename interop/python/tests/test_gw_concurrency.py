"""DEV_08 网关并发模型测试：慢 handler 不阻塞收包循环。

两段：
  A（无 ray）：serve 的收包/派发结构验证——ray 模块以桩替换
    （RayWorker.remote(...).ask 远端调用折叠为本地线程内执行），
    断言：慢 ASK 不阻塞快 ASK / 心跳即时应答 / 8 并发 ASK 全配对。
  B（真 ray）：dispatcher handler 本身的并发行为（线程池并发执行）。

桩设计：真实 `@ray.remote class RayWorker` 产出的 RayWorker.remote(d)
→ ActorHandle，worker.ask.remote(k,p) → ObjectRef，ray.get(ref) 取值。
桩对等：remote(cls) → decorator 工厂返回类；cls.remote(*a) → 句柄；
handle.method.remote(*a) → _Call；ray.get(_Call) → 直接调用。
"""

from __future__ import annotations

import socket
import struct
import sys
import threading
import time
import types

import pytest

from parrot_protocol import wire
from parrot_protocol.ray_gw import ParrotDispatcher, serve


# ---------------- ray 桩（折叠远端调用为本地线程执行） ----------------


class _Call:
    def __init__(self, fn, args):
        self._fn, self._args = fn, args


class _Method:
    """worker.method.remote(*args) → _Call（对齐 ray ObjectRef 形态）。"""

    def __init__(self, fn):
        self._fn = fn

    def remote(self, *args):
        return _Call(self._fn, args)


class _Handle:
    def __init__(self, inst):
        self._inst = inst

    def __getattr__(self, name):
        return _Method(getattr(self._inst, name))


def _install_fake_ray(monkeypatch):
    fake_ray = types.ModuleType("ray")

    def remote_decorator(cls):
        def remote_ctor(*args, **kwargs):
            return _Handle(cls(*args, **kwargs))

        cls.remote = staticmethod(remote_ctor)
        return cls

    fake_ray.remote = remote_decorator
    fake_ray.is_initialized = lambda: True
    fake_ray.init = lambda **_: None
    fake_ray.get = lambda call: call._fn(*call._args)
    monkeypatch.setitem(sys.modules, "ray", fake_ray)


@pytest.fixture()
def gw(monkeypatch):
    _install_fake_ray(monkeypatch)
    d = ParrotDispatcher()

    @d.handler("bin:t:SlowPing")
    def slow_ping(_k, p):
        time.sleep(1.0)
        return ("bin:t:Pong", p)

    @d.handler("bin:t:FastPing")
    def fast_ping(_k, p):
        return ("bin:t:Pong", p)

    ports = []
    orig_print = print

    def cap(*args, **kw):
        s = " ".join(str(a) for a in args)
        if s.startswith("RAY_GW_PORT="):
            ports.append(int(s.split("=", 1)[1]))
        else:
            orig_print(*args, **kw)

    monkeypatch.setattr("builtins.print", cap)
    threading.Thread(target=lambda: serve(0, d), daemon=True).start()
    deadline = time.time() + 5
    while not ports and time.time() < deadline:
        time.sleep(0.005)
    monkeypatch.setattr("builtins.print", orig_print)
    assert ports, "gateway did not report port"
    return ports[0]


class Client:
    """帧收发（多线程共享单连接）。

    结构：专职读线程持续 recv→解码→入 pending 表并 notify；
    发送仅持短锁；等待者在 cond 上等自己的帧（他人帧留在表里
    由对应线程认领——绝不丢弃）。
    """

    def __init__(self, port: int):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.dec = wire.FrameDecoder()
        self.cond = threading.Condition()
        self.frames: list = []
        self.send_mu = threading.Lock()
        self.alive = True

        def reader():
            try:
                while self.alive:
                    chunk = self.s.recv(65536)
                    if not chunk:
                        break
                    with self.cond:
                        self.dec.feed(chunk)
                        while (f := self.dec.next_frame()) is not None:
                            self.frames.append(f)
                        self.cond.notify_all()
            except OSError:
                pass

        threading.Thread(target=reader, daemon=True).start()
        self.s.sendall(
            wire.build_frame(wire.FT_HANDSHAKE, 1, "", "__handshake__", wire.handshake_body("py-test"))
        )
        self._wait(lambda f: f.ft == wire.FT_HANDSHAKE_ACK)

    def _wait(self, pred, timeout=5.0):
        deadline = time.time() + timeout
        with self.cond:
            while True:
                for i, f in enumerate(self.frames):
                    if pred(f):
                        return self.frames.pop(i)
                remain = deadline - time.time()
                assert remain > 0, "wait timeout"
                self.cond.wait(remain)

    def ask(self, cid: int, key: str, payload: bytes):
        rt = wire.with_reply_to_prefix("py-test/lite", payload)
        with self.send_mu:
            self.s.sendall(wire.build_frame(wire.FT_ASK, cid, "/user/w", key, rt))
        return self._wait(lambda f: f.cid == cid)

    def heartbeat(self, cid: int):
        with self.send_mu:
            self.s.sendall(wire.build_frame(wire.FT_HEARTBEAT, cid, "", "", b""))
        return self._wait(lambda f: f.ft == wire.FT_HEARTBEAT_ACK)

    def close(self):
        self.alive = False
        self.s.close()


def test_slow_ask_does_not_block_fast(gw):
    c = Client(gw)
    results = {}

    def slow():
        results["slow"] = c.ask(100, "bin:t:SlowPing", struct.pack("<Q", 1))

    th = threading.Thread(target=slow, daemon=True)
    th.start()
    time.sleep(0.15)  # 慢 ask 已入队并占住一个 worker
    t0 = time.time()
    f = c.ask(101, "bin:t:FastPing", struct.pack("<Q", 2))
    dt = time.time() - t0
    assert f.ft == wire.FT_REPLY
    assert dt < 0.7, f"fast ask blocked by slow handler: {dt:.2f}s"
    assert th.join(timeout=3) or results.get("slow"), "slow ask never completed"
    c.close()


def test_heartbeat_during_slow_ask(gw):
    c = Client(gw)

    def slow():
        c.ask(200, "bin:t:SlowPing", b"x")

    threading.Thread(target=slow, daemon=True).start()
    time.sleep(0.15)
    t0 = time.time()
    c.heartbeat(201)
    dt = time.time() - t0
    assert dt < 0.7, f"heartbeat blocked by slow handler: {dt:.2f}s"
    c.close()


def test_eight_concurrent_asks_all_paired(gw):
    c = Client(gw)
    got: dict[int, object] = {}
    lock = threading.Lock()

    def one(cid: int):
        f = c.ask(cid, "bin:t:FastPing", struct.pack("<Q", cid))
        with lock:
            got[cid] = f

    threads = [threading.Thread(target=one, args=(300 + i,)) for i in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=5)
    assert len(got) == 8, f"paired {len(got)}/8"
    for cid, f in got.items():
        assert f.ft == wire.FT_REPLY
        assert struct.unpack("<Q", f.payload)[0] == cid
    c.close()
