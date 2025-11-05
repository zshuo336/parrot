/**
 * golden vectors 逐字节对齐（docs/vectors/wire1.json）+ 半包语义（DEV_03 §2.4）。
 * 运行：node --test tests/（node:test 原生——零 jest 依赖）
 */
import { readFileSync } from "node:fs";
import { loadSharedVectors } from './support/shared-vectors.mjs';
import test from "node:test";
import assert from "node:assert/strict";

test('shared error payload corpus', () => {
  const rows = readFileSync(new URL('../../../docs/vectors/error-payloads.tsv', import.meta.url), 'utf8')
    .split('\n').filter(line => line && !line.startsWith('#')).map(line => line.split('\t'));
  assert.equal(rows.length, 30);
  for (const [name, result, payload, code, detail] of rows) {
    const bytes = Buffer.from(payload, 'hex');
    if (result === 'reject') assert.throws(() => decodeErrPayload(bytes), undefined, name);
    else {
      assert.equal(result, 'accept');
      const decoded = decodeErrPayload(bytes);
      assert.equal(decoded.code, Number(code), name);
      assert.equal(Buffer.from(decoded.detail, 'utf8').toString('hex'), detail, name);
      assert.equal(Buffer.from(encodeErrPayload(decoded.code, decoded.detail)).toString('hex'), payload, name);
    }
  }
});

test('error encoder rejects invalid codes and unpaired surrogates', () => {
  for (const code of [0, 14, -1, 65536, 1.5, NaN, true]) assert.throws(() => encodeErrPayload(code, 'error'));
  assert.throws(() => encodeErrPayload(3, '\ud800'));
});

import {
  FT,
  FrameDecoder,
  buildFrame,
  decodeErrPayload,
  encodeErrPayload,
  handshakeAckBody,
  parseTlv,
} from "../dist/wire.js";

const VECTORS = loadSharedVectors();

const hexToBytes = (hex) =>
  new Uint8Array((hex.match(/../g) ?? []).map((h) => parseInt(h, 16)));

test("frozen contract", () => {
  assert.equal(VECTORS.frozen, true);
  const names = new Set(VECTORS.vectors.map(vec => vec.name));
  for (const name of ['ask-basic', 'tell-basic', 'reply-err-stopped', 'heartbeat']) assert.ok(names.has(name));
});

function encodeDecoded(frame) {
  return buildFrame(frame.ft, frame.cid, frame.path, frame.typeKey, frame.payload,
    frame.flags, frame.hopCount, frame.hopLimit, frame.sequence);
}

for (const vec of VECTORS.vectors) {
  test(`roundtrip ${vec.name}`, () => {
    const frame = buildFrame(
      parseInt(vec.frame_type, 16),
      BigInt(vec.correlation_id),
      vec.path,
      vec.type_key,
      hexToBytes(vec.payload_hex),
      vec.flags, vec.hop_count, vec.hop_limit, vec.sequence,
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
    assert.equal(f.flags, vec.flags);
    assert.equal(f.hopCount, vec.hop_count);
    assert.equal(f.hopLimit, vec.hop_limit);
    assert.equal(f.sequence, vec.sequence);
    assert.equal(f.path, vec.path);
    assert.equal(f.typeKey, vec.type_key);
    assert.equal(Buffer.from(f.payload).toString("hex"), vec.payload_hex);
    assert.equal(Buffer.from(encodeDecoded(f)).toString('hex'), vec.bytes_hex);
    assert.equal(dec.nextFrame(), null);
  });

  test(`all split points ${vec.name}`, () => {
    const bytes = hexToBytes(vec.bytes_hex);
    for (let split = 0; split < bytes.length; split++) {
      const decoder = new FrameDecoder();
      decoder.feed(bytes.slice(0, split));
      assert.equal(decoder.nextFrame(), null, `${vec.name} split=${split}`);
      decoder.feed(bytes.slice(split));
      const frame = decoder.nextFrame();
      assert.ok(frame, `${vec.name} split=${split}`);
      assert.equal(Buffer.from(encodeDecoded(frame)).toString('hex'), vec.bytes_hex);
      assert.equal(decoder.nextFrame(), null);
    }
  });
}

test('all shared vectors pipeline preserves order and bytes', () => {
  const decoder = new FrameDecoder();
  decoder.feed(Buffer.concat(VECTORS.vectors.map(vec => Buffer.from(vec.bytes_hex, 'hex'))));
  for (const vec of VECTORS.vectors) {
    const frame = decoder.nextFrame();
    assert.ok(frame, vec.name);
    assert.equal(Buffer.from(encodeDecoded(frame)).toString('hex'), vec.bytes_hex);
  }
  assert.equal(decoder.nextFrame(), null);
});

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

test("malformed lengths and exhausted hop are rejected without overread", () => {
  const dec = new FrameDecoder();
  const bad = new Uint8Array(4 + 28);
  new DataView(bad.buffer).setUint32(0, 28, true);
  bad[4] = 1;
  bad[5] = FT.TELL;
  bad[17] = 8;
  new DataView(bad.buffer).setUint32(24, 1, true);
  assert.throws(() => { dec.feed(bad); dec.nextFrame(); }, /invalid path length/);

  const exhausted = buildFrame(FT.TELL, 1n, "/x", "k", new Uint8Array(), 0, 0, 8);
  exhausted[16] = 8;
  const d2 = new FrameDecoder();
  d2.feed(exhausted);
  assert.throws(() => d2.nextFrame(), /hop limit exceeded/);
});

test("handshake tlv: mandatory fields + chosen codec pb", () => {
  const body = handshakeAckBody("ts-lite-1");
  const tags = parseTlv(body).map(([t]) => t);
  for (const t of [1, 4, 5, 6, 7, 9]) assert.ok(tags.includes(t));
  const fields = new Map(parseTlv(body));
  assert.equal(new TextDecoder().decode(fields.get(1)), "ts-lite-1");
  const caps = new DataView(fields.get(4).buffer, fields.get(4).byteOffset).getUint32(0, true);
  assert.equal(caps, 0x02); // pb-only（07 §8.1）
  assert.equal(new TextDecoder().decode(fields.get(9)), "pb");
});
