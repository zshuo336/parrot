/**
 * golden vectors 逐字节对齐（docs/vectors/wire1.json）+ 半包语义（DEV_03 §2.4）。
 * 运行：node --test tests/（node:test 原生——零 jest 依赖）
 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";
import assert from "node:assert/strict";

import {
  FT,
  FrameDecoder,
  buildFrame,
  decodeErrPayload,
  encodeErrPayload,
  handshakeAckBody,
  parseTlv,
} from "../dist/wire.js";

const VECTORS = JSON.parse(
  readFileSync(resolve(process.cwd(), "../../docs/vectors/wire1.json"), "utf8"),
);

const hexToBytes = (hex) =>
  new Uint8Array((hex.match(/../g) ?? []).map((h) => parseInt(h, 16)));

test("frozen contract", () => {
  assert.equal(VECTORS.frozen, true);
});

for (const vec of VECTORS.vectors) {
  test(`roundtrip ${vec.name}`, () => {
    const frame = buildFrame(
      parseInt(vec.frame_type, 16),
      BigInt(vec.correlation_id),
      vec.path,
      vec.type_key,
      hexToBytes(vec.payload_hex),
    );
    assert.equal(Buffer.from(frame).toString("hex"), vec.bytes_hex);
  });

  test(`decode ${vec.name}`, () => {
    const dec = new FrameDecoder();
    dec.feed(hexToBytes(vec.bytes_hex));
    const f = dec.nextFrame();
    assert.ok(f);
    assert.equal(f.cid, BigInt(vec.correlation_id));
    assert.equal(f.ft, parseInt(vec.frame_type, 16));
    assert.equal(f.path, vec.path);
    assert.equal(f.typeKey, vec.type_key);
    assert.equal(Buffer.from(f.payload).toString("hex"), vec.payload_hex);
    assert.equal(dec.nextFrame(), null);
  });
}

test("partial: byte-by-byte feed emits only on complete", () => {
  const frame = buildFrame(FT.ASK, 7n, "/user/x", "bin:t::M", new Uint8Array([42]));
  const dec = new FrameDecoder();
  for (let i = 0; i < frame.length; i++) {
    dec.feed(frame.slice(i, i + 1));
    if (i < frame.length - 1) assert.equal(dec.nextFrame(), null);
  }
  assert.equal(dec.nextFrame()?.cid, 7n);
});

test("pipeline two frames", () => {
  const f1 = buildFrame(FT.REPLY, 1n, "", "bin:t::R", new Uint8Array([1, 2]));
  const f2 = buildFrame(FT.REPLY_ERR, 2n, "", "", encodeErrPayload(7, "route unreachable"));
  const dec = new FrameDecoder();
  dec.feed(new Uint8Array([...f1, ...f2]));
  const a = dec.nextFrame();
  const b = dec.nextFrame();
  assert.equal(a.cid, 1n);
  assert.equal(b.cid, 2n);
  const { code, detail } = decodeErrPayload(b.payload);
  assert.equal(code, 7);
  assert.ok(detail.includes("route"));
});

test("handshake tlv: mandatory fields + chosen codec pb", () => {
  const body = handshakeAckBody("ts-lite-1");
  const tags = parseTlv(body).map(([t]) => t);
  for (const t of [1, 4, 5, 6, 7, 8]) assert.ok(tags.includes(t));
  const fields = new Map(parseTlv(body));
  assert.equal(new TextDecoder().decode(fields.get(1)), "ts-lite-1");
  const caps = new DataView(fields.get(4).buffer, fields.get(4).byteOffset).getUint32(0, true);
  assert.equal(caps, 0x02); // pb-only（07 §8.1）
  assert.equal(new TextDecoder().decode(fields.get(8)), "pb");
});
