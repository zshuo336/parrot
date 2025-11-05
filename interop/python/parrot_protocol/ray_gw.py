"""Parrot Ray Gateway: Wire 1.0 connections to one owned Ray runtime.

Admin deployment creates path-owned dispatchers or native Ray actor handles.
ASK selects /user/{name}[-{i}] (also /ray/user/... and full Parrot URIs).
TELL selects the same path without a Wire completion receipt.

CLI: python3 -m parrot_protocol.ray_gw [port] [parrot=host:port] [--trace-export-endpoint URL]
"""

from __future__ import annotations

from contextlib import ExitStack
import os
import queue
import socket
import struct
import sys
import threading
import time
import uuid

from .wire import (
    BODY_FIXED_OVERHEAD,
    ERR_PROTOCOL_VIOLATION,
    ERR_OVERLOADED,
    MAX_FRAME_LEN,
    DEFAULT_HANDSHAKE_FRAME_LEN,
    CAPS_BIN_PB,
    CAPS_MEASURED_METRICS,
    CAPS_TRACE_CONTEXT,
    FLAG_TRACE_CONTEXT,
    decode_handshake,
    FT_ASK,
    FT_ERROR,
    FT_HANDSHAKE,
    FT_HANDSHAKE_ACK,
    FT_HEARTBEAT,
    FT_HEARTBEAT_ACK,
    FT_REPLY,
    FT_REPLY_ERR,
    FT_ROUTE_HINT,
    FT_SYSTEM_EVENT,
    FT_TELL,
    FrameDecoder,
    build_frame,
    encode_err_payload,
    handshake_ack_body,
    handshake_body,
    parse_tlv,
    split_reply_to,
)
from .trace import capture_parent, transferred_scope

ERR_UNKNOWN_TYPE_KEY = 6


class UnknownTypeKeyError(KeyError):
    """Dispatcher lookup failed, distinct from a KeyError inside user code."""


# DEV_08：ASK 执行池大小（慢 handler 并发度；环境变量可覆盖——
# PARROT_RAY_GW_WORKERS=1 即退化为旧行为做 A/B 对照）
RAY_GW_ASK_WORKERS = int(os.environ.get("PARROT_RAY_GW_WORKERS", "8"))
if not 1 <= RAY_GW_ASK_WORKERS <= 128:
    raise ValueError("PARROT_RAY_GW_WORKERS must be between 1 and 128")

# ---------------- 观测五件套：网关级指标注册表 ----------------
# 全部链路（listen + 反拨）共用的线程安全计数器；进程启动时刻固定。
_GATEWAY_CAPABILITIES = CAPS_BIN_PB | CAPS_MEASURED_METRICS | CAPS_TRACE_CONTEXT


class _GatewayMetrics:
    """逐帧计数（feed）+ 快照（snapshot）——探针经 admin-v2 MetricsReport 拉取。"""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.epoch = str(uuid.uuid4())
        self.started_ms = int(time.time() * 1000)
        self._c = {
            "connections": 0,
            "handshakes_ok": 0, "handshakes_failed": 0,
            "asks_rx": 0, "tells_rx": 0,
            "replies_tx": 0, "reply_errs": 0,
            "bytes_rx": 0, "bytes_tx": 0, "heartbeats_rx": 0,
        }

    def bump(self, key: str, n: int = 1) -> None:
        with self._lock:
            self._c[key] = min((1 << 64) - 1, max(0, self._c.get(key, 0) + n))

    def received(self, frame) -> None:
        """Only fully decoded Wire frames, including all UTF-8 header bytes."""
        self.bump("bytes_rx", 4 + BODY_FIXED_OVERHEAD + len(frame.path.encode())
                  + len(frame.type_key.encode()) + len(frame.payload))
        counter = {FT_ASK: "asks_rx", FT_TELL: "tells_rx",
                   FT_HEARTBEAT: "heartbeats_rx"}.get(frame.ft)
        if counter is not None:
            self.bump(counter)

    def transmitted(self, frame: bytes) -> None:
        """Called only after sendall succeeds, never on enqueue/partial failure.

        Production writers receive already encoded frames. Ignore non-frame
        bytes used by low-level writer lifecycle fixtures.
        """
        if len(frame) < 4 + BODY_FIXED_OVERHEAD or int.from_bytes(frame[:4], "little") != len(frame) - 4:
            return
        self.bump("bytes_tx", len(frame))
        kind = frame[5]
        if kind in (FT_REPLY, FT_REPLY_ERR):
            self.bump("replies_tx")
        if kind == FT_REPLY_ERR:
            self.bump("reply_errs")

    def _sample(self) -> tuple[dict, bool]:
        import platform

        with self._lock:
            base = dict(self._c)
        rss = _resident_bytes()
        base.update({
            "ts_ms": int(time.time() * 1000),
            "runtime": f"python/{platform.python_version()}",
            "processes": threading.active_count(),
            "memory_rss": 0 if rss is None else rss,
            "uptime_start_ms": self.started_ms,
        })
        return base, rss is not None

    def snapshot(self) -> dict:
        return self._sample()[0]

    def measured(self) -> dict:
        from .admin_v2 import METRICS_ALL

        snapshot, rss_available = self._sample()
        available = METRICS_ALL & ~((1 << 12) | (1 << 13))
        if not rss_available:
            available &= ~(1 << 15)
        return {"snapshot": snapshot, "available": available, "epoch": self.epoch,
                "process_kind": "python_threads"}


def _resident_bytes() -> int | None:
    # Bounded, local RSS only; no Ray cluster RPC and no synthetic zero mask.
    try:
        with open("/proc/self/status", "rb") as source:
            data = source.read(65537)
        if len(data) > 65536:
            return None
        for line in data.decode("ascii").splitlines():
            if line.startswith("VmRSS:"):
                fields = line.split()
                if len(fields) != 3 or fields[2] != "kB":
                    return None
                value = int(fields[1]) * 1024
                return value if 0 <= value < 1 << 64 else None
    except (OSError, UnicodeError, ValueError):
        pass
    return None


class _ConnectionMetricLease:
    """One entered Wire handshake attempt, retired exactly once by its owner."""
    def __init__(self, metrics):
        self.metrics = metrics
        self.connected = False
        self.closed = False

    def complete(self):
        with self.metrics._lock:
            if not self.closed and not self.connected:
                self.connected = True
                for key in ("handshakes_ok", "connections"):
                    self.metrics._c[key] = min((1 << 64)-1, self.metrics._c[key] + 1)

    def close(self):
        with self.metrics._lock:
            if self.closed:
                return
            self.closed = True
            if self.connected:
                self.metrics._c["connections"] -= 1
            else:
                self.metrics._c["handshakes_failed"] = min((1 << 64)-1, self.metrics._c["handshakes_failed"] + 1)


METRICS = _GatewayMetrics()


# ---------------- B3（DEV_09）：admin-v2 ray 方言执行器 ----------------
# DeployComponent{PyModule} → worker 内 importlib 载入模块并调用
# parrot_entry(ctx)；独立执行器也支持返回 Ray actor handle。
# Drain/Stop → 按路径退休；Status → owner 最近确认的逐路径状态（不是独立探活）。


class _AskPermit:
    def __init__(self, owner, size):
        self._owner = owner
        self._size = size
        self._released = False

    def release(self, _future=None):
        with self._owner.lock:
            if not self._released:
                self._released = True
                self._owner.pending -= 1
                self._owner.bytes -= self._size
                self._owner.changed.notify_all()


class _AskAdmission:
    """One gateway-wide ledger, including running work on retired connections."""
    def __init__(self):
        self.lock = threading.Lock()
        self.changed = threading.Condition(self.lock)
        self._closed = False
        self.pending = 0
        self.bytes = 0

    def acquire(self, size):
        with self.lock:
            if self._closed or self.pending >= 256 or size > 16 * 1024 * 1024 - self.bytes:
                return None
            self.pending += 1
            self.bytes += size
            return _AskPermit(self, size)

    def seal(self):
        with self.lock:
            self._closed = True

    def wait(self, deadline):
        with self.changed:
            while self.pending:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError('gateway requests are still running')
                self.changed.wait(remaining)


class _ManagementQueue:
    """Gateway-owned FIFO, including the currently executing command.

    A worker is created only when idle work becomes available and exits when
    drained. All admissions and worker transitions use a short independent lock;
    a blocked factory cannot hold this lock or stall connection heartbeats.
    """
    def __init__(self):
        from collections import deque
        self._queue = deque()
        self._lock = threading.Lock()
        self._count = 0
        self._bytes = 0
        self._thread = None
        self._closed = False

    def submit(self, action, args, size):
        with self._lock:
            if self._closed or self._count >= 256 or size > 16 * 1024 * 1024 - self._bytes:
                return False
            self._queue.append((action, args, size))
            self._count += 1
            self._bytes += size
            if self._thread is None:
                thread = threading.Thread(target=self._run, name=f'parrot-management-{id(self):x}', daemon=True)
                self._thread = thread
                try:
                    thread.start()
                except Exception:
                    self._thread = None
                    self._queue.pop()
                    self._count -= 1
                    self._bytes -= size
                    return False
            return True

    def close(self, deadline):
        with self._lock:
            self._closed = True
            discarded = list(self._queue)
            self._queue.clear()
            self._count -= len(discarded)
            self._bytes -= sum(item[2] for item in discarded)
            thread = self._thread
        discarded.clear()  # Release callable/argument ownership outside the lock.
        if thread is not None:
            thread.join(max(0, deadline - time.monotonic()))
            if thread.is_alive():
                raise TimeoutError('gateway management operation is still running')

    def _run(self):
        while True:
            with self._lock:
                if not self._queue:
                    self._thread = None
                    return
                action, args, size = self._queue.popleft()
            try:
                action(*args)
            except BaseException as error:
                # A broken reply sink or user SystemExit must not strand all
                # other accepted commands. The executing slot still retires.
                try:
                    print(f'[ray-gw] management task failed: {_detail(error)}', file=sys.stderr, flush=True)
                except BaseException:
                    pass  # Diagnostic output cannot break ownership of the queue.
            finally:
                with self._lock:
                    self._count -= 1
                    self._bytes -= size
                del action, args


class RayAdminState:
    """Gateway-owned component registry, retained across connection retirement.

    Serialize lifecycle operations so a concurrent stop cannot erase a newer
    deployment. Create one owner per gateway, after serializing its dispatcher
    into the Ray worker; this lock never crosses the Ray process boundary.
    """

    def __init__(self) -> None:
        self.components: dict[str, dict] = {}
        self.lock = threading.RLock()
        self.ask_admission = _AskAdmission()
        self.management = _ManagementQueue()
        self.local_runtime = None
        # Immutable snapshots let connection admission reject uncertain/retiring
        # paths without waiting on a possibly blocked lifecycle operation.
        self.path_states = {}
        self.closing = False

    def route_error(self, path):
        if self.closing:
            return dict(ok=False, code=3, detail='gateway is closing')
        state = self.path_states.get(_logical_path(path))
        if state is not None and state != 'running':
            return dict(ok=False, code=3, detail='component is not running: ' + _logical_path(path))
        return None


def _registry_operation(method):
    from functools import wraps

    @wraps(method)
    def locked(self, *args, **kwargs):
        with self._state.lock:
            return method(self, *args, **kwargs)
    return locked


def _reply_boundary(method):
    """Keep response preparation failures owned by their network connection."""
    from functools import wraps

    @wraps(method)
    def guarded(self, *args, **kwargs):
        try:
            return method(self, *args, **kwargs)
        except BaseException:
            if self._on_reply_failure is None:
                raise  # Standalone sinks retain their explicit failure contract.
            self._on_reply_failure()
            return None
    return guarded


class RayAdminExecutor:
    """admin-v2 四命令的 ray 方言实现。

    组件登记表：name → {version, paths, handles}（进程内；网关单点）。
    实例路径与 parrot/erl/jvm 方言同规：/user/{name}[-{i}]——
    path_prefix 匹配规则与 Rust 侧一致（整段相等或后随 '-'）。
    """

    def __init__(self, out_queue, ray_module=None, gateway_dispatcher=None,
                 worker_ref=None, state: RayAdminState | None = None,
                 on_reply_failure=None) -> None:
        if on_reply_failure is not None and not callable(on_reply_failure):
            raise TypeError('on_reply_failure must be callable')
        self._on_reply_failure = on_reply_failure
        self._out = out_queue          # 回帧队列（writer 线程消费）
        self._ray = ray_module         # 可注入（测试桩）；None → import ray
        self._state = state if state is not None else RayAdminState()
        self._components = self._state.components
        self._lock = self._state.lock
        # 本地模式的 gateway handler；部署组件由独立 path runtime 持有。
        self._gw_dispatcher = gateway_dispatcher
        # 工厂必须在真实 worker 进程执行，不能修改 driver 内的序列化副本。
        self._worker = worker_ref

    # ---- ray 句柄（惰性导入；测试注入桩）----
    @property
    def ray(self):
        if self._ray is None:
            import ray  # noqa: PLC0415（网关进程内惰性——测试可先注入桩）
            self._ray = ray
        return self._ray

    # ---- 回帧 ----
    @_reply_boundary
    def _send_reply(self, req_id: int, reply: dict, reply_to: str) -> None:
        from .admin_v2 import encode_admin_reply_v2

        self._out.put(
            build_frame(FT_SYSTEM_EVENT, req_id, reply_to, "",
                        encode_admin_reply_v2(reply))
        )

    # ---- SYSTEM_EVENT 入口（serve 帧循环调用）----
    def on_frame(self, cid: int, reply_to: str, payload: bytes, *, encoded_size=None) -> bool:
        """SYSTEM_EVENT 帧 → admin v2 命令处理。返回 True = 已消费。"""
        from .admin_v2 import (
            TAG_ADMIN_CMD_V2,
            decode_admin_cmd_v2,
            decode_tag,
        )

        try:
            tag = decode_tag(payload)
        except ValueError:
            return False  # 非 admin-v2（gossip 等）——原路透传给调用方忽略
        if tag != TAG_ADMIN_CMD_V2:
            return False  # 回执帧不落在网关侧（发起方 pending 表消费）
        try:
            cmd = decode_admin_cmd_v2(payload)
            if cmd['req_id'] != cid:
                raise ValueError('admin request ID disagrees with frame CID')
        except ValueError:
            from .admin_v2 import ERR_DIALECT_MISMATCH
            self._send_reply(cid, dict(kind='failed', req_id=cid,
                code=ERR_DIALECT_MISMATCH, detail='invalid admin payload or request correlation'), reply_to)
            return True
        minimum_size = 4 + BODY_FIXED_OVERHEAD + len(reply_to.encode('utf-8')) + len(payload)
        size = minimum_size if encoded_size is None else max(minimum_size, encoded_size)
        if not self._state.management.submit(self._dispatch, (cmd, reply_to), size):
            self._send_reply(cmd['req_id'], dict(kind='failed', req_id=cmd['req_id'],
                code=ERR_OVERLOADED, detail='gateway management capacity or scheduler unavailable'), reply_to)
        return True

    @_reply_boundary
    def _dispatch(self, cmd: dict, reply_to: str) -> None:
        from .admin_v2 import (
            ERR_ARTIFACT_FETCH,
            ERR_COMPONENT_NOT_FOUND,
            ERR_DIALECT_MISMATCH,
            ERR_DRAIN_TIMEOUT,
        )

        kind = cmd["kind"]
        try:
            if kind == "deploy":
                reply = self.deploy(cmd["component"])
            elif kind == "drain":
                d, a = self.drain(cmd["path_prefix"], cmd["timeout_ms"])
                reply = {"kind": "drained", "drained": d, "aborted": a}
            elif kind == "stop":
                reply = self.stop(cmd["path_prefix"])
            elif kind == "status":
                reply = self.status(cmd["path_prefix"])
            elif kind == "metrics":
                reply = {"kind": "metrics", "snapshot": self.collect_metrics()}
            elif kind == "measured_metrics":
                reply = {"kind": "measured_metrics", "metrics": self.collect_measured_metrics()}
            else:  # pragma: no cover —— 解码层已限四形态
                reply = {"kind": "failed", "code": ERR_DIALECT_MISMATCH,
                         "detail": f"unknown kind {kind}"}
        except _NotFound as e:
            reply = {"kind": "failed", "code": ERR_COMPONENT_NOT_FOUND,
                     "detail": f"no component under {e}"}
        except BaseException as e:  # User code runs on an owned background worker.
            reply = {"kind": "failed", "code": ERR_ARTIFACT_FETCH, "detail": _detail(e)}
        reply["req_id"] = cmd["req_id"]
        self._send_reply(cmd["req_id"], reply, reply_to)

    # ---- 观测五件套：指标采集（admin-v2 MetricsReport）----
    @_registry_operation
    def collect_metrics(self) -> dict:
        return self._add_component_metrics(METRICS.snapshot())

    @_registry_operation
    def collect_measured_metrics(self) -> dict:
        metrics = METRICS.measured()
        self._add_component_metrics(metrics["snapshot"])
        metrics["available"] |= (1 << 12) | (1 << 13)
        return metrics

    def _add_component_metrics(self, snap: dict) -> dict:
        with self._lock:
            comps = list(self._components.items())
        states = []
        for name, info in comps:
            for p in info.get("paths", []):
                states.append({"path": p, "state": info["states"][p],
                               "version": info.get("version", "?")})
        snap["components"] = len(comps)
        snap["component_states"] = states
        return snap

    def _runtime(self):
        with self._state.lock:
            if self._state.local_runtime is None:
                self._state.local_runtime = _ComponentRuntime(self._gw_dispatcher or ParrotDispatcher(), self._ray)
                if self._gw_dispatcher is not None:
                    self._gw_dispatcher._component_runtime = self._state.local_runtime
            return self._state.local_runtime

    def _call(self, method, *args, deadline=None):
        if deadline is not None and time.monotonic() >= deadline:
            raise TimeoutError('component retirement deadline elapsed')
        if self._worker is None:
            return getattr(self._runtime(), method)(*args)
        ref = getattr(self._worker, method).remote(*args)
        if deadline is None:
            return self.ray.get(ref)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('component retirement deadline elapsed')
        return self.ray.get(ref, timeout=remaining)

    def _set_paths(self, name, info, paths, states):
        if not paths:
            self._components.pop(name, None)
        else:
            info['paths'] = list(paths)
            info['states'] = dict(states)
            if self._worker is None:
                info['handles'] = self._runtime().handles(paths)
            self._components[name] = info
        self._publish_path_states()

    def _publish_path_states(self):
        self._state.path_states = {p: info['states'][p]
                                   for info in self._components.values() for p in info['paths']}

    @_registry_operation
    def deploy(self, comp):
        from .admin_v2 import ERR_DIALECT_MISMATCH, ERR_ARTIFACT_FETCH, ERR_SPAWN_FAILED
        name = comp['name']
        if name in self._components:
            return dict(kind='failed', code=ERR_ARTIFACT_FETCH, detail=f'component {name} already deployed; stop it first')
        art = comp['artifact']
        if art['kind'] != 'pymodule':
            return dict(kind='failed', code=ERR_DIALECT_MISMATCH, detail='ray executor expects PyModule')
        try:
            paths = _expand_paths(name, comp['instances'])
            occupied = {p for info in self._components.values() for p in info['paths']}
            if occupied.intersection(paths):
                raise ValueError('component logical path collision')
            if len(occupied) + len(paths) > 4096:
                raise ValueError('gateway instance capacity exhausted')
            from .ray_artifacts import source_directory
            module_dir = source_directory(art['module'], art.get('uri'))
            from .ray_environments import parse_environment
            env = parse_environment(art.get('runtime_env'))
            if env and self._worker is None:
                raise ValueError('runtime_env requires an isolated production Ray worker')
        except Exception as error:
            return dict(kind='failed', code=ERR_SPAWN_FAILED, detail=_detail(error))
        info = dict(version=comp['version'], paths=paths, handles=[], states={p:'starting' for p in paths})
        self._components[name] = info
        self._publish_path_states()
        try:
            receipt = self._call('mount_module', name, art['module'], module_dir, env, paths,
                                 comp['version'], comp.get('config'))
            if not isinstance(receipt, dict) or type(receipt.get('ok')) is not bool:
                raise ValueError('invalid component mount receipt')
            retained = receipt['paths']
            if (not isinstance(retained, list) or any(not isinstance(p, str) for p in retained) or
                    len(set(retained)) != len(retained) or not set(retained).issubset(paths)):
                raise ValueError('invalid component mount paths')
            if receipt['ok'] and retained != paths:
                raise ValueError('incomplete successful component mount')
            self._set_paths(name, info, retained, {p:'running' if receipt['ok'] else 'stop_failed' for p in retained})
            if not receipt['ok']:
                return dict(kind='failed', code=ERR_SPAWN_FAILED, detail=_detail(receipt.get('detail', 'component startup failed')))
        except Exception as error:
            # A failed RPC is not proof that the worker did nothing. Retain all
            # requested paths so an explicit Stop can retire an uncertain commit.
            self._set_paths(name, info, paths, {p:'deploy_failed' for p in paths})
            return dict(kind='failed', code=ERR_SPAWN_FAILED, detail=_detail(error))
        return dict(kind='deployed', instances=paths)

    def _retire_selected(self, name, info, selected, deadline=None):
        info['states'].update({p: 'retiring' if deadline is None else 'draining' for p in selected})
        self._publish_path_states()
        try:
            millis = None if deadline is None else max(0, int((deadline-time.monotonic())*1000))
            result = self._call('retire_paths', name, selected, millis, deadline=deadline)
            if (not isinstance(result, dict) or set(result) != set(selected) or
                    any(value not in (None, 'stop_failed', 'drain_failed') for value in result.values())):
                raise ValueError('invalid component retirement receipt')
        except Exception:
            state = 'stop_failed' if deadline is None else 'drain_failed'
            info['states'].update({p:state for p in selected})
            self._publish_path_states()
            raise
        retired = {p for p, state in result.items() if state is None}
        states = {p:result.get(p, info['states'][p]) for p in info['paths'] if p not in retired}
        self._set_paths(name, info, [p for p in info['paths'] if p not in retired], states)
        return len(retired), len(selected)-len(retired)

    @_registry_operation
    def stop(self, prefix):
        matched = self._match(prefix)
        if not matched:
            raise _NotFound(prefix)
        failure = None
        for name, info, selected in matched:
            try:
                _, remaining = self._retire_selected(name, info, selected)
                if remaining:
                    failure = RuntimeError('component stop incomplete; handles retained')
            except Exception as error:
                failure = error
        if failure is not None:
            raise failure
        return dict(kind='stopped')

    @_registry_operation
    def drain(self, prefix, timeout_ms):
        matched = self._match(prefix)
        if not matched:
            raise _NotFound(prefix)
        deadline = time.monotonic() + min(timeout_ms, 60000) / 1000
        drained = aborted = 0
        for name, info, selected in matched:
            try:
                done, pending = self._retire_selected(name, info, selected, deadline)
                drained += done
                aborted += pending
            except Exception:
                aborted += len(selected)
        return drained, aborted

    @_registry_operation
    def status(self, prefix):
        matched = self._match(prefix)
        if not matched:
            raise _NotFound(prefix)
        return dict(kind='status', states=[dict(path=p, state=info['states'][p], version=info['version'])
                    for _, info, selected in matched for p in selected])

    def _match(self, prefix):
        return [(name, info, [p for p in info['paths'] if _matches(p, prefix)])
                for name, info in self._components.items() if any(_matches(p, prefix) for p in info['paths'])]



def _is_dispatcher(h) -> bool:
    """组件句柄是否为 dispatcher 形态（鸭子判定）。

    网关以 `python3 -m parrot_protocol.ray_gw` 启动时运行模块为
    `__main__`，而 app 组件 `from parrot_protocol.ray_gw import
    ParrotDispatcher` 会二次导入同名模块——两份类对象 isinstance
    恒 False。改按结构判定（dispatch/mount/_handlers）。
    """
    return (
        hasattr(h, "dispatch")
        and callable(getattr(h, "dispatch", None))
        and hasattr(h, "_handlers")
    )


class _NotFound(Exception):
    """组件未部署（Stop/Status/Drain 找不到实例 → 0x0A03）。"""


def _expand_paths(name: str, policy: dict) -> list[str]:
    if not _valid_name(name):
        raise ValueError('invalid component name')
    if policy['kind'] == 'singleton':
        return [f'/user/{name}']
    if policy['kind'] not in ('pool', 'sharded'):
        raise ValueError('unknown instance policy')
    count = policy['count']
    if type(count) is not int or not 1 <= count <= 256:
        raise ValueError('instance count must be between 1 and 256')
    return [f'/user/{name}-{i}' for i in range(count)]


class ParrotDispatcher:
    """One dispatcher's type-key handlers, with optional explicit composition.

    Admin deployment does not mount components into a global type-key search:
    it owns each dispatcher at an exact logical path in _ComponentRuntime.
    Standalone callers use dispatch_path for admin-deployed components.
    mount/unmount remain explicit in-process composition helpers.
    """

    def __init__(self) -> None:
        self._handlers: dict[str, object] = {}
        self._mounted: list["ParrotDispatcher"] = []

    def handler(self, type_key: str):
        def deco(fn):
            self._handlers[type_key] = fn
            return fn

        return deco

    def mount(self, sub: "ParrotDispatcher") -> None:
        """显式组合本地 dispatcher；同一对象重复挂载幂等。"""
        if sub not in self._mounted:
            self._mounted.append(sub)

    def unmount(self, sub: "ParrotDispatcher") -> None:
        """解除显式本地组合；不改变管理组件生命周期。"""
        if sub in self._mounted:
            self._mounted.remove(sub)

    def dispatch_path(self, path, type_key, payload):
        runtime = getattr(self, '_component_runtime', None)
        if runtime is None:
            raise KeyError('no component runtime')
        result = runtime.ask(path, type_key, payload)
        if not result['ok']:
            raise KeyError(result['detail'])
        return result['key'], result['payload']

    def dispatch(self, type_key: str, payload: bytes, *, trace_recorder=None,
                 trace_path='', trace_operation='ask') -> tuple[str, bytes]:
        # 已部署组件优先（业务键）——网关内置探针兜底
        for sub in reversed(self._mounted):
            fn = sub._handlers.get(type_key)
            if fn is not None:
                return _recorded_handler(fn, type_key, payload, trace_recorder, trace_path, trace_operation)
        fn = self._handlers.get(type_key)
        if fn is None:
            raise UnknownTypeKeyError(f"unknown type key {type_key}")
        return _recorded_handler(fn, type_key, payload, trace_recorder, trace_path, trace_operation)


def _recorded_handler(fn, key, payload, recorder, path, operation):
    if recorder is None:
        return fn(key, payload)
    from .trace import TraceActor
    from .trace_recording import handler_scope
    with handler_scope(recorder, path, key, operation):
        return TraceActor._trace_sync_result(fn(key, payload))


def _logical_path(path):
    if path.startswith('parrot://'):
        authority, separator, tail = path[len('parrot://'):].partition('/')
        if not authority or not separator:
            return ''
        path = '/' + tail
    if path.startswith('/ray/user/'):
        path = path[len('/ray'):]
    if not path.startswith('/user/'):
        return ''
    # Components currently expose their root dispatcher, not arbitrary children.
    name = path[len('/user/'):]
    return path if _valid_name(name, 260) else ''


def _valid_name(name, limit=256):
    return (isinstance(name, str) and 0 < len(name) <= limit and name not in ('.', '..') and
            all(c.isalnum() or c in '-_.' for c in name))


def _matches(path, prefix):
    return path == prefix or path.startswith(prefix + '-')


def _detail(error):
    try:
        message = str(error)
    except BaseException:
        message = 'unprintable ' + type(error).__name__
    return message.encode('utf-8', errors='replace').decode('utf-8')[:1024]


def _kill_and_confirm(ray, handle, probe, deadline=None):
    """Wait for Ray's death report; kill may return before a probe stops running."""
    end = time.monotonic() + 5 if deadline is None else deadline
    if time.monotonic() >= end:
        raise TimeoutError('actor retirement deadline elapsed')
    ray.kill(handle, no_restart=True)
    while True:
        remaining = end-time.monotonic()
        if remaining <= 0:
            raise TimeoutError('actor death was not confirmed')
        try:
            ray.get(probe.remote(), timeout=remaining)
        except ray.exceptions.ActorDiedError:
            return
        except Exception as error:
            setup_error = getattr(ray.exceptions, 'RuntimeEnvSetupError', ())
            # RayTaskError also inherits its user cause. A wrapped user error
            # cannot prove that actor construction/execution never started.
            if type(error) is setup_error:
                return
            raise
        # Ray 2.59 can execute a queued method after kill submission. An alive
        # reply is neither death confirmation nor a permanent retirement failure.
        time.sleep(min(0.01, max(0, end-time.monotonic())))


class _ComponentRuntime:
    """Path-owned component handles; instantiated inside the Ray worker.

    The standalone executor uses the same implementation under a local lock.
    The registry retains failed cleanup handles, never publishes a partial pool,
    and does not transfer application closures out of their owning process.
    """
    def __init__(self, dispatcher, ray_module=None, prepared_root=None, trace_config=None):
        self._prepared_root = prepared_root
        self._artifacts = {}
        self._dispatcher = dispatcher
        self._ray = ray_module
        self._entries = {}
        self._lock = threading.RLock()
        self._trace_config = None
        self._trace_recorder = None
        if trace_config is not None:
            self.configure_tracing(trace_config)

    def configure_tracing(self, config):
        from .trace_recording import TraceRecorder
        with self._lock:
            if self._entries or self._trace_recorder is not None:
                raise RuntimeError('configure recording once, before mounting components')
            recorder = TraceRecorder(config)
            self._trace_config = config
            self._trace_recorder = recorder
            return True

    def trace_snapshot(self, path=None):
        """Non-destructive process snapshot. Collect native paths before retirement."""
        with self._lock:
            if path is not None:
                entry = self._entries.get(_logical_path(path))
                if entry is None:
                    raise KeyError('no live trace owner: ' + path)
                if not _is_dispatcher(entry['handle']):
                    if self._trace_recorder is None:
                        return dict(enabled=False)
                    return self.ray.get(entry['handle'].parrot_trace_snapshot.remote(), timeout=5)
            return (dict(enabled=False) if self._trace_recorder is None else
                    self._trace_recorder.snapshot(None if path is None else _logical_path(path)))

    def trace_acknowledge(self, path, span_ids):
        """Release a saved snapshot's spans, never implicit in snapshot/export."""
        with self._lock:
            if self._trace_recorder is None:
                raise RuntimeError('recording disabled')
            if path is not None:
                entry = self._entries.get(_logical_path(path))
                if entry is None or entry['state'] != 'running':
                    raise KeyError('no live trace owner: ' + path)
                if not _is_dispatcher(entry['handle']):
                    return self.ray.get(entry['handle'].parrot_trace_acknowledge.remote(span_ids), timeout=5)
            return self._trace_recorder.acknowledge(span_ids, None if path is None else _logical_path(path))

    def trace_export_status(self, path=None):
        with self._lock:
            entry = self._entries.get(_logical_path(path)) if path is not None else None
            if path is not None and entry is None:
                raise KeyError('trace owner not found: ' + path)
            if entry is not None and not _is_dispatcher(entry['handle']):
                return self.ray.get(entry['handle'].parrot_trace_export_status.remote(), timeout=5)
            return dict(enabled=False) if self._trace_recorder is None else self._trace_recorder.export_status()

    def trace_finish(self, timeout=5.0):
        with self._lock:
            return (dict(ok=True, enabled=False) if self._trace_recorder is None else
                    self._trace_recorder.finish_export(timeout))

    def trace_flush(self, path=None, timeout=5.0):
        with self._lock:
            entry = self._entries.get(_logical_path(path)) if path is not None else None
            if path is not None and entry is None:
                raise KeyError('trace owner not found: ' + path)
            if entry is not None and not _is_dispatcher(entry['handle']):
                return self.ray.get(entry['handle'].parrot_trace_flush.remote(timeout), timeout=timeout)
            return (dict(ok=True, enabled=False) if self._trace_recorder is None else
                    self._trace_recorder.flush_export(timeout, None if path is None else _logical_path(path)))

    @property
    def ray(self):
        if self._ray is None:
            import ray
            self._ray = ray
        return self._ray

    def ready(self):
        return True

    def handles(self, paths):
        with self._lock:
            return [self._entries[p]['handle'] for p in paths if p in self._entries]

    def _context(self, name):
        from contextlib import nullcontext
        artifact = self._artifacts.get(name)
        return artifact['imports'].enter() if artifact and artifact['imports'] else nullcontext()

    def _release_artifact(self, name):
        artifact = self._artifacts.get(name)
        if artifact is not None:
            if artifact['snapshot'] is not None:
                artifact['snapshot'].close()
            del self._artifacts[name]

    def mount_module(self, name, module_name, module_dir, env, paths, version, config):
        import importlib
        from pathlib import Path
        from .ray_artifacts import Snapshot, Imports, MAX_OWNED_BYTES
        with self._lock:
            if any(path in self._entries for path in paths):
                retained = [p for p in paths if p in self._entries and self._entries[p]['owner'] == name]
                return dict(ok=False, paths=retained, detail='logical path already owned')
            created = []
            try:
                root = self._prepared_root
                if module_dir is not None and root is None:
                    owned = sum(a['snapshot'].size for a in self._artifacts.values() if a['snapshot'])
                    snapshot = Snapshot(module_dir, MAX_OWNED_BYTES-owned)
                    root = snapshot.root
                    self._artifacts[name] = dict(snapshot=snapshot, imports=Imports(root, snapshot.tops))
                if root is not None:
                    target = Path(root).joinpath(*module_name.split('.'))
                    if not target.with_suffix('.py').is_file() and not (target/'__init__.py').is_file():
                        raise ValueError('entry module is missing from the artifact snapshot')
                with self._context(name):
                    module = importlib.import_module(module_name)
                    entry = getattr(module, 'parrot_entry')
                    for path in paths:
                        handle = entry(dict(path=path, version=version, config=config, env=env, ray=self.ray))
                        native = not _is_dispatcher(handle)
                        ready = getattr(handle, '__ray_ready__', None) if native else None
                        if native and not callable(getattr(ready, 'remote', None)):
                            raise TypeError('parrot_entry must return a dispatcher or Ray actor handle')
                        self._entries[path] = dict(owner=name, handle=handle, state='starting')
                        created.append(path)
                        if native:
                            self.ray.get(ready.remote(), timeout=5)
                            if self._trace_config is not None:
                                method = getattr(handle, 'parrot_trace_configure', None)
                                if not callable(getattr(method, 'remote', None)):
                                    raise TypeError('recording requires TraceActor or AsyncTraceActor')
                                self._entries[path]['trace_config_attempted'] = True
                                if self.ray.get(method.remote(self._trace_config, path), timeout=5) is not True:
                                    raise ValueError('invalid native trace configuration receipt')
                                self._entries[path]['trace_configured'] = True
                for path in created:
                    self._entries[path]['state'] = 'running'
                return dict(ok=True, paths=created)
            except BaseException as error:
                self.retire_paths(name, created)
                retained = [p for p in created if p in self._entries]
                if not retained:
                    try:
                        self._release_artifact(name)
                    except BaseException:
                        # Preserve a cleanup-only owner if filesystem retirement fails.
                        self._entries[paths[0]] = dict(owner=name, handle=None, state='stop_failed')
                        retained = [paths[0]]
                return dict(ok=False, paths=retained, detail=_detail(error))

    def retire_paths(self, name, paths, timeout_ms=None):
        with self._lock:
            deadline = None if timeout_ms is None else time.monotonic() + min(timeout_ms, 60000) / 1000
            states = {}
            for path in paths:
                entry = self._entries.get(path)
                if entry is None:
                    states[path] = None  # Retry after an uncertain remote receipt is idempotent.
                    continue
                if entry['owner'] != name:
                    raise ValueError('logical path belongs to another component')
                entry['state'] = 'retiring' if deadline is None else 'draining'
                handle = entry['handle']
                try:
                    if deadline is not None:
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            raise TimeoutError('component drain deadline elapsed')
                        hook = getattr(handle, 'parrot_drain', None)
                        if hook is not None:
                            if _is_dispatcher(handle):
                                with self._context(name):
                                    hook()
                            else:
                                self.ray.get(hook.remote(), timeout=remaining)
                        if time.monotonic() >= deadline:
                            raise TimeoutError('component drain deadline elapsed')
                    if handle is not None and not _is_dispatcher(handle):
                        if (self._trace_config is not None and self._trace_config.export is not None
                                and entry.get('trace_config_attempted') and not entry.get('trace_finished')):
                            remaining = 5.0 if deadline is None else deadline - time.monotonic()
                            if remaining <= 0:
                                raise TimeoutError('native trace retirement deadline elapsed')
                            receipt = self.ray.get(handle.parrot_trace_finish.remote(remaining), timeout=remaining)
                            _require_trace_finish(receipt, allow_disabled=not entry.get('trace_configured'))
                            entry['trace_finished'] = True
                        _kill_and_confirm(self.ray, handle, handle.__ray_ready__, deadline)
                    elif handle is not None and self._trace_recorder is not None and self._trace_config.export is not None:
                        remaining = 5.0 if deadline is None else deadline - time.monotonic()
                        receipt = self._trace_recorder.flush_export(remaining, path)
                        if receipt.get('ok') is not True:
                            raise RuntimeError('dispatcher trace export incomplete')
                    if not any(p != path and e['owner'] == name for p,e in self._entries.items()):
                        self._release_artifact(name)
                    del self._entries[path]
                    states[path] = None
                except BaseException:
                    entry['state'] = 'stop_failed' if deadline is None else 'drain_failed'
                    states[path] = entry['state']
            return states

    def ask_traced(self, path, key, payload, parent):
        with transferred_scope(parent):
            return self.ask(path, key, payload)

    def deliver_traced(self, path, key, payload, parent):
        with transferred_scope(parent):
            return self.deliver(path, key, payload)

    def ask(self, path, key, payload, *, _trace_operation='ask'):
        with self._lock:
            if (self._trace_recorder is not None and self._trace_config.export is not None
                    and self._trace_recorder.closed):
                return dict(ok=False, code=3, detail='trace owner is retiring')
            logical = _logical_path(path)
            entry = self._entries.get(logical)
            if entry is None:
                # Only explicitly configured gateway handlers may fall back.
                # A deployed component's same-named handler is never searched.
                if logical and key in self._dispatcher._handlers:
                    reply_key, reply = _recorded_handler(self._dispatcher._handlers[key], key, payload,
                                                        self._trace_recorder, logical, _trace_operation)
                else:
                    return dict(ok=False, code=1, detail='actor not found: ' + path)
            elif entry['state'] != 'running':
                return dict(ok=False, code=3, detail='component is retiring: ' + logical)
            else:
                handle = entry['handle']
                if _is_dispatcher(handle):
                    with self._context(entry['owner']):
                        if self._trace_recorder is None:
                            reply_key, reply = handle.dispatch(key, payload)
                        else:
                            reply_key, reply = handle.dispatch(key, payload, trace_recorder=self._trace_recorder,
                                                              trace_path=logical, trace_operation=_trace_operation)
                else:
                    parent = capture_parent()
                    if parent is None and self._trace_recorder is None:
                        reply_key, reply = self.ray.get(handle.ask.remote(key, payload))
                    else:
                        method = getattr(handle, 'ask_traced', None)
                        if not callable(getattr(method, 'remote', None)):
                            return dict(ok=False, code=4, detail='native actor requires TraceActor or AsyncTraceActor')
                        reply_key, reply = self.ray.get(method.remote(key, payload, parent))
            return dict(ok=True, key=reply_key, payload=reply)

    def deliver(self, path, key, payload):
        # A native actor may distinguish delivery from request/reply. Completion
        # remains internal: normal TELL emits no Wire success/failure receipt.
        with self._lock:
            entry = self._entries.get(_logical_path(path))
            if entry is not None and entry['state'] == 'running' and not _is_dispatcher(entry['handle']):
                parent = capture_parent()
                if parent is not None or self._trace_recorder is not None:
                    method = getattr(entry['handle'], 'deliver_traced', None)
                    if not callable(getattr(method, 'remote', None)):
                        raise TypeError('native actor requires TraceActor or AsyncTraceActor')
                    self.ray.get(method.remote(key, payload, parent))
                    return
                method = getattr(entry['handle'], 'deliver', None)
                if callable(getattr(method, 'remote', None)):
                    self.ray.get(method.remote(key, payload))
                    return
            self.ask(path, key, payload, _trace_operation='tell')


class GatewayCleanupError(RuntimeError):
    """Cleanup did not complete; owner.close() retains an explicit retry path."""
    def __init__(self, owner, cause):
        self.owner = owner
        super().__init__('gateway cleanup incomplete: ' + _detail(cause))


def _require_trace_finish(receipt, *, allow_disabled=False):
    if (allow_disabled and isinstance(receipt, dict) and set(receipt) == {'ok', 'enabled'}
            and receipt['ok'] is True and receipt['enabled'] is False):
        return
    if (not isinstance(receipt, dict) or receipt.get('ok') is not True or
            receipt.get('enabled') is not True or receipt.get('running') is not False or
            type(receipt.get('pending')) is not int or receipt['pending'] != 0 or
            type(receipt.get('active')) is not int or receipt['active'] != 0 or receipt.get('error') is not None):
        raise RuntimeError('trace export retirement not confirmed')


class _GatewayResources:
    def __init__(self, ray, state):
        self.ray, self.state = ray, state
        self.worker = None
        self._retirement = None
        self._retired = False
        self._closed = False
        self._lock = threading.Lock()

    def close(self, timeout=10.0):
        import math
        if not isinstance(timeout, (int, float)) or not math.isfinite(timeout) or timeout <= 0:
            raise ValueError('gateway shutdown timeout must be finite and positive')
        deadline = time.monotonic() + timeout
        if not self._lock.acquire(timeout=timeout):
            raise GatewayCleanupError(self, TimeoutError('another cleanup attempt is still running'))
        try:
            if self._closed:
                return
            self.state.closing = True
            self.state.ask_admission.seal()
            self.state.management.close(deadline)
            self.state.ask_admission.wait(deadline)
            if self.worker is not None:
                if not self._retired:
                    if self._retirement is None:
                        self._retirement = self.worker.close.remote()
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError('gateway retirement deadline elapsed')
                    try:
                        receipt = self.ray.get(self._retirement, timeout=remaining)
                    except Exception as error:
                        timeout_type = getattr(self.ray.exceptions, 'GetTimeoutError', TimeoutError)
                        if not isinstance(error, (TimeoutError, timeout_type)):
                            self._retirement = None
                        raise
                    self._retirement = None
                    if (not isinstance(receipt, dict) or type(receipt.get('ok')) is not bool or
                            receipt != {'ok': True, 'pending': []}):
                        raise RuntimeError('deployment retirement incomplete: ' + _detail(receipt))
                    self._retired = True
                _kill_and_confirm(self.ray, self.worker, self.worker.ready, deadline)
                self.worker = None
            with self.state.lock:
                self.state.components.clear()
                self.state.path_states = {}
            self._closed = True
        except BaseException as error:
            raise GatewayCleanupError(self, error) from error
        finally:
            self._lock.release()


def serve(port: int, dispatcher=None, ray_kwargs: dict | None = None,
          parrot_addr: str | None = None, *, shutdown_timeout=10.0, trace_config=None) -> None:
    """启动网关（阻塞）。

    - ray.init 参数可注入（测试用 num_cpus=2 / include_dashboard=False）
    - parrot_addr（双模式组网之注册模式）：给出 "host:port" 时不再被动
      accept，而是主动拨号 parrot 节点并发起客户端握手（发 HANDSHAKE →
      收 HANDSHAKE_ACK），随后同一帧循环服务——生产拓扑形态。
    - dispatcher 可注入（默认空；未部署 path 返回 ActorNotFound）
    - DEV_08 并发模型：收包线程只做帧解码与派发；ASK 的 ray.get 在
      worker 线程池执行（慢 handler 不阻塞后续帧/心跳）；回复经
      有界 outbox 由单独 writer 写出。组件 worker 默认串行执行。
    """
    import ray
    import math
    if not isinstance(shutdown_timeout, (int, float)) or not math.isfinite(shutdown_timeout) or shutdown_timeout <= 0:
        raise ValueError('gateway shutdown timeout must be finite and positive')
    if trace_config is not None:
        from .trace_recording import TraceConfig
        if not isinstance(trace_config, TraceConfig):
            raise TypeError('expected TraceConfig')

    from .ray_startup import initialize
    if ray.is_initialized():
        if ray_kwargs:
            raise ValueError('ray_kwargs cannot reconfigure an existing Ray runtime')
    else:
        initialize(ray, ray_kwargs or {"num_cpus": 2, "include_dashboard": False, "log_to_driver": False})

    dispatch = dispatcher or ParrotDispatcher()

    from .ray_hosts import IsolatedRuntime
    admin_state = RayAdminState()
    owner = _GatewayResources(ray, admin_state)
    try:
        factory = ray.remote(IsolatedRuntime).options(num_cpus=0)
        worker = factory.remote(dispatch) if trace_config is None else factory.remote(dispatch, trace_config=trace_config)
        owner.worker = worker
        _serve_network(port, dispatch, worker, ray, admin_state, parrot_addr)
    finally:
        owner.close(shutdown_timeout)


def _serve_network(port, dispatch, worker, ray, admin_state, parrot_addr):
    if parrot_addr:
        ray.get(worker.ready.remote())
        from urllib.parse import urlsplit
        address = urlsplit("//" + parrot_addr)
        host, port = address.hostname, address.port
        if not host or not port:
            raise ValueError("invalid registration address")
        attempt = 0
        announced = False
        while True:
            if attempt:
                time.sleep(min(60.0, 2 ** min(attempt - 1, 6)))
            try:
                sock = socket.create_connection((host, port), timeout=10)
            except OSError as error:
                print(f"[ray-gw] connect failed: {error!r}", file=sys.stderr, flush=True)
                attempt += 1
                continue
            established = False

            def ready():
                nonlocal established, announced
                established = True
                if not announced:
                    print(f"RAY_GW_REGISTERED={parrot_addr}", flush=True)
                    announced = True

            with ExitStack() as connection:
                connection.callback(sock.close)
                links = _DirectLinks(dispatch, worker, ray, admin_state)
                connection.callback(links.close)
                _serve_connection_guarded(sock, dispatch, worker, ray, admin_state,
                                          initiator=True, ready=ready, route_hint=links.hint,
                                          idle_timeout=10.0)
            attempt = 1 if established else attempt + 1
        return

    with ExitStack() as network:
        connections = _ConnectionDrivers()
        network.callback(connections.close)
        srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        network.callback(srv.close)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1 << 20)
        srv.bind(("127.0.0.1", port))
        srv.listen(128)
        print(f"RAY_GW_PORT={srv.getsockname()[1]}", flush=True)

        while True:
            sock, _addr = srv.accept()
            try:
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                connections.start(sock, dispatch, worker, ray, admin_state)
            except BaseException:
                sock.close()
                raise


class _ConnectionDrivers:
    """Own accepted sockets and join their Wire drivers before serve unwinds.

    Closing a peer retires only that connection. Gateway exit first shuts down
    every live socket to wake readers/writers, then joins every started driver.
    Completed thread objects are pruned on admission, without retaining every
    historical connection. Running ASK/admin work remains owned by its runtime.
    """
    def __init__(self):
        self._lock = threading.Lock()
        self._sockets = set()
        self._threads = set()
        self._closed = False

    def start(self, sock, *args):
        with self._lock:
            if self._closed:
                raise RuntimeError('gateway connection owner is closed')
            self._threads = {thread for thread in self._threads if thread.is_alive()}
            thread = threading.Thread(target=self._run, args=(sock, args),
                                      name='parrot-gateway-connection', daemon=True)
            self._sockets.add(sock)
            self._threads.add(thread)
            try:
                thread.start()
            except BaseException:
                if thread.ident is None:
                    self._sockets.discard(sock)
                    self._threads.discard(thread)
                raise

    def _run(self, sock, args):
        try:
            _serve_connection_guarded(sock, *args)
        finally:
            with self._lock:
                self._sockets.discard(sock)

    def close(self):
        with self._lock:
            self._closed = True
            sockets, threads = list(self._sockets), list(self._threads)
        for sock in sockets:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        for thread in threads:
            thread.join()
        with self._lock:
            self._sockets.clear()
            self._threads.clear()



class _ConnectionOutbox:
    """Bound queued plus in-flight wire bytes; overload retires the connection."""

    MAX_PENDING_FRAMES = 256
    MAX_PENDING_BYTES = 16 * 1024 * 1024

    def __init__(self, sock):
        self._sock = sock
        self.max_frame_len = MAX_FRAME_LEN
        self._pending_frames = 0
        self._pending_bytes = 0
        self._queue = queue.Queue()
        self._lock = threading.Lock()
        self._closed = False
        self._thread = threading.Thread(target=self._write, name="parrot-gateway-writer", daemon=True)
        self._thread.start()

    def put(self, frame):
        if len(frame) >= 4 and int.from_bytes(frame[:4], "little") == len(frame) - 4 and len(frame) - 4 > self.max_frame_len:
            self._finish()
            return False
        with self._lock:
            if self._closed:
                return False
            if (self._pending_frames >= self.MAX_PENDING_FRAMES or
                    len(frame) > self.MAX_PENDING_BYTES - self._pending_bytes):
                self._retire_locked()
            else:
                self._pending_frames += 1
                self._pending_bytes += len(frame)
                self._queue.put(frame)
                return True
        # Closing on overload must also unblock the in-flight socket write and
        # reader; never enqueue an error response into the full outbox.
        self._shutdown_socket()
        return False

    def _retire_locked(self):
        self._closed = True
        while True:
            try:
                frame = self._queue.get_nowait()
            except queue.Empty:
                break
            if frame is not None:
                self._pending_frames -= 1
                self._pending_bytes -= len(frame)
        self._queue.put(None)

    def _shutdown_socket(self):
        try:
            self._sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

    def _finish(self):
        with self._lock:
            if self._closed:
                return
            self._retire_locked()
        self._shutdown_socket()

    def _write(self):
        try:
            while True:
                frame = self._queue.get()
                if frame is None:
                    return
                try:
                    self._sock.sendall(frame)
                    METRICS.transmitted(frame)
                finally:
                    # The writer owns this frame after dequeue, including when
                    # shutdown concurrently discards the remaining queue.
                    with self._lock:
                        self._pending_frames -= 1
                        self._pending_bytes -= len(frame)
        except OSError:
            # Also wake the reader so a failed writer cannot leave a live session.
            pass
        finally:
            self._finish()

    def close(self):
        self._finish()
        self._thread.join()


class _DirectLinks:
    """Bounded child connections owned by one registered parent connection."""
    def __init__(self, dispatch, worker, ray, admin_state, max_links=8):
        from concurrent.futures import ThreadPoolExecutor
        self._dispatch, self._worker, self._ray = dispatch, worker, ray
        self._admin_state = admin_state
        self._lock = threading.Lock()
        self._closed = False
        self._links = {}
        self._max_links = max_links
        self._pool = ThreadPoolExecutor(max_workers=max_links, thread_name_prefix="parrot-direct")

    def hint(self, payload):
        from urllib.parse import urlsplit
        if not 4 <= len(payload) <= 4096:
            return
        try:
            nlen = int.from_bytes(payload[:2], "little")
            off = 2 + nlen
            if off + 2 > len(payload):
                return
            node = payload[2:off].decode()
            alen = int.from_bytes(payload[off:off+2], "little")
            if off + 2 + alen != len(payload):
                return
            address = urlsplit("//" + payload[off+2:].decode())
            host, port = address.hostname, address.port
            if not node or not host or not port or address.path or address.query or address.fragment or address.username:
                return
        except (ValueError, UnicodeError):
            return
        with self._lock:
            if self._closed or node in self._links or len(self._links) >= self._max_links:
                return
            self._links[node] = None
            self._pool.submit(self._dial, node, host, port)

    def _dial(self, node, host, port):
        sock = None
        try:
            sock = socket.create_connection((host, port), timeout=10)
            with self._lock:
                if self._closed:
                    return
                self._links[node] = sock
            _serve_connection_guarded(sock, self._dispatch, self._worker, self._ray,
                                      self._admin_state, initiator=True, expected_node=node,
                                      idle_timeout=10.0)
        except (OSError, ValueError) as error:
            print(f"[ray-gw] direct dial {node!r} failed: {error!r}", file=sys.stderr, flush=True)
        finally:
            if sock is not None:
                sock.close()
            with self._lock:
                self._links.pop(node, None)

    def close(self):
        with self._lock:
            self._closed = True
            sockets = [sock for sock in self._links.values() if sock is not None]
        for sock in sockets:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        # Socket connects have a ten-second timeout; hostname resolution is
        # governed by the system resolver. Join every child before retirement.
        self._pool.shutdown(wait=True, cancel_futures=True)


def _serve_connection_guarded(sock, dispatch, worker, ray, admin_state=None, **options) -> None:
    lease = _ConnectionMetricLease(METRICS)
    try:
        _serve_connection(sock, dispatch, worker, ray, admin_state, lease=lease, **options)
    except (OSError, ValueError) as error:
        print(f"[ray-gw] connection retired: {error!r}", file=sys.stderr, flush=True)
    finally:
        lease.close()
        try:
            sock.close()
        except OSError:
            pass


def _serve_connection(sock, dispatch, worker, ray, admin_state=None, *, lease,
                      initiator=False, ready=None, expected_node=None,
                      route_hint=None, handshake_timeout=10.0, idle_timeout=None) -> None:
    """One Wire driver for listen, registration and owned direct connections."""
    from concurrent.futures import ThreadPoolExecutor

    with ExitStack() as connection:
        if admin_state is None:
            admin_state = RayAdminState()
        out = _ConnectionOutbox(sock)
        connection.callback(out.close)
        pool = ThreadPoolExecutor(max_workers=RAY_GW_ASK_WORKERS)
        connection.callback(pool.shutdown, wait=False, cancel_futures=True)

        def run_ask(cid, reply_to, path, type_key, payload, parent):
            try:
                result = admin_state.route_error(path)
                if result is None:
                    with transferred_scope(parent):
                        if worker is None:
                            result = admin._runtime().ask(path, type_key, payload)
                        elif parent is None:
                            result = ray.get(worker.ask.remote(path, type_key, payload))
                        else:
                            result = ray.get(worker.ask_traced.remote(path, type_key, payload, parent))
                if result['ok']:
                    out.put(build_frame(FT_REPLY, cid, reply_to, result['key'], result['payload']))
                else:
                    out.put(build_frame(FT_REPLY_ERR, cid, reply_to, '',
                                        encode_err_payload(result['code'], result['detail'])))
            except BaseException as error:
                # This is an owned background task, not the driver's main thread.
                # User SystemExit/KeyboardInterrupt must not disappear in a Future.
                try:
                    code = ERR_UNKNOWN_TYPE_KEY if isinstance(error, UnknownTypeKeyError) else ERR_PROTOCOL_VIOLATION
                    out.put(build_frame(FT_REPLY_ERR, cid, reply_to, "",
                                        encode_err_payload(code, _detail(error))))
                except BaseException:
                    # Even diagnostic construction can fail. Retire only this
                    # connection so callers observe failure instead of waiting.
                    out._finish()

        admin = RayAdminExecutor(out, ray_module=ray, gateway_dispatcher=dispatch,
                                worker_ref=worker, state=admin_state,
                                on_reply_failure=out._finish)
        # Both handshake frames use the global bound, before negotiation.
        decoder = FrameDecoder()
        deadline = time.monotonic() + handshake_timeout
        last_inbound = time.monotonic()
        handshake_done = False
        trace_capable = False
        if initiator:
            sock.settimeout(max(0.001, deadline - time.monotonic()))
            encoded = build_frame(FT_HANDSHAKE, 1, "", "__handshake__",
                                  handshake_body(os.environ.get("PARROT_NODE_ID", "ray-gw-1"),
                                                 _GATEWAY_CAPABILITIES))
            # Before handshake admission no business/admin task can enqueue a
            # frame; the outbox is idle, so there is exactly one socket writer.
            sock.sendall(encoded)
            METRICS.transmitted(encoded)
        while True:
            if not handshake_done:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("Wire handshake deadline elapsed")
                sock.settimeout(remaining)
            try:
                chunk = sock.recv(65536)
            except socket.timeout:
                if not handshake_done or (idle_timeout is not None and time.monotonic() - last_inbound >= idle_timeout):
                    raise TimeoutError("Wire handshake or idle deadline elapsed")
                continue
            if not chunk:
                return
            last_inbound = time.monotonic()
            decoder.feed(chunk)
            while (frame := decoder.next_frame()) is not None:
                METRICS.received(frame)
                if frame.ft == FT_ERROR:
                    # Retire outside the recoverable application-dispatch boundary.
                    raise ValueError("remote terminal ERROR")
                # Decoding is structurally lossless; execution admits only
                # metadata flags. Batch/compression/encryption/completion and
                # durable envelopes require semantics not implemented here.
                if frame.flags & ~(2 | 8 | FLAG_TRACE_CONTEXT) or frame.type_key == "parrot:durable#v1":
                    raise ValueError("unsupported execution envelope")
                encoded_size = (4 + BODY_FIXED_OVERHEAD + len(frame.path.encode('utf-8')) +
                                len(frame.type_key.encode('utf-8')) + len(frame.payload))
                if frame.flags & FLAG_TRACE_CONTEXT and not trace_capable:
                    raise ValueError("unnegotiated causal context")
                trace_parent = frame.take_trace_parent()
                parent = None if trace_parent is None else trace_parent.encode()
                if not handshake_done:
                    expected = FT_HANDSHAKE_ACK if initiator else FT_HANDSHAKE
                    if frame.ft != expected:
                        raise ValueError("unexpected frame before Wire handshake")
                    peer = decode_handshake(frame.payload, acknowledgement=initiator)
                    if expected_node is not None and peer["node_id"] != expected_node:
                        raise ValueError("direct link peer identity mismatch")
                    if not initiator:
                        encoded = build_frame(FT_HANDSHAKE_ACK, frame.cid, "", "__handshake__",
                            handshake_ack_body(os.environ.get("PARROT_NODE_ID", "ray-gw-1"),
                                               _GATEWAY_CAPABILITIES, peer["chosen_codec"]))
                        sock.sendall(encoded)
                        METRICS.transmitted(encoded)
                    # Activate only after ACK has been fully written/validated,
                    # before decoding any pipelined established frame.
                    negotiated = min(peer["max_frame_len"], DEFAULT_HANDSHAKE_FRAME_LEN, MAX_FRAME_LEN)
                    out.max_frame_len = negotiated
                    decoder.max_frame_len = negotiated
                    lease.complete()
                    handshake_done = True
                    trace_capable = bool(peer['capabilities'] & CAPS_TRACE_CONTEXT)
                    sock.settimeout(None if idle_timeout is None else min(5.0, idle_timeout))
                    if ready is not None:
                        ready()
                    continue
                if frame.ft in (FT_HANDSHAKE, FT_HANDSHAKE_ACK):
                    raise ValueError("duplicate Wire handshake")
                try:
                    if frame.ft == FT_ASK:
                        try:
                            request = split_reply_to(frame.payload)
                        except UnicodeError:
                            request = None
                        if request is None:
                            out.put(build_frame(FT_REPLY_ERR, frame.cid, "", "",
                                encode_err_payload(ERR_PROTOCOL_VIOLATION, "invalid ASK reply_to prefix")))
                            continue
                        reply_to, payload = request
                        permit = admin_state.ask_admission.acquire(encoded_size)
                        if permit is None:
                            out.put(build_frame(FT_REPLY_ERR, frame.cid, reply_to, "",
                                encode_err_payload(ERR_OVERLOADED, "gateway ASK capacity exhausted")))
                            continue
                        try:
                            pending = pool.submit(run_ask, frame.cid, reply_to, frame.path, frame.type_key, payload, parent)
                        except BaseException as error:
                            permit.release()
                            if not isinstance(error, Exception):
                                raise
                            out.put(build_frame(FT_REPLY_ERR, frame.cid, reply_to, "",
                                encode_err_payload(ERR_OVERLOADED, "gateway ASK scheduling unavailable")))
                            continue
                        # Bind this request's permit now, not a loop-variable
                        # closure that could release another request's charge.
                        pending.add_done_callback(permit.release)
                    elif frame.ft == FT_TELL:
                        if admin_state.route_error(frame.path) is not None:
                            continue
                        with transferred_scope(parent):
                            if worker is None:
                                admin._runtime().deliver(frame.path, frame.type_key, frame.payload)
                            elif parent is None:
                                worker.deliver.remote(frame.path, frame.type_key, frame.payload)
                            else:
                                worker.deliver_traced.remote(frame.path, frame.type_key, frame.payload, parent)
                    elif frame.ft == FT_HEARTBEAT:
                        out.put(build_frame(FT_HEARTBEAT_ACK, frame.cid, "", "", b""))
                    elif frame.ft == FT_ROUTE_HINT and route_hint is not None:
                        route_hint(frame.payload)
                    elif frame.ft == FT_SYSTEM_EVENT:
                        admin.on_frame(frame.cid, frame.path, frame.payload,
                            encoded_size=4 + BODY_FIXED_OVERHEAD + len(frame.path.encode('utf-8')) +
                                         len(frame.type_key.encode('utf-8')) + len(frame.payload))
                except Exception as error:  # noqa: BLE001
                    print(f"[ray-gw] frame dispatch error: {error!r}", file=sys.stderr, flush=True)


def main(argv: list[str], *, ray_kwargs: dict | None = None) -> None:
    from .gateway_cli import parse_gateway_arguments
    port, parrot_addr, trace_config = parse_gateway_arguments(argv)
    d = ParrotDispatcher()

    @d.handler("bin:u:Ping")
    def _ping(_k: str, p: bytes) -> tuple[str, bytes]:
        (n,) = struct.unpack("<Q", p)
        return ("bin:u:Pong", struct.pack("<Q", n + 2))  # ray 方言 +2

    @d.handler("bin:u:Add")
    def _add(_k: str, p: bytes) -> tuple[str, bytes]:
        a, b = struct.unpack("<QQ", p)
        return ("bin:u:AddR", struct.pack("<Q", a + b + 1000))  # ray 方言 +1000

    # Gateway handlers are explicit probes. Deploy creates separate path-owned
    # components inside the worker; their handlers are not globally merged.


    import ray
    from .ray_startup import graceful_sigterm
    already_initialized = ray.is_initialized()
    with graceful_sigterm():
        try:
            serve(port, d, ray_kwargs=ray_kwargs, parrot_addr=parrot_addr, trace_config=trace_config)
        except KeyboardInterrupt:
            pass
        finally:
            # The CLI owns a runtime it initialized, including partial startup
            # failure. Never disconnect an embedding caller's existing runtime.
            if not already_initialized:
                ray.shutdown()


if __name__ == "__main__":
    main(sys.argv)
