import test from 'node:test';
import assert from 'node:assert/strict';
import { parseTlv, splitReplyTo, FrameDecoder, FT, buildFrame } from '../dist/index.js';

function input(bytes, kind, offset) {
  const storage = kind === 'Buffer' ? Buffer.alloc(bytes.length + offset + 5, 0xee)
    : new Uint8Array(bytes.length + offset + 5).fill(0xee);
  storage.set(bytes, offset);
  return { storage, view: storage.subarray(offset, offset + bytes.length) };
}

test('decoder preserves unread frames across compaction, growth and reused input buffers', () => {
  const frames = Array.from({ length: 384 }, (_, n) => buildFrame(FT.TELL, BigInt(n),
    `/目录/${n}`, 'pb:字节', new Uint8Array((n * 991) % 8192).fill(n & 255)));
  const stream = Buffer.concat(frames);
  const decoder = new FrameDecoder();
  const chunkSizes = [1, 31, 4093, 65536, 257];
  let received = 0, turn = 0;
  const kept = [];
  function receive() {
    const frame = decoder.nextFrame();
    if (!frame) return false;
    assert.equal(frame.cid, BigInt(received));
    assert.equal(frame.path, `/目录/${received}`);
    assert.equal(frame.typeKey, 'pb:字节');
    assert.equal(frame.payload.length, (received * 991) % 8192);
    assert.ok(frame.payload.every(byte => byte === (received & 255)));
    kept.push(frame.payload);
    received++;
    return true;
  }
  for (let offset = 0; offset < stream.length; turn++) {
    const length = Math.min(chunkSizes[turn % chunkSizes.length], stream.length - offset);
    const scratch = Buffer.from(stream.subarray(offset, offset + length));
    decoder.feed(scratch);
    scratch.fill(0);
    // Intentionally retain unread complete frames while new input arrives.
    for (let n = 0; n < turn % 7; n++) if (!receive()) break;
    offset += length;
  }
  while (receive()) {}
  assert.equal(received, frames.length);
  for (let n = 0; n < kept.length; n++) assert.ok(kept[n].every(byte => byte === (n & 255)));
});

for (const kind of ['Uint8Array', 'Buffer']) for (const offset of [0, 11]) {
  test(`TLV values own bytes for ${kind} at offset ${offset}`, () => {
    const original = [255, 2, 0, 7, 8, 1, 1, 0, 9, 255, 1, 0, 10];
    const { storage, view } = input(original, kind, offset);
    const fields = parseTlv(view);
    assert.deepEqual(fields.map(([tag, value]) => [tag, [...value]]),
      [[255, [7, 8]], [1, [9]], [255, [10]]]);
    fields[0][1][0] = 42;
    assert.deepEqual([...view], original, 'returned value mutation must not change caller input');
    view.fill(0);
    assert.deepEqual(fields.map(([tag, value]) => [tag, [...value]]),
      [[255, [42, 8]], [1, [9]], [255, [10]]], 'caller buffer reuse must not change decoded fields');
    assert.ok(storage.subarray(0, offset).every(byte => byte === 0xee));
    assert.ok(storage.subarray(offset + original.length).every(byte => byte === 0xee));
  });

  test(`ASK payload owns bytes for ${kind} at offset ${offset}`, () => {
    // Independent wire prefix: two UTF-8 bytes for /r, then opaque payload.
    const original = [2, 0, 0, 0, 47, 114, 7, 8, 9];
    const { view } = input(original, kind, offset);
    const result = splitReplyTo(view);
    assert.equal(result.replyTo, '/r');
    assert.deepEqual([...result.payload], [7, 8, 9]);
    result.payload[0] = 42;
    assert.deepEqual([...view], original, 'handler payload mutation must not change caller input');
    view.fill(0);
    assert.equal(result.replyTo, '/r');
    assert.deepEqual([...result.payload], [42, 8, 9], 'caller buffer reuse must not change handler payload');
  });

  test(`frame decoder snapshots fragmented ${kind} input at offset ${offset}`, () => {
    // Independent frame bytes: body length 31, TELL, cid=1, hop limit=8,
    // empty path/key, three payload bytes. Exercise reuse before and after decode.
    const wire = [31,0,0,0, 1,0x13,0,0, 1,0,0,0,0,0,0,0,
      0,8,0,0,0,0,0,0, 0,0,0,0, 0,0,0,0, 7,8,9];
    const decoder = new FrameDecoder();
    for (const fragment of [wire.slice(0, 17), wire.slice(17)]) {
      const { view } = input(fragment, kind, offset);
      decoder.feed(view);
      view.fill(0);
    }
    const decoded = decoder.nextFrame();
    assert.equal(decoded.ft, FT.TELL);
    assert.equal(decoded.cid, 1n);
    assert.deepEqual([...decoded.payload], [7, 8, 9]);
    decoder.feed(new Uint8Array(wire));
    const next = decoder.nextFrame();
    next.payload.fill(0);
    assert.deepEqual([...decoded.payload], [7, 8, 9]);
    assert.equal(decoder.nextFrame(), null);
  });
}
