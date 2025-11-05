"""Production connection dispatch with a controlled in-process Ray adapter.

A blocked ASK must leave fast ASK and heartbeat service available. Eight
concurrent requests must retain their own correlation IDs and payloads.
All fixture connections and threads are owned and joined; future exceptions
fail the test. Actual Ray actor execution is covered in test_ray_adapter.py.
"""

from __future__ import annotations

import socket
import struct
import threading
import time
import types

import pytest
from concurrent.futures import ThreadPoolExecutor

from parrot_protocol import wire
from parrot_protocol.ray_gw import ParrotDispatcher


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


@pytest.fixture()
def gw():
    # Exercise the production per-connection service with an owned listener and
    # controllable handler; no daemon listener or global ray monkeypatch leaks.
    from parrot_protocol.ray_gw import _serve_connection_guarded
    entered, release = threading.Event(), threading.Event()
    dispatcher = ParrotDispatcher()

    @dispatcher.handler("bin:t:SlowPing")
    def slow_ping(_key, payload):
        entered.set()
        assert release.wait(5), "test must release the slow handler"
        return ("bin:t:Pong", payload)

    @dispatcher.handler("bin:t:FastPing")
    def fast_ping(_key, payload):
        return ("bin:t:Pong", payload)

    class Worker:
        def ask(self, path, key, payload):
            reply_key, reply = dispatcher.dispatch(key, payload)
            return dict(ok=True, key=reply_key, payload=reply)

    fake_ray = types.SimpleNamespace(get=lambda call: call._fn(*call._args))
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.settimeout(5)
    errors, connections = [], []

    def accept():
        try:
            sock, _ = listener.accept()
            connections.append(sock)
            _serve_connection_guarded(sock, dispatcher, _Handle(Worker()), fake_ray)
        except Exception as error:
            errors.append(error)

    thread = threading.Thread(target=accept, daemon=True)
    thread.start()
    try:
        yield listener.getsockname()[1], entered, release
    finally:
        release.set()
        for sock in connections:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        listener.close()
        thread.join(6)
        assert not thread.is_alive(), "connection service leaked"
        assert not errors, errors


class Client:
    """帧收发（多线程共享单连接）。

    结构：专职读线程持续 recv→解码→入 pending 表并 notify；
    发送仅持短锁；等待者在 cond 上等自己的帧（他人帧留在表里
    由对应线程认领——绝不丢弃）。
    """

    def __init__(self, port: int):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.s.settimeout(0.2)
        self.dec = wire.FrameDecoder()
        self.cond = threading.Condition()
        self.frames: list = []
        self.send_mu = threading.Lock()
        self.alive = True

        def reader():
            try:
                while self.alive:
                    try:
                        chunk = self.s.recv(65536)
                    except socket.timeout:
                        continue
                    if not chunk:
                        break
                    with self.cond:
                        self.dec.feed(chunk)
                        while (f := self.dec.next_frame()) is not None:
                            self.frames.append(f)
                        self.cond.notify_all()
            except OSError:
                pass
            finally:
                with self.cond:
                    self.alive = False
                    self.cond.notify_all()

        self.reader = threading.Thread(target=reader, daemon=True)
        self.reader.start()
        self.s.sendall(
            wire.build_frame(wire.FT_HANDSHAKE, 1, "", "__handshake__", wire.handshake_body("py-test"))
        )
        self._wait(lambda f: f.ft == wire.FT_HANDSHAKE_ACK)

    def _wait(self, pred, timeout=10.0):
        # timeout 10s：慢 handler 1s × 5 裕度——并行测试满载机器上
        # 线程调度可能拉长数倍（flaky 教训：5s 在 cargo 全测并行时偶超）
        deadline = time.time() + timeout
        with self.cond:
            while True:
                for i, f in enumerate(self.frames):
                    if pred(f):
                        return self.frames.pop(i)
                assert self.alive, "connection closed before reply"
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
        try:
            self.s.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.reader.join(2)
        self.s.close()
        assert not self.reader.is_alive()


@pytest.mark.parametrize("probe", ["ask", "heartbeat"])
def test_slow_ask_does_not_block_connection(gw, probe):
    port, entered, release = gw
    client = Client(port)
    try:
        with ThreadPoolExecutor(max_workers=1) as pool:
            slow = pool.submit(client.ask, 100, "bin:t:SlowPing", b"slow")
            try:
                assert entered.wait(2), "slow handler did not start"
                if probe == "ask":
                    frame = client.ask(101, "bin:t:FastPing", b"fast")
                    assert frame.ft == wire.FT_REPLY
                    assert frame.payload == b"fast"
                else:
                    frame = client.heartbeat(101)
                    assert frame.ft == wire.FT_HEARTBEAT_ACK
                assert not slow.done(), "probe must finish while slow handler is blocked"
            finally:
                release.set()
            response = slow.result(timeout=3)  # Thread exceptions must fail the test.
            assert response.ft == wire.FT_REPLY
            assert response.payload == b"slow"
    finally:
        release.set()
        client.close()


def test_eight_concurrent_asks_all_paired(gw):
    client = Client(gw[0])
    try:
        with ThreadPoolExecutor(max_workers=8) as pool:
            pending = {cid: pool.submit(client.ask, cid, "bin:t:FastPing", struct.pack("<Q", cid))
                       for cid in range(300, 308)}
            for cid, future in pending.items():
                frame = future.result(timeout=5)
                assert frame.ft == wire.FT_REPLY
                assert frame.cid == cid
                assert struct.unpack("<Q", frame.payload)[0] == cid
    finally:
        client.close()
