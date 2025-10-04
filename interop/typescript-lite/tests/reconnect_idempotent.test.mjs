/**
 * reconnect 幂等（DEV_03 §2.4）：HANDSHAKE_ACK → registrations 重放恰一次/条目。
 * 运行：node --test tests/（node:test 原生）
 */
import test from "node:test";
import assert from "node:assert/strict";

import { ParrotLite } from "../dist/lite.js";
import { FrameDecoder, FT, buildFrame, handshakeAckBody, newCid } from "../dist/wire.js";

function mkWs() {
  return {
    sent: [],
    onmessage: null,
    onclose: null,
    onopen: null,
    onerror: null,
    readyState: 1,
    send(d) {
      this.sent.push(d);
    },
    close() {
      this.readyState = 3;
    },
  };
}

function decodeSent(ws) {
  const dec = new FrameDecoder();
  for (const s of ws.sent) dec.feed(s);
  const out = [];
  for (let f = dec.nextFrame(); f; f = dec.nextFrame()) {
    out.push({ ft: f.ft, path: f.path, key: f.typeKey });
  }
  return out;
}

test("handshake ack replays each registration exactly once", async () => {
  const stub = mkWs();
  const node = await ParrotLite.connect({
    url: "ws://x",
    nodeId: "lite-1",
    wsFactory: () => {
      queueMicrotask(() => stub.onopen?.());
      return stub;
    },
  });

  await node.register("edge/rpa", "rpa-main");
  await node.register("edge/cam", "cam-main");
  assert.equal(
    decodeSent(stub).filter((f) => f.key.startsWith("$receptionist.register:")).length,
    2,
  );

  // 重连完成：服务端 HANDSHAKE_ACK 到达 → registrations 重放
  stub.onmessage?.({
    data: buildFrame(FT.HANDSHAKE_ACK, newCid(), "", "__handshake__", handshakeAckBody("gw"))
      .slice().buffer,
  });
  const regs = decodeSent(stub).filter((f) => f.key.startsWith("$receptionist.register:"));
  assert.equal(regs.length, 4); // replay 恰好 +2（每条注册一次）
  assert.equal(regs.filter((r) => r.key.endsWith("edge/rpa")).length, 2);
  assert.equal(regs.filter((r) => r.key.endsWith("edge/cam")).length, 2);

  node.close();
});
