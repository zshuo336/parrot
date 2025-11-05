import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';

test('fragmented maximum-size frames require linear buffer allocation and copying', { timeout: 20000 }, () => {
  const moduleUrl = new URL('../dist/wire.js', import.meta.url).href;
  const program = `
    import assert from 'node:assert/strict';
    import { FrameDecoder, FT } from ${JSON.stringify(moduleUrl)};
    const Native = Uint8Array;
    const nativeSet = Native.prototype.set;
    const nativeSlice = Native.prototype.slice;
    const nativeCopyWithin = Native.prototype.copyWithin;
    let allocated = 0, copied = 0;
    globalThis.Uint8Array = new Proxy(Native, {
      construct(target, args) {
        const result = Reflect.construct(target, args);
        if (typeof args[0] === 'number') allocated += result.byteLength;
        else if (args[0] !== undefined && !(args[0] instanceof ArrayBuffer)) {
          allocated += result.byteLength; copied += result.byteLength;
        }
        return result;
      }
    });
    Native.prototype.set = function(source, offset) { copied += source.length; return nativeSet.call(this, source, offset); };
    Native.prototype.slice = function(start, end) {
      const result = nativeSlice.call(this, start, end);
      allocated += result.byteLength; copied += result.byteLength; return result;
    };
    Native.prototype.copyWithin = function(target, start, end = this.length) {
      copied += end - start; return nativeCopyWithin.call(this, target, start, end);
    };
    function wire(body) {
      const bytes = new Native(body + 4);
      const view = new DataView(bytes.buffer);
      view.setUint32(0, body, true); bytes[4] = 1; bytes[5] = FT.TELL; bytes[17] = 8;
      bytes.fill(42, 32); return bytes;
    }
    for (const [body, fragment] of [[128*1024, 31], [1024*1024, 4093]]) {
      const input = wire(body), decoder = new FrameDecoder(body);
      allocated = 0; copied = 0;
      let received;
      for (let p = 0; p < input.length; p += fragment) {
        decoder.feed(input.subarray(p, p + fragment));
        const frame = decoder.nextFrame();
        if (p + fragment < input.length) assert.equal(frame, null);
        else received = frame;
      }
      assert.equal(received.payload.length, body - 28);
      assert.ok(received.payload.every(byte => byte === 42));
      const receipt = { body, fragment, allocated, copied };
      console.log(JSON.stringify(receipt));
      assert.ok(allocated <= input.length * 10, 'buffer allocation must be linear: ' + JSON.stringify(receipt));
      assert.ok(copied <= input.length * 10, 'buffer copying must be linear: ' + JSON.stringify(receipt));
    }
  `;
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', program],
    { encoding: 'utf8', timeout: 15000, maxBuffer: 1024 * 1024 });
  assert.equal(result.error, undefined);
  assert.equal(result.status, 0, `signal=${result.signal}; ${result.stdout}\n${result.stderr}`);
  assert.equal(result.stdout.trim().split('\n').length, 2);
  console.log(result.stdout.trim());
});
