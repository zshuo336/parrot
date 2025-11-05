import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import { setTimeout as delay } from 'node:timers/promises';
import { ParrotLite, FrameDecoder, FT, buildFrame } from '../dist/index.js';
const empty = new Uint8Array();
const u32 = n => { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; };
const tlv = (tag, b) => Buffer.concat([Buffer.from([tag, b.length & 255, b.length >> 8]), b]);
function ack(limit, extended) {
  const fields = [tlv(1, Buffer.from('peer')), tlv(4, u32(2)), tlv(5, u32(limit)), tlv(6, Buffer.from([0])), tlv(9, Buffer.from('pb'))];
  if (extended) fields.push(Buffer.from(tlv(255, Buffer.alloc(40)).toString('hex').repeat(32768), 'hex'));
  return buildFrame(FT.HANDSHAKE_ACK, 1n, '', '', Buffer.concat(fields));
}
async function until(condition) {
  const end = performance.now() + 2000;
  while (!condition()) { assert.ok(performance.now() < end, 'TCP event deadline'); await delay(2); }
}
for (const limit of [28, 256, 2 << 20]) for (const pipelined of [false, true]) {
  test(`TCP negotiated limit ${limit} with pipelined=${pipelined}`, { timeout: 6000 }, async () => {
    const negotiated = Math.min(limit, 1 << 20);
    const sockets = new Set(), errors = [];
    let closed = false, sentExact = false, heartbeat = false, node;
    const server = net.createServer(sock => {
      sockets.add(sock); sock.on('error', () => {});
      sock.on('close', () => { sockets.delete(sock); closed = true; });
      const decoder = new FrameDecoder();
      sock.on('data', data => {
        try {
          decoder.feed(data);
          for (let f = decoder.nextFrame(); f; f = decoder.nextFrame()) {
            if (f.ft === FT.HANDSHAKE) {
              sock.write(Buffer.concat([ack(limit, limit > (1 << 20)), pipelined ? u32(negotiated + 1) : Buffer.alloc(0)]));
            } else if (f.ft === FT.TELL) {
              assert.equal(28 + f.payload.length, negotiated);
              assert.equal(f.path, ''); assert.equal(f.typeKey, ''); sentExact = true;
              // An exact-bound input remains valid, followed by a 28-byte heartbeat.
              sock.write(Buffer.concat([buildFrame(FT.TELL, 2n, '', '', new Uint8Array(negotiated - 28)), buildFrame(FT.HEARTBEAT, 3n, '', '', empty)]));
            } else if (f.ft === FT.HEARTBEAT_ACK) {
              heartbeat = true; sock.write(u32(negotiated + 1));
            } else assert.fail(`unexpected type ${f.ft}`);
          }
        } catch (error) { errors.push(error); sock.destroy(); }
      });
    });
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    try {
      node = await ParrotLite.connect({ url: `tcp://127.0.0.1:${server.address().port}`, nodeId: 'client', reconnectDelayMs: 30000 });
      if (!pipelined) {
        assert.throws(() => node.tell('', '', new Uint8Array(negotiated - 28 + 1)), /large|limit/);
        node.tell('', '', new Uint8Array(negotiated - 28));
      }
      await until(() => closed);
      if (!pipelined) assert.ok(sentExact && heartbeat, 'rejected local input must not poison the connection');
      assert.deepEqual(errors, []);
    } finally {
      node?.close(); for (const sock of sockets) sock.destroy();
      await new Promise(resolve => server.close(resolve));
    }
  });
}

test('negotiated decoder changes preserve partial input and invalid changes preserve the bound', () => {
  const wire = buildFrame(FT.HEARTBEAT, 19n, '', '', empty);
  const decoder = new FrameDecoder();
  decoder.feed(wire.subarray(0, 7));
  decoder.setMaxFrameLen(28);
  for (const invalid of [0, 27, (16 << 20) + 1, NaN, Infinity, 28.5])
    assert.throws(() => decoder.setMaxFrameLen(invalid), /frame limit/);
  assert.equal(decoder.nextFrame(), null);
  decoder.feed(wire.subarray(7));
  assert.equal(decoder.nextFrame().cid, 19n);
  decoder.feed(u32(29));
  assert.throws(() => decoder.nextFrame(), /too large/);
});
