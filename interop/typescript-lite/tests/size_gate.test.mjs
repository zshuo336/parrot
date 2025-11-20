import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';

const gate = fileURLToPath(new URL('../scripts/size-gate.mjs', import.meta.url));

function exactGzipBytes(size) {
  const bytes = Buffer.alloc(size);
  let state = 0x6d2b79f5;
  for (let i = 0; i < bytes.length; i++) {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    bytes[i] = state & 0xff;
  }
  // Find actual compressed lengths, independent of the gate's budget/units.
  for (let length = Math.max(0, size - 128); length <= size; length++) {
    const candidate = bytes.subarray(0, length);
    if (gzipSync(candidate).length === size) return candidate;
  }
  throw new Error(`could not construct a ${size}-byte gzip fixture`);
}

function runGate(t, bytes) {
  const root = mkdtempSync(join(tmpdir(), 'parrot-size-gate-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  if (bytes !== undefined) {
    mkdirSync(join(root, 'dist'));
    writeFileSync(join(root, 'dist/parrot-lite.bundle.js'), bytes);
  }
  const result = spawnSync(process.execPath, [gate], {
    cwd: root, encoding: 'utf8', timeout: 10_000,
  });
  assert.ifError(result.error);
  assert.equal(result.signal, null, result.stderr);
  return result;
}

test('size gate admits a real gzip stream of 49,999 bytes', t => {
  const bytes = exactGzipBytes(49_999);
  assert.equal(gzipSync(bytes).length, 49_999);
  const result = runGate(t, bytes);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /size gate PASS/);
});

for (const size of [50_000, 50_001, 51_200, 51_201]) {
  test(`size gate rejects a real gzip stream of ${size} bytes`, t => {
    const bytes = exactGzipBytes(size);
    assert.equal(gzipSync(bytes).length, size);
    const result = runGate(t, bytes);
    assert.equal(result.status, 1, result.stdout + result.stderr);
    assert.match(result.stderr, /SIZE GATE FAILED/);
    assert.doesNotMatch(result.stdout, /size gate PASS/);
  });
}

test('size gate measures gzip output rather than uncompressed bytes', t => {
  const bytes = Buffer.from('export const repeated = "' + 'a'.repeat(100_000) + '";\n');
  assert.ok(bytes.length > 50_000);
  assert.ok(gzipSync(bytes).length < 50_000);
  const result = runGate(t, bytes);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /size gate PASS/);
});

test('size gate fails on a missing artifact without a success marker', t => {
  const result = runGate(t);
  assert.equal(result.status, 2, result.stdout + result.stderr);
  assert.match(result.stderr, /missing/);
  assert.doesNotMatch(result.stdout, /size gate PASS/);
});
