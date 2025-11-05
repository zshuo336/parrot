import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import test from 'node:test';
import { FrameDecoder } from '../dist/wire.js';

const cases = readFileSync(new URL('../../../docs/vectors/wire1-boundaries.tsv', import.meta.url), 'utf8')
  .trimEnd().split('\n').filter(line => !line.startsWith('#')).map(line => line.split('\t'));
for (const [name, expected, encoded] of cases) {
  test(`shared wire boundary: ${name}`, () => {
    const decoder = new FrameDecoder();
    decoder.feed(Buffer.from(encoded, 'hex'));
    if (expected === 'reject') assert.throws(() => decoder.nextFrame());
    else {
      const frame = decoder.nextFrame();
      assert.equal(frame !== null, expected === 'frame');
      if (frame) assert.equal(frame.cid, 0x123456789abcdef0n);
    }
  });
}
test('every valid split and pipelined tail', () => {
  for (const [, expected, encoded] of cases) {
    if (expected !== 'frame') continue;
    const wire = Buffer.from(encoded, 'hex');
    for (let split = 0; split < wire.length; split++) {
      const decoder = new FrameDecoder();
      decoder.feed(wire.subarray(0, split));
      assert.equal(decoder.nextFrame(), null);
      decoder.feed(Buffer.concat([wire.subarray(split), wire]));
      assert.equal(decoder.nextFrame().cid, 0x123456789abcdef0n);
      assert.equal(decoder.nextFrame().cid, 0x123456789abcdef0n);
      assert.equal(decoder.nextFrame(), null);
    }
  }
});
