"""Parrot Ray Gateway（DEV_03 §5 / E4 转正）。

Rust 节点 <--Wire 1.0(TLV 握手)--> 本网关 <--ray API--> Ray actor。

- ParrotDispatcher（ray.remote actor）：TYPE_KEY → handler 分发；
  ask = ray.get（同步配对）；deliver = 不 get（非取消语义——文档明示）
- 路径映射：parrot://…/ray/{name} ↔ ActorHandle（named actor）

用法（对齐 JVM ParrotGatewayMain 的 CLI 契约）：
  python3 -m parrot_protocol.ray_gw <port>        # 打印 RAY_GW_PORT=<port>
"""

from __future__ import annotations

import os
import queue
import socket
import struct
import sys
import threading
import time

from .wire import (
    FT_ASK,
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

ERR_UNKNOWN_TYPE_KEY = 6

# DEV_08：ASK 执行池大小（慢 handler 并发度；环境变量可覆盖——
# PARROT_RAY_GW_WORKERS=1 即退化为旧行为做 A/B 对照）
RAY_GW_ASK_WORKERS = int(os.environ.get("PARROT_RAY_GW_WORKERS", "8"))


# ---------------- B3（DEV_09）：admin-v2 ray 方言执行器 ----------------
# DeployComponent{PyModule} → ray job API submit(working_dir=module 目录,
# runtime_env) → module 内 parrot_entry(ctx) 起命名 actor；
# Drain/Stop → ray kill 命名 actor；Status → 网关侧登记表 + ray 探活。


class RayAdminExecutor:
    """admin-v2 四命令的 ray 方言实现。

    组件登记表：name → {version, paths, handles}（进程内；网关单点）。
    实例路径与 parrot/erl/jvm 方言同规：/user/{name}[-{i}]——
    path_prefix 匹配规则与 Rust 侧一致（整段相等或后随 '-'）。
    """

    def __init__(self, out_queue, ray_module=None, gateway_dispatcher=None,
                 worker_ref=None) -> None:
        self._out = out_queue          # 回帧队列（writer 线程消费）
        self._ray = ray_module         # 可注入（测试桩）；None → import ray
        self._components: dict[str, dict] = {}
        self._lock = threading.Lock()
        # R4：网关 dispatcher（deploy 组件挂载点；None = 测试桩形态）
        self._gw_dispatcher = gateway_dispatcher
        # R4：ray actor 引用（组件 dispatcher 送进 actor 进程挂载——
        # 本地 mount 对 actor 内深拷贝副本无效）
        self._worker = worker_ref

    # ---- ray 句柄（惰性导入；测试注入桩）----
    @property
    def ray(self):
        if self._ray is None:
            import ray  # noqa: PLC0415（网关进程内惰性——测试可先注入桩）
            self._ray = ray
        return self._ray

    # ---- 回帧 ----
    def _send_reply(self, req_id: int, reply: dict, reply_to: str) -> None:
        from .admin_v2 import encode_admin_reply_v2

        self._out.put(
            build_frame(FT_SYSTEM_EVENT, req_id, reply_to, "",
                        encode_admin_reply_v2(reply))
        )

    # ---- SYSTEM_EVENT 入口（serve 帧循环调用）----
    def on_frame(self, cid: int, reply_to: str, payload: bytes) -> bool:
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
        cmd = decode_admin_cmd_v2(payload)
        # 命令执行线程池外跑（deploy 可能拉 ray job——秒级；不阻塞收帧）
        threading.Thread(
            target=self._dispatch, args=(cmd, reply_to), daemon=True
        ).start()
        return True

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
            else:  # pragma: no cover —— 解码层已限四形态
                reply = {"kind": "failed", "code": ERR_DIALECT_MISMATCH,
                         "detail": f"unknown kind {kind}"}
        except _NotFound as e:
            reply = {"kind": "failed", "code": ERR_COMPONENT_NOT_FOUND,
                     "detail": f"no component under {e}"}
        except Exception as e:  # noqa: BLE001
            reply = {"kind": "failed", "code": ERR_ARTIFACT_FETCH, "detail": str(e)}
        reply["req_id"] = cmd["req_id"]
        self._send_reply(cmd["req_id"], reply, reply_to)

    # ---- Deploy：PyModule → ray job → parrot_entry(ctx) 命名 actor ----
    def deploy(self, comp: dict) -> dict:
        from .admin_v2 import ERR_DIALECT_MISMATCH

        art = comp["artifact"]
        if art["kind"] != "pymodule":
            return {
                "kind": "failed",
                "code": ERR_DIALECT_MISMATCH,
                "detail": f"ray executor expects PyModule, got {art['kind']}",
            }
        # R3：uri（file:// 目录形态）= app 源码 working_dir——优先于
        # module 相对发现（app 标准包直发形态）
        uri = art.get("uri")
        if uri and uri.startswith("file://"):
            module_dir = uri[len("file://"):]
        else:
            module_dir = os.path.dirname(art["module"].replace(".", "/")) or "."
        # runtime_env：TOML 文本 → dict（py3.11+ tomllib；旧版容错降级）
        env_dict: dict = {}
        re_text = art.get("runtime_env")
        if re_text:
            try:
                import tomllib  # noqa: PLC0415

                env_dict = tomllib.loads(re_text)
            except ImportError:
                env_dict = {"pip": [], "env_vars": {}, "_raw": re_text}
        ray = self.ray
        # ray job submission（集群形态）——本地 init 形态退化为
        # importlib 直载 module + 本地调用 parrot_entry
        instances = _expand_paths(comp["name"], comp["instances"])
        try:
            handles = self._launch(comp, module_dir, env_dict, instances)
        except Exception as e:  # noqa: BLE001
            return {
                "kind": "failed",
                "code": 0x0A00,
                "detail": f"ray launch failed: {e}",
            }
        with self._lock:
            self._components[comp["name"]] = {
                "version": comp["version"],
                "paths": instances,
                "handles": handles,
            }
        return {"kind": "deployed", "instances": instances}

    def _launch(self, comp: dict, module_dir: str, env_dict: dict, instances: list[str]):
        """PyModule 启动：module 目录为 working_dir。

        真 ray 集群：JobSubmissionClient().submit(working_dir, entrypoint)。
        本地 ray.init（测试/单机形态）：importlib 直载 + parrot_entry 本地起
        命名 actor（ray.get 强制落位——与 serve 的 worker 就绪等待同式）。

        R4：parrot_entry 返回 ParrotDispatcher（app 组件形态——apps/*/python）
        时额外挂载到网关 dispatcher（组件 handler 优先于内置探针）；
        返回 ray actor 句柄时保持 B3 句柄形态（兼容旧 fixture）。
        """
        # dispatcher 形态：组件送入 ray actor 进程构建+挂载（ask 路由到
        # 组件 handler）；网关本地 dispatcher 同挂（测试桩/本地直连形态）
        handles: list = []
        mounted = None
        module_name = comp["artifact"]["module"]
        if self._worker is not None:
            self.ray.get(self._worker.mount_module.remote(
                comp["name"], module_name, module_dir, env_dict, instances))
            mounted = True
        else:
            import importlib
            import os
            import sys

            # working_dir 语义：module 目录入 sys.path（app 标准包解包位）
            if module_dir and module_dir not in sys.path:
                sys.path.insert(0, os.path.abspath(module_dir))
            mod = importlib.import_module(module_name)
            entry = getattr(mod, "parrot_entry", None)
            if entry is None:
                raise RuntimeError(f"{module_name} has no parrot_entry(ctx)")
            # parrot_entry(ctx) → dispatcher 工厂；ctx 含 instances/env
            for path in instances:
                h = entry({"path": path, "env": env_dict, "ray": self.ray})
                handles.append(h)
                if _is_dispatcher(h):
                    mounted = h
            if mounted is not None and self._gw_dispatcher is not None:
                self._gw_dispatcher.mount(mounted)
        return handles

    def _unmount(self, info: dict, comp_name: str | None = None) -> None:
        """卸载组件 dispatcher（drain/stop——句柄形态无操作）。"""
        if self._worker is not None and comp_name is not None:
            try:
                self._worker.unmount_name.remote(comp_name)
            except Exception:  # noqa: BLE001
                pass
        for h in info.get("handles", []):
            if _is_dispatcher(h):
                if self._gw_dispatcher is not None:
                    self._gw_dispatcher.unmount(h)

    # ---- Drain：排空（命名 actor 优雅停——ray 无 drain 原语，
    #      语义映射为 drain 钩子调用 + ray.kill(no_restart=True)）----
    def drain(self, prefix: str, timeout_ms: int) -> tuple[int, int]:
        comps = self._match(prefix)
        if not comps:
            raise _NotFound(prefix)
        drained = aborted = 0
        deadline = time.monotonic() + min(timeout_ms, 60000) / 1000.0
        for name, info in comps:
            ok = True
            for h in info["handles"]:
                try:
                    if hasattr(h, "parrot_drain"):
                        h.parrot_drain.remote()
                        self.ray.get(h.parrot_drain.remote())
                except Exception:  # noqa: BLE001
                    ok = False
            if ok and time.monotonic() < deadline:
                self._kill_handles(info["handles"])
                self._unmount(info, name)
                drained += len(info["paths"])
            else:
                aborted += len(info["paths"])
            with self._lock:
                self._components.pop(name, None)
        return drained, aborted

    # ---- Stop：立即停（ray.kill 全部实例句柄）----
    def stop(self, prefix: str) -> dict:
        comps = self._match(prefix)
        if not comps:
            raise _NotFound(prefix)
        for name, info in comps:
            self._kill_handles(info["handles"])
            self._unmount(info, name)
            with self._lock:
                self._components.pop(name, None)
        return {"kind": "stopped"}

    # ---- Status：登记表 + 句柄探活（stub：alive 标记；真 ray：登记表为准）----
    def status(self, prefix: str) -> dict:
        comps = self._match(prefix)
        if not comps:
            raise _NotFound(prefix)
        states = []
        for _name, info in comps:
            alive = all(
                getattr(h, "alive", True) for h in info["handles"]
            )
            for p in info["paths"]:
                states.append({
                    "path": p,
                    "state": "running" if alive else "degraded",
                    "version": info["version"],
                })
        return {"kind": "status", "states": states}

    # ---- 前缀匹配（Rust 侧同规：整段相等或后随 '-'）----
    def _match(self, prefix: str) -> list[tuple[str, dict]]:
        with self._lock:
            return [
                (name, dict(info))
                for name, info in self._components.items()
                if any(
                    p == prefix or (p.startswith(prefix) and p[len(prefix) : len(prefix) + 1] == "-")
                    for p in info["paths"]
                )
            ]

    def _kill_handles(self, handles: list) -> None:
        ray = self.ray
        for h in handles:
            try:
                ray.kill(h, no_restart=True)
            except Exception:  # noqa: BLE001
                pass


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
    """实例路径展开（与 parrot-app/Rust executor 同规）。"""
    if policy["kind"] == "singleton":
        return [f"/user/{name}"]
    n = policy["count"]
    return [f"/user/{name}-{i}" for i in range(n)]



class ParrotDispatcher:
    """TYPE_KEY → handler 分发（ray 侧单例形态）。

    子类注册 handler：`@dispatcher.handler("pb:demo/Echo")`。

    R4（应用体系架构纠正）：`mount` 支持挂载已部署组件的子 dispatcher
    （deploy 时网关调用）——组件 handler 与网关内置探针同键时组件优先
    （业务键唯一——冲突视为部署错误抛 KeyError）。网关自身不再内置
    业务 handler（业务已迁 apps/*/python/）。
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
        """挂载组件 dispatcher（deploy）——重复挂载幂等跳过。"""
        if sub not in self._mounted:
            self._mounted.append(sub)

    def unmount(self, sub: "ParrotDispatcher") -> None:
        """卸载组件 dispatcher（drain/stop）。"""
        if sub in self._mounted:
            self._mounted.remove(sub)

    def dispatch(self, type_key: str, payload: bytes) -> tuple[str, bytes]:
        # 已部署组件优先（业务键）——网关内置探针兜底
        for sub in reversed(self._mounted):
            fn = sub._handlers.get(type_key)
            if fn is not None:
                return fn(type_key, payload)
        fn = self._handlers.get(type_key)
        if fn is None:
            raise KeyError(f"unknown type key {type_key}")
        return fn(type_key, payload)


def serve(port: int, dispatcher=None, ray_kwargs: dict | None = None,
          parrot_addr: str | None = None) -> None:
    """启动网关（阻塞）。

    - ray.init 参数可注入（测试用 num_cpus=2 / include_dashboard=False）
    - parrot_addr（双模式组网之注册模式）：给出 "host:port" 时不再被动
      accept，而是主动拨号 parrot 节点并发起客户端握手（发 HANDSHAKE →
      收 HANDSHAKE_ACK），随后同一帧循环服务——生产拓扑形态。
    - dispatcher 可注入（默认空——无 handler 时 ASK 一律 UnknownTypeKey）
    - DEV_08 并发模型：收包线程只做帧解码与派发；ASK 的 ray.get 在
      worker 线程池执行（慢 handler 不阻塞后续帧/心跳）；回复经
      out 队列由 writer 线程写出（天然合并写、无锁竞争）。
    """
    import ray

    ray.init(**(ray_kwargs or {"num_cpus": 2, "include_dashboard": False, "log_to_driver": False}))

    dispatch = dispatcher or ParrotDispatcher()

    @ray.remote
    class RayWorker:
        """ParrotDispatcher 的 ray actor 化（ask=ray.get / deliver=fire-and-forget）。

        R4：mount_module/unmount_name 远程方法——deploy 的 PyModule 组件
        在 actor 进程内 importlib 载入 + parrot_entry 实例化 + 挂载
        （dispatcher 含闭包状态，跨进程序列化会因 actor 侧无模块而
        失败——组件必须在目标进程内构建）。ask 路由：组件 handler
        优先（后挂载优先），网关探针兜底。
        """

        def __init__(self, d: ParrotDispatcher) -> None:
            self._d = d
            self._mounted: dict[str, object] = {}  # comp_name → dispatcher

        def ask(self, type_key: str, payload: bytes) -> tuple[str, bytes]:
            for sub in reversed(list(self._mounted.values())):
                fn = getattr(sub, "_handlers", {}).get(type_key)
                if fn is not None:
                    return fn(type_key, payload)
            return self._d.dispatch(type_key, payload)

        def deliver(self, type_key: str, payload: bytes) -> None:
            self.ask(type_key, payload)  # 不 get（非取消语义）

        def mount_module(self, comp_name: str, module_name: str, module_dir: str,
                         env_dict: dict, paths: list[str]) -> list[str]:
            """actor 进程内载入 app 组件模块并挂载其 dispatcher。"""
            import importlib
            import os
            import sys

            if module_dir and module_dir not in sys.path:
                sys.path.insert(0, os.path.abspath(module_dir))
            mod = importlib.import_module(module_name)
            entry = getattr(mod, "parrot_entry", None)
            if entry is None:
                raise RuntimeError(f"{module_name} has no parrot_entry(ctx)")
            import ray as _ray

            for p in paths:
                h = entry({"path": p, "env": env_dict, "ray": _ray})
                if _is_dispatcher(h):
                    self._mounted[comp_name] = h
            return paths

        def unmount_name(self, comp_name: str) -> None:
            self._mounted.pop(comp_name, None)

    worker = RayWorker.remote(dispatch)

    if parrot_addr:
        # 注册模式：主动拨号 parrot 节点 + 客户端握手（Rust 侧 accept 后
        # 会回 HANDSHAKE_ACK——随后双向帧流与被动模式完全一致）
        # worker 就绪等待：actor 异步调度（SchedulingCancelled 规避——
        # ray.get 强制等待首个任务落位再进入帧循环）
        class _Ready:
            def ready(self) -> bool:
                return True

        _probe = ray.remote(_Ready).remote()
        ray.get(_probe.ready.remote())
        host, _, pport = parrot_addr.partition(":")

        # ---- 容灾：重连监督 + 半开检测（与 Rust hub 心跳对称）----
        # connect_loop：断线/半开 → 指数退避重拨（1s→60s）→ 重新握手注册。
        # 首连成功打印 RAY_GW_REGISTERED（stdout 契约）；重连静默恢复。
        import time as _time

        def connect_loop() -> None:
            attempt = 0
            announced = False
            while True:
                if attempt:
                    _time.sleep(min(60.0, 1.0 * (2 ** min(attempt, 6))))
                try:
                    s = socket.create_connection((host, int(pport)), timeout=10)
                except OSError as e:
                    print(f"[ray-gw] connect parrot failed: {e!r}", file=sys.stderr, flush=True)
                    attempt += 1
                    continue
                s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                try:
                    serve_link(s, attempt, announced)
                    announced = True  # serve_link 正常返回 = 曾成功注册
                except _HalfOpen:
                    print("[ray-gw] parrot silent >10s — half-open, reconnecting",
                          file=sys.stderr, flush=True)
                attempt = 0 if announced else attempt + 1

        class _HalfOpen(Exception):
            pass

        def serve_link(sock: socket.socket, _attempt: int, announced: bool) -> None:
            out_local: queue.Queue[bytes] = queue.Queue()

            def writer() -> None:
                while True:
                    sock.sendall(out_local.get())

            threading.Thread(target=writer, daemon=True).start()

            from concurrent.futures import ThreadPoolExecutor

            pool_local = ThreadPoolExecutor(max_workers=RAY_GW_ASK_WORKERS)

            # B3（DEV_09）：admin-v2 执行器（回帧经 out_local——writer 线程）
            admin_local = RayAdminExecutor(out_local, gateway_dispatcher=dispatch,
                                           worker_ref=worker)

            def reply(cid: int, key: str, payload: bytes) -> None:
                out_local.put(build_frame(FT_REPLY, cid, "", key, payload))

            def reply_err(cid: int, code: int, detail: str) -> None:
                out_local.put(build_frame(FT_REPLY_ERR, cid, "", "", encode_err_payload(code, detail)))

            def run_ask(cid: int, type_key: str, payload: bytes) -> None:
                try:
                    rkey, rpayload = ray.get(worker.ask.remote(type_key, payload))
                    reply(cid, rkey, rpayload)
                except Exception as e:  # noqa: BLE001
                    reply_err(cid, ERR_UNKNOWN_TYPE_KEY, str(e))

            # 方案 A：直连学习表 node → socket（hint 到达后台拨号建立）
            direct_links: dict[str, socket.socket] = {}

            def on_route_hint(payload: bytes) -> None:
                try:
                    nlen = int.from_bytes(payload[0:2], "little")
                    node = payload[2:2 + nlen].decode()
                    alen = int.from_bytes(payload[2 + nlen:4 + nlen], "little")
                    addr = payload[4 + nlen:4 + nlen + alen].decode()
                except Exception:  # noqa: BLE001
                    return
                if node in direct_links:
                    return

                def dial() -> None:
                    h, _, p = addr.partition(":")
                    try:
                        ds = socket.create_connection((h, int(p)), timeout=10)
                        ds.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                        # 客户端握手（对端 accept 侧回 ACK）
                        out_d: queue.Queue[bytes] = queue.Queue()

                        def w_d() -> None:
                            while True:
                                ds.sendall(out_d.get())

                        threading.Thread(target=w_d, daemon=True).start()
                        out_d.put(build_frame(FT_HANDSHAKE, 1, "", "__handshake__",
                                              handshake_body("ray-gw-1")))
                        # 简化：不等待 ACK 即登记（对端 accept 逻辑固定回 ACK；
                        # 失败由 socket 异常清理）。直连承载 ASK/TELL 出站。
                        direct_links[node] = ds
                        print(f"[ray-gw] direct link to {node} up", file=sys.stderr, flush=True)
                    except OSError as e:
                        print(f"[ray-gw] direct dial {node} failed: {e!r}",
                              file=sys.stderr, flush=True)

                threading.Thread(target=dial, daemon=True).start()

            dec_local = FrameDecoder()
            handshake_done = False
            last_inbound = _time.monotonic()
            if not announced:
                print(f"RAY_GW_REGISTERED={parrot_addr}", flush=True)
            out_local.put(build_frame(FT_HANDSHAKE, 1, "", "__handshake__",
                                      handshake_body("ray-gw-1")))
            sock.settimeout(5.0)  # 半开检测唤醒周期
            try:
                while True:
                    try:
                        chunk = sock.recv(65536)
                    except socket.timeout:
                        if _time.monotonic() - last_inbound > 10.0:
                            raise _HalfOpen()
                        continue
                    if not chunk:
                        return
                    last_inbound = _time.monotonic()
                    dec_local.feed(chunk)
                    while (f := dec_local.next_frame()) is not None:
                        try:
                            if not handshake_done:
                                if f.ft == FT_HANDSHAKE_ACK:
                                    print(f"[ray-gw] parrot {parrot_addr} handshake ok",
                                          file=sys.stderr, flush=True)
                                    handshake_done = True
                                continue
                            if f.ft == FT_ASK:
                                reply_to, real_payload = split_reply_to(f.payload) or ("", f.payload)
                                _ = reply_to
                                pool_local.submit(run_ask, f.cid, f.type_key, real_payload)
                            elif f.ft == FT_TELL:
                                worker.deliver.remote(f.type_key, f.payload)
                            elif f.ft == FT_HEARTBEAT:
                                out_local.put(build_frame(FT_HEARTBEAT_ACK, f.cid, "", "", b""))
                            elif f.ft == FT_ROUTE_HINT:
                                on_route_hint(f.payload)
                            elif f.ft == FT_SYSTEM_EVENT:
                                # B3：admin-v2（0x03 命令）——执行器消费；其余忽略
                                admin_local.on_frame(f.cid, f.path, f.payload)
                        except Exception as loop_err:  # noqa: BLE001
                            print(f"[ray-gw] frame loop error: {loop_err!r}",
                                  file=sys.stderr, flush=True)
            finally:
                pool_local.shutdown(wait=False)
                sock.close()

        connect_loop()
        return

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    # DEV_08：大帧突发下内核缓冲放大（wire 1.0 上限 16MiB 帧体）
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1 << 20)
    srv.bind(("127.0.0.1", port))
    srv.listen(128)
    print(f"RAY_GW_PORT={srv.getsockname()[1]}", flush=True)

    sock, _addr = srv.accept()
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    srv.close()

    out: queue.Queue[bytes] = queue.Queue()

    def writer() -> None:
        while True:
            sock.sendall(out.get())

    threading.Thread(target=writer, daemon=True).start()

    # DEV_08：ASK 执行池——ray.get 阻塞调用移出收包线程（头阻塞消除）。
    # max_workers 可注入性保留在模块级（测试/生产分别调优）。
    from concurrent.futures import ThreadPoolExecutor

    pool = ThreadPoolExecutor(max_workers=RAY_GW_ASK_WORKERS)

    def reply(cid: int, key: str, payload: bytes) -> None:
        out.put(build_frame(FT_REPLY, cid, "", key, payload))

    def reply_err(cid: int, code: int, detail: str) -> None:
        out.put(build_frame(FT_REPLY_ERR, cid, "", "", encode_err_payload(code, detail)))

    def run_ask(cid: int, type_key: str, payload: bytes) -> None:
        try:
            rkey, rpayload = ray.get(worker.ask.remote(type_key, payload))
            reply(cid, rkey, rpayload)
        except Exception as e:  # noqa: BLE001
            reply_err(cid, ERR_UNKNOWN_TYPE_KEY, str(e))

    # B3（DEV_09）：admin-v2 执行器（回帧经 out 队列——writer 线程）
    # R4：dispatcher + worker 传入——deploy 组件挂载路由（actor 进程内）
    admin_exec = RayAdminExecutor(out, gateway_dispatcher=dispatch, worker_ref=worker)

    dec = FrameDecoder()
    handshake_done = False
    while True:
        try:
            chunk = sock.recv(65536)
        except OSError:
            break
        if not chunk:
            break
        dec.feed(chunk)
        while (f := dec.next_frame()) is not None:
            try:
                if not handshake_done:
                    if f.type_key == "__handshake__" or f.ft == FT_HANDSHAKE:
                        fields = dict(parse_tlv(f.payload))
                        peer = fields.get(1, b"?").decode()
                        print(f"[ray-gw] handshake from {peer}", file=sys.stderr, flush=True)
                        out.put(
                            build_frame(
                                FT_HANDSHAKE_ACK,
                                f.cid,
                                "",
                                "__handshake__",
                                handshake_ack_body("ray-gw-1"),
                            )
                        )
                        handshake_done = True
                    continue
                if f.ft == FT_ASK:
                    reply_to, real_payload = split_reply_to(f.payload) or ("", f.payload)
                    _ = reply_to  # 回程经 cid 配对（reply_to 仅诊断）
                    pool.submit(run_ask, f.cid, f.type_key, real_payload)
                elif f.ft == FT_TELL:
                    worker.deliver.remote(f.type_key, f.payload)
                elif f.ft == FT_HEARTBEAT:
                    out.put(build_frame(FT_HEARTBEAT_ACK, f.cid, "", "", b""))
                elif f.ft == FT_SYSTEM_EVENT:
                    # B3（DEV_09）：admin-v2（0x03 命令）——执行器消费；其余忽略
                    admin_exec.on_frame(f.cid, f.path, f.payload)
            except Exception as loop_err:  # noqa: BLE001
                print(f"[ray-gw] frame loop error: {loop_err!r}", file=sys.stderr, flush=True)
    pool.shutdown(wait=False)


def main(argv: list[str]) -> None:
    port = int(argv[1]) if len(argv) > 1 else 9851
    # 双模式组网：argv[2] = "parrot=host:port" → 注册模式（主动拨号 parrot）
    parrot_addr = None
    if len(argv) > 2 and argv[2].startswith("parrot="):
        parrot_addr = argv[2][len("parrot="):]
    d = ParrotDispatcher()

    @d.handler("bin:u:Ping")
    def _ping(_k: str, p: bytes) -> tuple[str, bytes]:
        (n,) = struct.unpack("<Q", p)
        return ("bin:u:Pong", struct.pack("<Q", n + 2))  # ray 方言 +2

    @d.handler("bin:u:Add")
    def _add(_k: str, p: bytes) -> tuple[str, bytes]:
        a, b = struct.unpack("<QQ", p)
        return ("bin:u:AddR", struct.pack("<Q", a + b + 1000))  # ray 方言 +1000

    # R4（应用体系架构纠正）：网关不再内置业务 handler——业务组件经
    # Deploy{PyModule} 载入（ray 网关 _launch importlib 载入 app 模块，
    # dispatcher 融合：已部署组件 handler 优先，网关探针兜底）。


    serve(port, d, parrot_addr=parrot_addr)


if __name__ == "__main__":
    main(sys.argv)
