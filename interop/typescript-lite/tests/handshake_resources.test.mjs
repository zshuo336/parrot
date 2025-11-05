import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { parseTlv } from '../dist/index.js';

test('network ACK decoding stays within a bounded heap for large unknown and duplicate TLV counts', { timeout: 15000 }, () => {
  // Isolate V8's heap budget from the runner and unrelated concurrent tests.
  const moduleUrl = new URL('../dist/wire.js', import.meta.url).href;
  const program = `
    import assert from 'node:assert/strict';
    import { decodeHandshakeAck } from ${JSON.stringify(moduleUrl)};
    const base = Buffer.from([1,1,0,112,4,4,0,2,0,0,0,5,4,0,0,0,16,0,6,1,0,0,9,2,0,112,98]);
    const fields = Buffer.alloc(400000 * 3);
    for (let n = 0; n < fields.length; n += 3) fields[n] = 255;
    const extended = Buffer.concat([base, fields]);
    assert.equal(decodeHandshakeAck(extended).nodeId, 'p');
    assert.throws(() => decodeHandshakeAck(Buffer.concat([extended, Buffer.from([255])])), /truncated/);
    for (let n = 0; n < fields.length; n += 3) fields[n] = 1;
    assert.throws(() => decodeHandshakeAck(Buffer.concat([base, fields])), /duplicate/);
    assert.throws(() => decodeHandshakeAck(Buffer.concat([base, fields, Buffer.from([255])])), /truncated/);
    console.log('bounded ACK cases passed');
  `;
  const result = spawnSync(process.execPath, ['--max-old-space-size=32', '--input-type=module', '-e', program],
    { encoding: 'utf8', timeout: 12000, maxBuffer: 1024 * 1024 });
  assert.equal(result.error, undefined);
  assert.equal(result.status, 0, `signal=${result.signal}; ${result.stderr.slice(0, 1500) + result.stderr.slice(-500)}`);
  assert.match(result.stdout, /bounded ACK cases passed/);
});

test('generic TLV API still owns all fields and preserves repeated unknown tags', () => {
  const bytes = new Uint8Array([255,1,0,7,1,1,0,8,255,1,0,9]);
  const fields = parseTlv(bytes); bytes.fill(0);
  assert.deepEqual(fields.map(([tag, value]) => [tag, Array.from(value)]), [[255,[7]],[1,[8]],[255,[9]]]);
});
