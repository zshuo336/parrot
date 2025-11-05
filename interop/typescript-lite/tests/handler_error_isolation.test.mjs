import assert from 'node:assert/strict';
import net from 'node:net';
import test from 'node:test';
import { ParrotLite, FrameDecoder, FT, buildFrame, decodeErrPayload, withReplyToPrefix } from '../dist/index.js';

const empty = new Uint8Array();
const turn = () => new Promise(resolve => setImmediate(resolve));
function decode(bytes) {
  const decoder = new FrameDecoder();
  decoder.feed(bytes);
  return decoder.nextFrame();
}
function ack(limit) {
  // Independent ACK fields: NODE_ID, CAPS_PB, MAX_FRAME_LEN, ROLE, CODEC.
  const body = Buffer.from([1,4,0,112,101,101,114,4,4,0,2,0,0,0,5,4,0,0,0,0,0,6,1,0,0,9,2,0,112,98]);
  body.writeUInt32LE(limit, 17);
  return buildFrame(FT.HANDSHAKE_ACK, 1n, '', '', body);
}
async function ready(limit = 1 << 20) {
  const ws = {
    readyState: 1, bufferedAmount: 0, sent: [],
    onopen: null, onclose: null, onmessage: null, onerror: null,
    send(bytes) {
      const frame = decode(bytes);
      this.sent.push(frame);
      if (frame.ft === FT.HANDSHAKE) this.deliver(ack(limit));
    },
    deliver(bytes) { this.onmessage?.({ data: Uint8Array.from(bytes).buffer }); },
    close() { this.readyState = 3; this.onclose?.(); },
  };
  const node = await ParrotLite.connect({ url: 'ws://fixture', nodeId: 'client', wsFactory: () => ws,
    maxPending: 1, reconnectDelayMs: 30000 });
  return { node, ws };
}

const cases = [
  { name: 'lone high surrogate', detail: '\ud800', expected: '\ufffd' },
  { name: 'lone low surrogate', detail: '\udfff', expected: '\ufffd' },
  { name: 'mixed valid text and malformed pairs', detail: 'a\ud800b\udfff\ud800\udfff', expected: 'a\ufffdb\ufffd\ud800\udfff' },
  { name: 'literal BOM remains part of error detail', detail: '\ufeffok', expected: '\ufeffok' },
  { name: 'astral scalars at negotiated byte bound', detail: '\ud83d\ude03'.repeat(9), expected: '\ud83d\ude03'.repeat(8), limit: 64 },
  { name: 'replacement bytes fit within default budget', detail: '\ud800'.repeat(342), expected: '\ufffd'.repeat(341) },
  { name: 'valid ASCII detail stays unchanged', detail: 'application failed', expected: 'application failed' },
];
for (const style of ['synchronous', 'asynchronous']) for (const item of cases) {
  test(`${style} handler error: ${item.name}`, { timeout: 5000 }, async () => {
    const { node, ws } = await ready(item.limit);
    let pending;
    try {
      node.spawn('handler', { onAsk: style === 'synchronous'
        ? () => { throw new Error(item.detail); }
        : async () => { throw new Error(item.detail); } });
      pending = node.ask('/other', 'pb:k', empty).then(bytes => ({ ok: true, bytes }), error => ({ ok: false, error }));
      const outbound = ws.sent.find(frame => frame.ft === FT.ASK);
      assert.ok(outbound, 'unrelated pending ASK was not published');
      ws.deliver(buildFrame(FT.ASK, 90n, '/user/handler', 'pb:k', withReplyToPrefix('/return', empty)));
      await turn();
      assert.equal(ws.readyState, 1, 'malformed application error text disconnected unrelated requests');
      const replies = ws.sent.filter(frame => frame.ft === FT.REPLY_ERR);
      assert.equal(replies.length, 1, 'handler must produce one protocol error');
      assert.equal(replies[0].cid, 90n);
      const error = decodeErrPayload(replies[0].payload);
      assert.equal(error.code, 12);
      assert.equal(error.detail, item.expected);
      assert.ok(28 + replies[0].payload.length <= (item.limit ?? (1 << 20)));
      ws.deliver(buildFrame(FT.REPLY, outbound.cid, '', '', new Uint8Array([7])));
      const result = await pending;
      assert.equal(result.ok, true, 'handler error cancelled an unrelated in-flight ASK');
      assert.deepEqual(Array.from(result.bytes), [7]);
      // Successful settlement must return the one available pending slot.
      const next = node.ask('/next', 'pb:k', empty);
      ws.deliver(buildFrame(FT.REPLY, ws.sent.at(-1).cid, '', '', empty));
      assert.equal((await next).length, 0);
    } finally {
      node.close();
      await pending;
    }
  });
}

test('real TCP malformed handler error does not disconnect an unrelated ASK', { timeout: 10000 }, async () => {
  for (const [detail, expected] of [['before\ud800after', 'before\ufffdafter'], ['\ufeff\udfff', '\ufeff\ufffd']]) {
    const sockets = new Set(), failures = [];
    let handlerError, peerClosed = false, pendingCid, node;
    const server = net.createServer(sock => {
      sockets.add(sock);
      sock.on('error', () => {});
      sock.on('close', () => { peerClosed = true; sockets.delete(sock); });
      const decoder = new FrameDecoder();
      sock.on('data', bytes => {
        try {
          decoder.feed(bytes);
          for (let frame = decoder.nextFrame(); frame; frame = decoder.nextFrame()) {
            if (frame.ft === FT.HANDSHAKE) sock.write(ack(1 << 20));
            else if (frame.ft === FT.ASK) {
              pendingCid = frame.cid;
              sock.write(buildFrame(FT.ASK, 91n, '/user/handler', 'pb:k', withReplyToPrefix('/return', empty)));
            } else if (frame.ft === FT.REPLY_ERR) {
              assert.equal(frame.cid, 91n);
              handlerError = decodeErrPayload(frame.payload);
              sock.write(buildFrame(FT.REPLY, pendingCid, '', '', new Uint8Array([42])));
            } else assert.fail(`unexpected frame ${frame.ft}`);
          }
        } catch (error) { failures.push(error); sock.destroy(); }
      });
    });
    await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
    try {
      node = await ParrotLite.connect({ url: `tcp://127.0.0.1:${server.address().port}`, nodeId: 'client', reconnectDelayMs: 30000 });
      node.spawn('handler', { onAsk: async () => { throw new Error(detail); } });
      const result = await node.ask('/other', 'pb:k', empty, 3000).then(bytes => ({ ok: true, bytes }), error => ({ ok: false, error }));
      assert.equal(result.ok, true, 'invalid handler error text closed the real TCP connection');
      assert.deepEqual(Array.from(result.bytes), [42]);
      assert.deepEqual(handlerError, { code: 12, detail: expected });
      assert.equal(peerClosed, false, 'connection retired instead of isolating the failing handler');
      assert.deepEqual(failures, []);
    } finally {
      node?.close();
      for (const socket of sockets) socket.destroy();
      await new Promise(resolve => server.close(resolve));
    }
  }
});

test('unacknowledged TELL failure still retires its owning connection', { timeout: 5000 }, async () => {
  const { node, ws } = await ready();
  let signal;
  try {
    node.spawn('handler', { onTell: async (_key, _payload, context) => { signal = context.signal; throw new Error('\ud800'); } });
    ws.deliver(buildFrame(FT.TELL, 92n, '/user/handler', 'pb:k', empty));
    await turn();
    assert.equal(ws.readyState, 3);
    assert.equal(signal.aborted, true);
    assert.equal(ws.sent.filter(frame => frame.ft === FT.REPLY_ERR).length, 0);
  } finally { node.close(); }
});
