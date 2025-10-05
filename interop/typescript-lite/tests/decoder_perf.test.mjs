/**
 * DEV_08 解码器性能/正确性测试（游标式重构回归锚；纯 JS——node --test 直跑）。
 *
 * 覆盖：
 *   T1 10k 帧一次性 feed：全部按序解出 + 限时（旧 O(n²) 实现在此量级
 *      需数百 ms——游标式 < 100ms）
 *   T2 逐字节喂入（极端半包）：内容零丢失
 *   T3 混合尺寸帧（1B..4KB 载荷）：往返逐帧校验
 */

import test from "node:test";
import assert from "node:assert/strict";
import { FrameDecoder, buildFrame, FT } from "../dist/wire.js";

function mkFrame(i, payloadLen) {
  const payload = new Uint8Array(payloadLen);
  payload.fill(i & 0xff);
  return buildFrame(FT.TELL, BigInt(i), "/user/bench", "bin:u:Tick", payload);
}

test("T1: 10k frames bulk decode under 100ms", () => {
  const N = 10_000;
  const frames = [];
  for (let i = 0; i < N; i++) frames.push(mkFrame(i, 16));
  const total = frames.reduce((n, f) => n + f.length, 0);
  const wire = new Uint8Array(total);
  let off = 0;
  for (const f of frames) {
    wire.set(f, off);
    off += f.length;
  }

  const dec = new FrameDecoder();
  const t0 = performance.now();
  dec.feed(wire);
  let got = 0;
  let last = null;
  for (let f = dec.nextFrame(); f; f = dec.nextFrame()) {
    last = f;
    assert.equal(f.cid, BigInt(got), `frame order at ${got}`);
    got++;
  }
  const ms = performance.now() - t0;
  assert.equal(got, N, "all frames decoded");
  assert.equal(last.typeKey, "bin:u:Tick");
  assert.ok(ms < 100, `bulk decode took ${ms.toFixed(1)}ms (budget 100ms)`);
});

test("T2: byte-by-byte feed keeps content intact", () => {
  const f1 = mkFrame(7, 32);
  const f2 = mkFrame(8, 5);
  const wire = new Uint8Array(f1.length + f2.length);
  wire.set(f1, 0);
  wire.set(f2, f1.length);

  const dec = new FrameDecoder();
  const out = [];
  for (let i = 0; i < wire.length; i++) {
    dec.feed(wire.subarray(i, i + 1));
    for (let f = dec.nextFrame(); f; f = dec.nextFrame()) out.push(f);
  }
  assert.equal(out.length, 2);
  assert.equal(out[0].cid, 7n);
  assert.equal(out[1].cid, 8n);
  assert.equal(out[0].payload.length, 32);
  assert.equal(out[1].payload.length, 5);
  assert.ok(out[0].payload.every((b) => b === 7));
  assert.ok(out[1].payload.every((b) => b === 8));
});

test("T3: mixed frame sizes roundtrip", () => {
  const sizes = [1, 4, 64, 1024, 4096, 3, 300];
  const frames = sizes.map((s, i) => mkFrame(i, s));
  const total = frames.reduce((n, f) => n + f.length, 0);
  const wire = new Uint8Array(total);
  let off = 0;
  for (const f of frames) {
    wire.set(f, off);
    off += f.length;
  }
  const dec = new FrameDecoder();
  dec.feed(wire);
  let i = 0;
  for (let f = dec.nextFrame(); f; f = dec.nextFrame()) {
    assert.equal(f.cid, BigInt(i));
    assert.equal(f.payload.length, sizes[i]);
    i++;
  }
  assert.equal(i, sizes.length);
});
