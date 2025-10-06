"""B3（DEV_09）：admin-v2 ray 方言契约测试。

三段：
  1. wire 编解码：docs/vectors/admin_v2.json 冻结向量逐字节 roundtrip
     （四方言共享事实源——Rust bincode ↔ Python 重实现互锁）
  2. 契约（无 ray 集群——桩替换 ray API）：网关帧级四命令全弧
     Deploy{PyModule} → parrot_entry 起命名 actor → Status/Stop/Drain
  3. 真 ray 子进程（ray 可导入时）：local init + 命名 actor 生命周期
"""

from __future__ import annotations

import json
import pathlib
import socket
import struct
import sys
import threading
import time
import types

import pytest

from parrot_protocol import admin_v2, wire
from parrot_protocol.ray_gw import ParrotDispatcher, RayAdminExecutor, serve

VECTORS = json.loads(
    (pathlib.Path(__file__).resolve().parents[3] / "docs" / "vectors" / "admin_v2.json").read_text()
)


# ================= 1. 冻结向量（wire 事实源互锁） =================


@pytest.mark.parametrize(
    "vec", VECTORS["vectors"], ids=[v["name"] for v in VECTORS["vectors"]]
)
def test_frozen_vector_roundtrip(vec):
    raw = bytes.fromhex(vec["payload_hex"])
    if raw[0] == admin_v2.TAG_ADMIN_CMD_V2:
        cmd = admin_v2.decode_admin_cmd_v2(raw)
        assert admin_v2.encode_admin_cmd_v2(cmd) == raw
    else:
        r = admin_v2.decode_admin_reply_v2(raw)
        assert admin_v2.encode_admin_reply_v2(r) == raw


def test_v2_deploy_cmd_known_bytes():
    # 首字节语义锚（与 Rust 侧 golden_deploy_cmd_stable_bytes 同款断言）
    raw = bytes.fromhex(
        next(v["payload_hex"] for v in VECTORS["vectors"] if v["name"] == "v2-deploy-props-singleton")
    )
    assert raw[0] == 0x03       # ADMIN_CMD_V2
    assert raw[1] == 0x00       # variant DeployComponent
    assert raw[2] == 0x01       # req_id varint = 1
    assert raw[3] == 0x04       # name len
    assert raw[4:8] == b"echo"


def test_varint_boundaries():
    # ≤250 单字节 / 0xFB+u16 / 0xFC+u32
    assert admin_v2.put_varint(0) == b"\x00"
    assert admin_v2.put_varint(250) == b"\xfa"
    assert admin_v2.put_varint(251) == b"\xfb\xfb\x00"
    assert admin_v2.put_varint(5000) == b"\xfb\x88\x13"
    for v in [0, 1, 250, 251, 5000, 65535, 65536, 1 << 32]:
        mv = memoryview(admin_v2.put_varint(v))
        got, off = admin_v2.read_varint(mv, 0)
        assert got == v and off == len(mv)


def test_bad_tag_rejected():
    with pytest.raises(ValueError):
        admin_v2.decode_admin_cmd_v2(b"\x99\x00")
    with pytest.raises(ValueError):
        admin_v2.decode_admin_reply_v2(b"\x03\x00")
    assert admin_v2.decode_tag(b"\x03\x00") == 0x03
    with pytest.raises(ValueError):
        admin_v2.decode_tag(b"\x01\x00")


# ================= 2. 契约测试（ray 桩——无集群） =================


class _StubHandle:
    """命名 actor 桩句柄：alive 标记 + kill 记录。"""

    def __init__(self, path, killed, drain_calls):
        self.path = path
        self.alive = True
        self._killed = killed
        self._drain_calls = drain_calls

    def parrot_drain_remote(self):
        self._drain_calls.append(self.path)


class _StubRay:
    """ray API 桩：kill/get/remote 三入口（RayAdminExecutor 依赖面）。"""

    def __init__(self):
        self.killed = []
        self.drain_calls = []

    def kill(self, h, no_restart=False):
        h.alive = False
        self.killed.append((h.path, no_restart))

    def get(self, ref):
        return ref() if callable(ref) else True

    def remote(self, fn):
        return lambda *a: lambda: fn(*a)


class _OutQueue:
    def __init__(self):
        self.frames = []
        self._cond = threading.Condition()

    def put(self, f):
        with self._cond:
            self.frames.append(f)
            self._cond.notify_all()

    def wait_reply(self, req_id, timeout=5.0):
        deadline = time.time() + timeout
        with self._cond:
            while True:
                for f in self.frames:
                    dec = wire.FrameDecoder()
                    # build_frame 产出 bytes——直接手工解包（单帧无粘包）
                    ft, cid, path, key, payload = _parse_frame(f)
                    if ft == wire.FT_SYSTEM_EVENT and payload[:1] == b"\x04":
                        r = admin_v2.decode_admin_reply_v2(payload)
                        if r["req_id"] == req_id:
                            return r
                remain = deadline - time.time()
                assert remain > 0, f"reply {req_id} timeout"
                self._cond.wait(remain)


def _parse_frame(buf: bytes):
    """单帧解包（测试便利——不走增量解码器）。"""
    dec = wire.FrameDecoder()
    dec.feed(buf)
    f = dec.next_frame()
    assert f is not None
    return f.ft, f.cid, f.path, f.type_key, f.payload


def _deploy_cmd(req_id, name, module, count=None, runtime_env=None):
    policy = {"kind": "singleton"} if count is None else {"kind": "pool", "count": count}
    return admin_v2.encode_admin_cmd_v2({
        "kind": "deploy", "req_id": req_id,
        "component": {
            "name": name, "version": "2.0.0",
            "artifact": {"kind": "pymodule", "module": module, "runtime_env": runtime_env},
            "instances": policy, "config": None,
        },
    })


@pytest.fixture()
def exec_env(monkeypatch):
    """执行器 + 桩 ray + 测试 module（parrot_entry 起 stub 命名 actor）。"""
    stub_ray = _StubRay()
    out = _OutQueue()
    ex = RayAdminExecutor(out, ray_module=stub_ray)

    # 测试 module：parrot_entry(ctx) → stub 句柄（真 ray 段换成真 actor）
    mod = types.ModuleType("parrot_test_jobs")
    def parrot_entry(ctx):
        return _StubHandle(ctx["path"], stub_ray.killed, stub_ray.drain_calls)
    mod.parrot_entry = parrot_entry
    monkeypatch.setitem(sys.modules, "parrot_test_jobs", mod)
    return ex, out, stub_ray


def _dispatch_sync(ex: RayAdminExecutor, cmd_bytes: bytes, reply_to="parrot://n1/_admin"):
    """同步等回帧（绕开 on_frame 的线程异步——测试确定性）。"""
    from parrot_protocol.admin_v2 import decode_admin_cmd_v2

    cmd = decode_admin_cmd_v2(cmd_bytes)
    out = ex._out
    ex._dispatch(cmd, reply_to)
    return out.wait_reply(cmd["req_id"])


def test_deploy_pymodule_singleton(exec_env):
    ex, out, stub = exec_env
    r = _dispatch_sync(ex, _deploy_cmd(1, "echo", "parrot_test_jobs"))
    assert r["kind"] == "deployed"
    assert r["instances"] == ["/user/echo"]
    # 登记表就位（status 可见）
    r2 = _dispatch_sync(ex, admin_v2.encode_admin_cmd_v2(
        {"kind": "status", "req_id": 2, "path_prefix": "/user/echo"}))
    assert r2["kind"] == "status"
    assert r2["states"][0]["path"] == "/user/echo"
    assert r2["states"][0]["version"] == "2.0.0"
    assert r2["states"][0]["state"] == "running"


def test_deploy_pool_multi_instance(exec_env):
    ex, out, stub = exec_env
    r = _dispatch_sync(ex, _deploy_cmd(3, "calc", "parrot_test_jobs", count=3))
    assert r["instances"] == ["/user/calc-0", "/user/calc-1", "/user/calc-2"]
    r2 = _dispatch_sync(ex, admin_v2.encode_admin_cmd_v2(
        {"kind": "status", "req_id": 4, "path_prefix": "/user/calc"}))
    assert len(r2["states"]) == 3


def test_deploy_dialect_mismatch(exec_env):
    ex, out, stub = exec_env
    cmd = admin_v2.encode_admin_cmd_v2({
        "kind": "deploy", "req_id": 5,
        "component": {
            "name": "x", "version": "1",
            "artifact": {"kind": "beam", "app": "frontier"},
            "instances": {"kind": "singleton"}, "config": None,
        },
    })
    r = _dispatch_sync(ex, cmd)
    assert r["kind"] == "failed"
    assert r["code"] == admin_v2.ERR_DIALECT_MISMATCH


def test_deploy_missing_entry_point(exec_env, monkeypatch):
    ex, out, stub = exec_env
    bad = types.ModuleType("parrot_no_entry")
    monkeypatch.setitem(sys.modules, "parrot_no_entry", bad)
    r = _dispatch_sync(ex, _deploy_cmd(6, "y", "parrot_no_entry"))
    assert r["kind"] == "failed"
    assert "parrot_entry" in r["detail"]


def test_stop_kills_named_actors(exec_env):
    ex, out, stub = exec_env
    _dispatch_sync(ex, _deploy_cmd(7, "echo", "parrot_test_jobs"))
    r = _dispatch_sync(ex, admin_v2.encode_admin_cmd_v2(
        {"kind": "stop", "req_id": 8, "path_prefix": "/user/echo"}))
    assert r["kind"] == "stopped"
    assert stub.killed == [("/user/echo", True)]
    # 停后 status → NOT_FOUND
    r2 = _dispatch_sync(ex, admin_v2.encode_admin_cmd_v2(
        {"kind": "status", "req_id": 9, "path_prefix": "/user/echo"}))
    assert r2["kind"] == "failed"
    assert r2["code"] == admin_v2.ERR_COMPONENT_NOT_FOUND


def test_prefix_no_false_match(exec_env):
    ex, out, stub = exec_env
    _dispatch_sync(ex, _deploy_cmd(10, "comp", "parrot_test_jobs"))
    _dispatch_sync(ex, _deploy_cmd(11, "comp2", "parrot_test_jobs"))
    r = _dispatch_sync(ex, admin_v2.encode_admin_cmd_v2(
        {"kind": "stop", "req_id": 12, "path_prefix": "/user/comp"}))
    assert r["kind"] == "stopped"
    # /user/comp2 未被误停
    killed_paths = [p for p, _ in stub.killed]
    assert killed_paths == ["/user/comp"]


def test_drain_empty_mailbox(exec_env):
    ex, out, stub = exec_env
    _dispatch_sync(ex, _deploy_cmd(13, "echo", "parrot_test_jobs"))
    r = _dispatch_sync(ex, admin_v2.encode_admin_cmd_v2({
        "kind": "drain", "req_id": 14, "path_prefix": "/user/echo", "timeout_ms": 2000}))
    assert r["kind"] == "drained"
    assert (r["drained"], r["aborted"]) == (1, 0)


def test_status_not_found(exec_env):
    ex, out, stub = exec_env
    r = _dispatch_sync(ex, admin_v2.encode_admin_cmd_v2(
        {"kind": "status", "req_id": 15, "path_prefix": "/user/nope"}))
    assert r["kind"] == "failed"
    assert r["code"] == admin_v2.ERR_COMPONENT_NOT_FOUND


# ---- 帧级契约：FT_SYSTEM_EVENT 帧经网关 serve 全链 ----


class _Client:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.dec = wire.FrameDecoder()
        self.frames = []
        self.cond = threading.Condition()

        def reader():
            try:
                while True:
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
        self.s.sendall(wire.build_frame(
            wire.FT_HANDSHAKE, 1, "", "__handshake__", wire.handshake_body("py-test")))
        self._wait(lambda f: f.ft == wire.FT_HANDSHAKE_ACK)

    def _wait(self, pred, timeout=10.0):
        deadline = time.time() + timeout
        with self.cond:
            while True:
                for i, f in enumerate(self.frames):
                    if pred(f):
                        return self.frames.pop(i)
                remain = deadline - time.time()
                assert remain > 0, "wait timeout"
                self.cond.wait(remain)

    def admin(self, cmd_bytes: bytes, req_id: int, reply_to="parrot://py-test/_admin"):
        self.s.sendall(wire.build_frame(
            wire.FT_SYSTEM_EVENT, req_id, reply_to, "", cmd_bytes))
        return self._wait(
            lambda f: f.ft == wire.FT_SYSTEM_EVENT
            and f.payload[:1] == b"\x04"
            and admin_v2.decode_admin_reply_v2(f.payload)["req_id"] == req_id
        )

    def close(self):
        self.s.close()


@pytest.fixture()
def gw(monkeypatch):
    """ray 桩 + serve 全链网关（端口 0 → 实际端口）。"""
    stub_ray = _StubRay()
    real_ray = types.ModuleType("ray")

    def remote_decorator(cls):
        def remote_ctor(*args, **kwargs):
            inst = cls(*args, **kwargs)

            class _H:
                def __init__(self, inst):
                    self._inst = inst

                def __getattr__(self, name):
                    return getattr(self._inst, name)

            return _H(inst)

        cls.remote = staticmethod(remote_ctor)
        return cls

    real_ray.remote = remote_decorator
    real_ray.is_initialized = lambda: True
    real_ray.init = lambda **_: None
    real_ray.get = lambda x: x() if callable(x) else x
    real_ray.kill = stub_ray.kill
    monkeypatch.setitem(sys.modules, "ray", real_ray)

    mod = types.ModuleType("parrot_test_jobs")
    def parrot_entry(ctx):
        return _StubHandle(ctx["path"], stub_ray.killed, stub_ray.drain_calls)
    mod.parrot_entry = parrot_entry
    monkeypatch.setitem(sys.modules, "parrot_test_jobs", mod)

    ports = []
    orig_print = print

    def cap(*args, **kw):
        s = " ".join(str(a) for a in args)
        if s.startswith("RAY_GW_PORT="):
            ports.append(int(s.split("=", 1)[1]))
        else:
            orig_print(*args, **kw)

    monkeypatch.setattr("builtins.print", cap)
    d = ParrotDispatcher()
    threading.Thread(target=lambda: serve(0, d), daemon=True).start()
    deadline = time.time() + 5
    while not ports and time.time() < deadline:
        time.sleep(0.005)
    monkeypatch.setattr("builtins.print", orig_print)
    assert ports, "gateway did not report port"
    return ports[0], stub_ray


def test_frame_level_deploy_status_stop(gw):
    port, stub = gw
    c = _Client(port)
    f = c.admin(_deploy_cmd(21, "echo", "parrot_test_jobs"), 21)
    r = admin_v2.decode_admin_reply_v2(f.payload)
    assert r == {"kind": "deployed", "req_id": 21, "instances": ["/user/echo"]}

    f = c.admin(admin_v2.encode_admin_cmd_v2(
        {"kind": "status", "req_id": 22, "path_prefix": "/user/echo"}), 22)
    r = admin_v2.decode_admin_reply_v2(f.payload)
    assert r["states"][0]["path"] == "/user/echo"

    f = c.admin(admin_v2.encode_admin_cmd_v2(
        {"kind": "stop", "req_id": 23, "path_prefix": "/user/echo"}), 23)
    r = admin_v2.decode_admin_reply_v2(f.payload)
    assert r["kind"] == "stopped"
    c.close()


def test_frame_level_non_admin_sys_event_ignored(gw):
    """非 admin-v2 tag 的 SYSTEM_EVENT（如 gossip 0x05）——网关吞帧不崩。"""
    port, stub = gw
    c = _Client(port)
    # 发一个 0x05 tag（membership gossip 段）——on_frame 返回 False（未消费）
    c.s.sendall(wire.build_frame(wire.FT_SYSTEM_EVENT, 1, "", "", b"\x05garbage"))
    # 网关仍活着：deploy 正常
    f = c.admin(_deploy_cmd(24, "echo2", "parrot_test_jobs"), 24)
    r = admin_v2.decode_admin_reply_v2(f.payload)
    assert r["kind"] == "deployed"
    c.close()


# ================= 3. 真 ray 子进程（集群本地形态） =================

ray = pytest.importorskip("ray")


@pytest.fixture(scope="module")
def ray_ctx():
    if not ray.is_initialized():
        ray.init(num_cpus=2, include_dashboard=False, log_to_driver=False)
    yield


def _make_real_entry(r):
    """真 ray parrot_entry 工厂（保留形态——后续 job submission 段复用）。"""
    def parrot_entry(ctx):
        cls = r.remote(_RealEntryActor)
        return cls.remote(ctx["path"])
    return parrot_entry


class _RealEntryActor:
    """真 ray 命名 actor 桩（deploy 目标——parrot_entry 产出形态）。"""

    def __init__(self, path: str):
        self._path = path

    def ping(self, n: int) -> int:
        return n + 5  # ray 方言辨识值

    def parrot_drain(self) -> bool:
        return True


def test_real_ray_deploy_lifecycle(ray_ctx, monkeypatch):
    """真 ray：parrot_entry 返回真 actor handle → kill/探活全链。"""
    import ray as r
    from ray.cloudpickle import register_pickle_by_value

    mod = types.ModuleType("parrot_real_jobs")
    register_pickle_by_value(sys.modules[__name__])  # 测试模块按值序列化
    def parrot_entry(ctx):
        cls = r.remote(_RealEntryActor)
        return cls.remote(ctx["path"])
    mod.parrot_entry = parrot_entry
    monkeypatch.setitem(sys.modules, "parrot_real_jobs", mod)

    out = _OutQueue()
    ex = RayAdminExecutor(out)

    r1 = _dispatch_sync(ex, _deploy_cmd(31, "recho", "parrot_real_jobs"))
    assert r1["kind"] == "deployed"
    assert r1["instances"] == ["/user/recho"]

    h = ex._components["recho"]["handles"][0]
    got = r.get(h.ping.remote(100))
    assert got == 105, "真 ray actor 行为（+5 方言辨识）"

    r2 = _dispatch_sync(ex, admin_v2.encode_admin_cmd_v2(
        {"kind": "stop", "req_id": 32, "path_prefix": "/user/recho"}))
    assert r2["kind"] == "stopped"
    # kill 后 actor 不再响应（ray.kill 语义验证）
    time.sleep(0.3)
    with pytest.raises(Exception):
        r.get(h.ping.remote(1), timeout=5)

