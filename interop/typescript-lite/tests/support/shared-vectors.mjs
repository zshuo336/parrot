import { readFileSync, statSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import JSONBigInt from 'json-bigint';
import { LosslessNumber, parse as parseLossless } from 'lossless-json';

const parser = JSONBigInt({ strict: true, alwaysParseAsBig: true,
  protoAction: 'error', constructorAction: 'error' });
const budget = 64 * 1024 * 1024;
const defaultPath = fileURLToPath(new URL('../../../../docs/vectors/wire1.json', import.meta.url));

function requireValue(condition, message) {
  if (!condition) throw new Error(message);
}

function text(row, key, nonempty = false) {
  const value = row[key];
  requireValue(typeof value === 'string' && (!nonempty || value.length > 0), `invalid fixture string: ${key}`);
  requireValue(!value.includes('\0') && Buffer.from(value, 'utf8').toString('utf8') === value,
    `invalid fixture identity: ${key}`);
  return value;
}

function uint(row, key, bits, fallback) {
  if (!Object.hasOwn(row, key) && fallback !== undefined) return fallback;
  const value = row[key];
  requireValue(value instanceof LosslessNumber && value.value.length <= 21 && /^-?(?:0|[1-9][0-9]*)$/.test(value.value),
    `invalid fixture integer: ${key}`);
  const integer = BigInt(value.value);
  requireValue(integer >= 0n && integer < (1n << BigInt(bits)), `invalid fixture integer: ${key}`);
  return bits === 64 ? integer : Number(integer);
}

function hex(row, key) {
  const value = text(row, key);
  requireValue(value.length % 2 === 0 && /^[0-9a-fA-F]*$/.test(value) &&
    value.length <= 2 * (16 * 1024 * 1024 + 4), `invalid fixture hex: ${key}`);
  return value.toLowerCase();
}

export function parseSharedVectors(input) {
  requireValue(typeof input === 'string' && Buffer.byteLength(input, 'utf8') <= budget, 'fixture exceeds input budget');
  // Syntax and duplicate checks never supply numeric expectations.
  JSON.parse(input);
  parser.parse(input);
  const document = parseLossless(input);
  requireValue(document && typeof document === 'object' && !Array.isArray(document) &&
    document.format === 'parrot-wire-1.0', 'wrong fixture format');
  requireValue(document.frozen === true, 'fixture must be frozen');
  requireValue(Array.isArray(document.vectors) && document.vectors.length > 0 &&
    document.vectors.length <= 4096, 'invalid fixture vector list');
  const names = new Set();
  const vectors = document.vectors.map(row => {
    requireValue(row && typeof row === 'object' && !Array.isArray(row), 'fixture row must be an object');
    const name = text(row, 'name', true);
    requireValue(!names.has(name), 'duplicate fixture vector name');
    names.add(name);
    const frameType = text(row, 'frame_type');
    requireValue(/^0x[0-9a-fA-F]{2}$/.test(frameType), 'invalid fixture frame type');
    return { name, frame_type: frameType, correlation_id: uint(row, 'correlation_id', 64),
      flags: uint(row, 'flags', 16, 0), hop_count: uint(row, 'hop_count', 8, 0),
      hop_limit: uint(row, 'hop_limit', 8, 8), sequence: uint(row, 'sequence', 32, 0),
      path: text(row, 'path'), type_key: text(row, 'type_key'),
      payload_hex: hex(row, 'payload_hex'), bytes_hex: hex(row, 'bytes_hex') };
  });
  return { format: document.format, frozen: true, vectors };
}

export function parseSharedVectorBytes(bytes) {
  requireValue(bytes.byteLength <= budget, 'fixture exceeds input budget');
  return parseSharedVectors(new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes));
}

export function loadSharedVectors(options = {}) {
  const environment = options.env ?? process.env;
  let path = options.defaultPath ?? defaultPath;
  if (Object.hasOwn(environment, 'PARROT_REPO')) {
    const root = environment.PARROT_REPO;
    requireValue(typeof root === 'string' && root.length > 0, 'empty configured fixture repository');
    path = resolve(root, 'docs/vectors/wire1.json');
  }
  const metadata = statSync(path);
  requireValue(metadata.isFile(), `fixture is not a file: ${path}`);
  requireValue(metadata.size <= budget, 'fixture exceeds input budget');
  return parseSharedVectorBytes(readFileSync(path));
}
