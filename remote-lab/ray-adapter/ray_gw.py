"""Parrot Ray Gateway POC.

Rust 节点 <--Parrot Wire(LE 帧)--> 本网关 <--ray API--> Ray 集群 actor。

帧格式（与 rust POC / Java AkkaGw 完全一致）：
  [u32 frame_len][u8 ver][u8 ft][u16 flags][u64 cid][u64 reserved]
  [u32 path_len][path][u32 key_len][key][payload]
ASK=0x10 REPLY=0x11 REPLY_ERR=0x12 TELL=0x13

用法：python3 ray_gw.py <port>
"""
import socket
import struct
import sys
import threading
import time
import queue

import ray

ASK, REPLY, REPLY_ERR, TELL = 0x10, 0x11, 0x12, 0x13

# ---------------- Ray 侧 worker actor ----------------

@ray.remote
class EchoWorker:
    """Ray actor：模拟云端任务执行体。"""
    def __init__(self, node_id: str):
        self.node_id = node_id
        self.count = 0

    def handle(self, type_key: str, payload: bytes) -> tuple[str, bytes]:
        self.count += 1
        if type_key == "bin:u:Ping":
            n = struct.unpack("<Q", payload)[0]
            # ray 方言：n+2（可辨识确实到了 ray）
            return ("bin:u:Pong", struct.pack("<Q", n + 2))
        if type_key == "bin:u:Add":
            a, b = struct.unpack("<QQ", payload)
            return ("bin:u:AddR", struct.pack("<Q", a + b + 1000))  # ray 方言 +1000
        raise ValueError(f"unknown key {type_key}")

    def tell(self, type_key: str, payload: bytes) -> None:
        self.count += 1


# ---------------- 帧编解码（与 rust/Java 一致） ----------------

def build_frame(ft: int, cid: int, path: bytes, key: bytes, payload: bytes) -> bytes:
    body_len = 28 + len(path) + len(key) + len(payload)
    return (
        struct.pack("<I", body_len)
        + struct.pack("<BBH", 1, ft, 0)
        + struct.pack("<Q", cid)
        + struct.pack("<Q", 0)            # reserved
        + struct.pack("<I", len(path)) + path
        + struct.pack("<I", len(key)) + key
        + payload
    )

def recv_exact(sock, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("eof")
        buf += chunk
    return buf

def read_frame(sock):
    (body_len,) = struct.unpack("<I", recv_exact(sock, 4))
    body = recv_exact(sock, body_len)
    ver, ft, _flags = struct.unpack("<BBH", body[:4])
    (cid,) = struct.unpack("<Q", body[4:12])
    struct.unpack("<Q", body[12:20])      # reserved
    (path_len,) = struct.unpack("<I", body[20:24])
    path = body[24:24 + path_len]
    off = 24 + path_len
    (key_len,) = struct.unpack("<I", body[off:off + 4])
    key = body[off + 4:off + 4 + key_len]
    payload = body[off + 4 + key_len:]
    return ver, ft, cid, path.decode(), key.decode(), payload

# ---------------- 网关主体 ----------------

def main(port: int):
    ray.init(num_cpus=2, include_dashboard=False, log_to_driver=False)
    worker = EchoWorker.options(name="parrot_echo").remote("ray-node-1")
    print(f"[ray-gw] ray ready, worker={worker}", flush=True)

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port))
    srv.listen(1)
    print(f"[ray-gw] listening on {port}", flush=True)

    sock, addr = srv.accept()
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    print(f"[ray-gw] rust node connected: {addr}", flush=True)
    srv.close()

    out_q: queue.Queue = queue.Queue()

    def writer():
        while True:
            frame = out_q.get()
            sock.sendall(frame)

    threading.Thread(target=writer, daemon=True).start()

    # 主动 ask rust 侧（双向验证）
    def ask_rust():
        time.sleep(1.0)
        cid = int(time.time() * 1000) % (1 << 62)
        out_q.put(build_frame(ASK, cid, b"/user/rust_service",
                              b"bin:u:Ping", struct.pack("<Q", 100)))
        deadline = time.time() + 5
        while time.time() < deadline and cid not in done_map:
            time.sleep(0.05)
        if cid in done_map:
            (n,) = struct.unpack("<Q", done_map[cid])
            print(f"[ray-gw] ask rust /user/rust_service Ping(100) -> {n}", flush=True)
        else:
            print("[ray-gw] ask rust TIMEOUT", flush=True)

    done_map: dict[int, bytes] = {}
    threading.Thread(target=ask_rust, daemon=True).start()

    while True:
        ver, ft, cid, path, key, payload = read_frame(sock)
        if ver != 1:
            continue
        if ft == ASK:
            try:
                reply_key, reply_payload = ray.get(worker.handle.remote(key, payload))
                out_q.put(build_frame(REPLY, cid, b"", reply_key.encode(), reply_payload))
            except Exception as e:  # noqa: BLE001
                out_q.put(build_frame(REPLY_ERR, cid, b"", b"", str(e).encode()))
        elif ft == TELL:
            worker.tell.remote(key, payload)
        elif ft == REPLY:
            done_map[cid] = payload
        elif ft == REPLY_ERR:
            done_map[cid] = None
            print(f"[ray-gw] reply_err cid={cid}", flush=True)

if __name__ == "__main__":
    main(int(sys.argv[1]) if len(sys.argv) > 1 else 9841)
