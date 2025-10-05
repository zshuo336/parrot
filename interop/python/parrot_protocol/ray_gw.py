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
    FT_TELL,
    FrameDecoder,
    build_frame,
    encode_err_payload,
    handshake_ack_body,
    parse_tlv,
    split_reply_to,
)

ERR_UNKNOWN_TYPE_KEY = 6

# DEV_08：ASK 执行池大小（慢 handler 并发度；环境变量可覆盖——
# PARROT_RAY_GW_WORKERS=1 即退化为旧行为做 A/B 对照）
RAY_GW_ASK_WORKERS = int(os.environ.get("PARROT_RAY_GW_WORKERS", "8"))


class ParrotDispatcher:
    """TYPE_KEY → handler 分发（ray 侧单例形态）。

    子类注册 handler：`@dispatcher.handler("pb:demo/Echo")`。
    """

    def __init__(self) -> None:
        self._handlers: dict[str, object] = {}

    def handler(self, type_key: str):
        def deco(fn):
            self._handlers[type_key] = fn
            return fn

        return deco

    def dispatch(self, type_key: str, payload: bytes) -> tuple[str, bytes]:
        fn = self._handlers.get(type_key)
        if fn is None:
            raise KeyError(f"unknown type key {type_key}")
        return fn(type_key, payload)


def serve(port: int, dispatcher=None, ray_kwargs: dict | None = None) -> None:
    """启动网关（阻塞）。

    - ray.init 参数可注入（测试用 num_cpus=2 / include_dashboard=False）
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
        """ParrotDispatcher 的 ray actor 化（ask=ray.get / deliver=fire-and-forget）。"""

        def __init__(self, d: ParrotDispatcher) -> None:
            self._d = d

        def ask(self, type_key: str, payload: bytes) -> tuple[str, bytes]:
            return self._d.dispatch(type_key, payload)

        def deliver(self, type_key: str, payload: bytes) -> None:
            self._d.dispatch(type_key, payload)  # 不 get（非取消语义）

    worker = RayWorker.remote(dispatch)

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
            except Exception as loop_err:  # noqa: BLE001
                print(f"[ray-gw] frame loop error: {loop_err!r}", file=sys.stderr, flush=True)
    pool.shutdown(wait=False)


def main(argv: list[str]) -> None:
    port = int(argv[1]) if len(argv) > 1 else 9851
    d = ParrotDispatcher()

    @d.handler("bin:u:Ping")
    def _ping(_k: str, p: bytes) -> tuple[str, bytes]:
        (n,) = struct.unpack("<Q", p)
        return ("bin:u:Pong", struct.pack("<Q", n + 2))  # ray 方言 +2

    @d.handler("bin:u:Add")
    def _add(_k: str, p: bytes) -> tuple[str, bytes]:
        a, b = struct.unpack("<QQ", p)
        return ("bin:u:AddR", struct.pack("<Q", a + b + 1000))  # ray 方言 +1000

    serve(port, d)


if __name__ == "__main__":
    main(sys.argv)
