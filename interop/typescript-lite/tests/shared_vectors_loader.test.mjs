import test from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { loadSharedVectors, parseSharedVectors, parseSharedVectorBytes } from './support/shared-vectors.mjs';

const row = { name: 'parser-unit', frame_type: '0x03', correlation_id: 1,
  path: '', type_key: '', payload_hex: '',
  bytes_hex: '1c00000001030000010000000000000000080000000000000000000000000000' };
const document = () => ({ format: 'parrot-wire-1.0', frozen: true, vectors: [{ ...row }] });
const parse = value => parseSharedVectors(JSON.stringify(value));
const changed = (key, value) => {
  const valueDocument = document();
  valueDocument.vectors[0][key] = value;
  return valueDocument;
};

test('shared loader standard whitespace ordering and escaping', () => {
  const value = document();
  value.vectors[0].path = '\ufeff\u96ea"\\\n{payload}';
  const input = JSON.stringify(value, null, 3).replaceAll('":', '" \t:');
  assert.equal(parseSharedVectors(input).vectors[0].path, value.vectors[0].path);
});

test('shared loader preserves unsigned header maxima', () => {
  const value = document();
  Object.assign(value.vectors[0], { flags: 65535, hop_count: 254, hop_limit: 255, sequence: 4294967295 });
  const result = parseSharedVectors(JSON.stringify(value).replace('"correlation_id":1', '"correlation_id":18446744073709551615')).vectors[0];
  assert.equal(result.correlation_id, 18446744073709551615n);
  assert.deepEqual([result.flags, result.hop_count, result.hop_limit, result.sequence], [65535, 254, 255, 4294967295]);
});

test('shared loader rejects noncanonical and underflowed integer tokens', () => {
  const accepted = [];
  for (const token of ['1.0', '1e0', '1e-400', '1e-999999999999999999999999',
    '1.0000000000000001', '9007199254740993.0']) {
    try {
      parseSharedVectors(JSON.stringify(document()).replace('"correlation_id":1', '"correlation_id":' + token));
      accepted.push(token);
    } catch { /* A schema or syntax rejection is required for each token. */ }
  }
  assert.deepEqual(accepted, []);
});

test('shared loader checks format and frozen type', () => {
  for (const value of [{ ...document(), format: 'bad' }, { ...document(), frozen: 'yes' },
    { ...document(), frozen: false }]) assert.throws(() => parse(value));
});

test('shared loader checks document and row shape', () => {
  for (const value of [null, [], 1, { ...document(), vectors: [] }, { ...document(), vectors: {} },
    { ...document(), vectors: [null] }, { ...document(), vectors: [[]] }]) assert.throws(() => parse(value));
});

test('shared loader rejects duplicate keys including equal values', () => {
  const input = JSON.stringify(document());
  for (const duplicate of ['{"format":"parrot-wire-1.0",' + input.slice(1),
    input.replace('"correlation_id":1', '"correlation_id":1,"correlation_id":2'),
    input.replace('"correlation_id":1', '"correlation_id":1,"correlation_id":1')]) {
    assert.throws(() => parseSharedVectors(duplicate), error => /Duplicate key/.test(error.message));
  }
});

test('shared loader rejects empty and duplicate names', () => {
  assert.throws(() => parse(changed('name', '')));
  const value = document();
  value.vectors.push({ ...row });
  assert.throws(() => parse(value), /duplicate fixture vector name/);
});

test('shared loader rejects malformed and nonstandard JSON', () => {
  const input = JSON.stringify(document());
  for (const invalid of [input + ' {}', input + ' garbage', input.slice(0, -1),
    input.replace('"correlation_id":1', '"correlation_id":01'),
    input.replace('"correlation_id":1', '"correlation_id":NaN'),
    input.replace('"correlation_id":1', '"correlation_id":1.'),
    input.replace('parser-unit', 'parser\nunit')]) assert.throws(() => parseSharedVectors(invalid));
});

test('shared loader requires typed strings and all mandatory fields', () => {
  for (const key of Object.keys(row)) {
    const value = document();
    delete value.vectors[0][key];
    assert.throws(() => parse(value), key);
  }
  for (const key of ['name', 'frame_type', 'path', 'type_key', 'payload_hex', 'bytes_hex']) {
    assert.throws(() => parse(changed(key, 1)), key);
  }
});

test('shared loader rejects numeric coercion negatives and overflow', () => {
  for (const key of ['correlation_id', 'flags', 'hop_count', 'hop_limit', 'sequence']) {
    for (const value of ['1', true, null, {}, { isLosslessNumber: true, value: '1' }, -1, 1.5]) assert.throws(() => parse(changed(key, value)), key);
  }
  for (const [key, value] of [['flags', 65536], ['hop_count', 256], ['hop_limit', 256], ['sequence', 4294967296]]) {
    assert.throws(() => parse(changed(key, value)), key);
  }
  assert.throws(() => parseSharedVectors(JSON.stringify(document()).replace('"correlation_id":1', '"correlation_id":18446744073709551616')));
});

test('shared loader validates hex and exact frame type', () => {
  for (const key of ['bytes_hex', 'payload_hex']) {
    for (const value of ['0', 'gg', '00 ff', '0x00']) assert.throws(() => parse(changed(key, value)));
    assert.equal(parse(changed(key, 'AaFF')).vectors[0][key], 'aaff');
  }
  for (const value of ['3', '0X03', '0x3', '0x003', '0xgg']) assert.throws(() => parse(changed('frame_type', value)));
});

test('shared loader rejects NUL surrogate and invalid UTF8 identities', () => {
  for (const key of ['name', 'path', 'type_key']) {
    for (const value of ['a\0b', '\ud800', '\udfff']) assert.throws(() => parse(changed(key, value)));
  }
  assert.throws(() => parseSharedVectorBytes(Buffer.from([0xff])));
  assert.throws(() => parseSharedVectorBytes(Buffer.from([0xed, 0xa0, 0x80])));
});

test('shared loader executes appended rows and binary data', () => {
  const value = document();
  value.vectors.push({ ...row, name: 'second', payload_hex: '00ff80' });
  const result = parse(value);
  assert.equal(result.vectors.length, 2);
  assert.equal(result.vectors[1].payload_hex, '00ff80');
});

test('shared loader explicit invalid repository cannot fall back', () => {
  const defaultPath = fileURLToPath(new URL('../../../docs/vectors/wire1.json', import.meta.url));
  assert.throws(() => loadSharedVectors({ env: { PARROT_REPO: '' }, defaultPath }), /empty configured/);
  assert.throws(() => loadSharedVectors({ env: { PARROT_REPO: new URL('./missing-repo/', import.meta.url).pathname }, defaultPath }), /ENOENT/);
});

test('shared loader default repository is module relative', () => {
  const loaded = loadSharedVectors({ env: {} });
  assert.ok(loaded.vectors.length >= 4);
  assert.ok(loaded.vectors.some(vec => vec.name === 'reply-err-stopped'));
});
