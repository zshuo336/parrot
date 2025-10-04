/**
 * Rust ↔ TS(lite) 握手协商 + 互操作（DEV_03 §2.4 handshake_negotiation）。
 *
 * 用法：node tests/interop_with_rust.mjs <rust-gw-port>
 * （Rust 侧由 parrot-remote 的 interop_lite 二进制提供 echo actor）
 */
import assert from "node:assert/strict";
import { ParrotLite } from "../dist/lite.js";

const port = process.argv[2] ?? "9871";
const url = `ws://127.0.0.1:${port}`;

// Rust 网关直连形态：lite 原生 WS 二进制帧（Rust 侧 TcpTransport 不说 WS——
// 本互测经桥层语义对齐：直接 TCP 帧流。node 无原生 TCP 客户端的 WS 语义，
// 用 net.Socket 承载（lite 的 wsFactory 注入最小 TCP 面）
import net from "node:net";

function tcpWs(urlStr) {
  const u = new URL(urlStr);
  const sock = net.connect(Number(u.port), u.hostname);
  const ws = {
    sent: [],
    onmessage: null,
    onclose: null,
    onopen: null,
    onerror: null,
    readyState: 0,
    send(d) {
      sock.write(Buffer.from(d));
    },
    close() {
      sock.destroy();
    },
  };
  sock.on("connect", () => {
    ws.readyState = 1;
    ws.onopen?.();
  });
  sock.on("data", (buf) => {
    ws.onmessage?.({ data: buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.length) });
  });
  sock.on("close", () => {
    ws.readyState = 3;
    ws.onclose?.();
  });
  sock.on("error", () => ws.onerror?.());
  return ws;
}

const node = await ParrotLite.connect({ url, nodeId: "ts-lite-1", wsFactory: tcpWs });
console.log("handshake ok (ACK received, no error)");

// 本地 actor：Rust → lite 方向 ask
node.spawn("lite-echo", {
  onAsk: async (_key, payload) => payload, // 原样回
});

// lite → Rust 方向 ask（Rust 网关 echo）
const reply = await node.ask("/user/echo", "bin:u:Echo", new Uint8Array([1, 2, 3]));
console.log("rust echo reply bytes:", reply.length);
assert.ok(reply.length >= 3, "echo payload returned");

node.close();
console.log("LITE-INTEROP PASS");
