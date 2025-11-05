/**
 * Parrot Wire 1.0 帧编解码（TS 栈——DEV_03 §2 / E1）。
 *
 * 布局（07 §2.1，与 Rust/JVM/Python/Erlang 逐字节一致）：
 *   外层: [u32 body_len LE]
 *   body: [ver u8][ft u8][flags u16 LE][cid u64 LE]
 *         [hop_count u8][hop_limit u8][sequence u32][reserved u16]
 *         [path_len u32 LE][path][key_len u32 LE][key][payload]
 *
 * 实现注意（§10.1）：cid 生成用双 u32 拼接——JS Number 无 u64 精度，
 * DataView 全 LE 手写（无 Buffer 依赖——浏览器兜底形态零 polyfill）。
 */

export const WIRE_VERSION = 1;
export const MAX_FRAME_LEN = 16 * 1024 * 1024;
export const DEFAULT_HANDSHAKE_FRAME_LEN = 1 << 20;
const BODY_FIXED_OVERHEAD = 28;

export const FT = {
  HANDSHAKE: 0x01,
  HANDSHAKE_ACK: 0x02,
  HEARTBEAT: 0x03,
  HEARTBEAT_ACK: 0x04,
  SYSTEM_EVENT: 0x20,
  ASK: 0x10,
  REPLY: 0x11,
  REPLY_ERR: 0x12,
  TELL: 0x13,
} as const;

export interface Frame {
  ft: number;
  flags: number;
  /** u64——以 hex 字符串承载（BigInt 可用场景经 parse 直转）。 */
  cid: bigint;
  hopCount: number;
  hopLimit: number;
  path: string;
  typeKey: string;
  payload: Uint8Array;
  sequence: number;
}

const enc = new TextEncoder();

// Paths and type keys are routing identities, so replacement characters would
// silently turn malformed wire bytes into a different, valid identity.
const identityDecoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
const knownFrameTypes = new Set([1, 2, 3, 4, 0x10, 0x11, 0x12, 0x13,
  0x14, 0x15, 0x20, 0x21, 0x22, 0x23, 0x24, 0x7f]);

export function validateIdentity(value: string, nonempty = false): void {
  if (typeof value !== "string" || (nonempty && !value) || value.includes("\0") ||
      identityDecoder.decode(enc.encode(value)) !== value) throw new Error("invalid identity");
}
function uint(value: number, max: number, name: string): void {
  if (!Number.isInteger(value) || value < 0 || value > max) throw new Error(`invalid ${name}`);
}
function bytes(value: Uint8Array): void {
  if (!(value instanceof Uint8Array)) throw new Error("payload must be Uint8Array");
}

export function buildFrame(
  ft: number,
  cid: bigint,
  path: string,
  typeKey: string,
  payload: Uint8Array,
  flags = 0,
  hopCount = 0,
  hopLimit = 8,
  sequence = 0,
  maxFrameLen = MAX_FRAME_LEN,
): Uint8Array {
  validateIdentity(path); validateIdentity(typeKey); bytes(payload);
  if (!knownFrameTypes.has(ft)) throw new Error("unknown frame type");
  if (typeof cid !== "bigint" || cid < 0n || cid > 0xffffffffffffffffn) throw new Error("invalid correlation id");
  uint(flags, 0xffff, "flags"); uint(hopCount, 255, "hop count");
  uint(hopLimit, 255, "hop limit"); uint(sequence, 0xffffffff, "sequence");
  if (!hopLimit || hopCount >= hopLimit) throw new Error("hop limit exceeded");
  uint(maxFrameLen, MAX_FRAME_LEN, "frame limit");
  if (maxFrameLen < BODY_FIXED_OVERHEAD) throw new Error("invalid frame limit");
  const pathB = enc.encode(path);
  const keyB = enc.encode(typeKey);
  const bodyLen = BODY_FIXED_OVERHEAD + pathB.length + keyB.length + payload.length;
  if (bodyLen > maxFrameLen) throw new Error(`frame too large: ${bodyLen}`);
  const out = new Uint8Array(4 + bodyLen);
  const dv = new DataView(out.buffer);
  dv.setUint32(0, bodyLen, true);
  dv.setUint8(4, WIRE_VERSION);
  dv.setUint8(5, ft);
  dv.setUint16(6, flags, true);
  dv.setBigUint64(8, cid, true);
  dv.setUint8(16, hopCount);
  dv.setUint8(17, hopLimit);
  dv.setUint32(18, sequence, true);
  // Reserved high 16 bits @22..24 remain zero.
  dv.setUint32(24, pathB.length, true);
  out.set(pathB, 28);
  const keyOff = 28 + pathB.length;
  dv.setUint32(keyOff, keyB.length, true);
  out.set(keyB, keyOff + 4);
  out.set(payload, keyOff + 4 + keyB.length);
  return out;
}

/** Owned streaming decoder: cursors consume frames, geometric growth accumulates
 * partial reads, and geometric compaction releases large consumed prefixes.
 * Both fragmented input and batches avoid repeatedly copying the whole buffer. */
export class FrameDecoder {
  private buf: Uint8Array = new Uint8Array(0);
  private pos = 0; // 已消费游标
  private end = 0; // 有效数据末尾；buf.length 是容量
  constructor(private maxFrameLen = MAX_FRAME_LEN) {
    this.setMaxFrameLen(maxFrameLen);
  }

  /** Change the established limit without discarding bytes buffered behind ACK. */
  setMaxFrameLen(maxFrameLen: number): void {
    uint(maxFrameLen, MAX_FRAME_LEN, "frame limit");
    if (maxFrameLen < BODY_FIXED_OVERHEAD) throw new Error("invalid frame limit");
    this.maxFrameLen = maxFrameLen;
  }

  /** Snapshot input bytes; callers may reuse their Uint8Array or Node Buffer. */
  feed(data: Uint8Array): void {
    if (data.length === 0) return;
    if (this.end + data.length > this.buf.length) {
      const unread = this.end - this.pos;
      const required = unread + data.length;
      if (required <= this.buf.length && this.pos >= this.buf.length / 2) {
        this.buf.copyWithin(0, this.pos, this.end);
      } else {
        // Geometric growth prevents every short read from copying the entire
        // incomplete frame. Copy input into owned storage, including Buffer.
        const growth = Math.min(this.maxFrameLen + 4, Math.max(4096, this.buf.length * 2));
        const grown = new Uint8Array(Math.max(required, growth));
        grown.set(this.buf.subarray(this.pos, this.end));
        this.buf = grown;
      }
      this.pos = 0;
      this.end = unread;
    }
    this.buf.set(data, this.end);
    this.end += data.length;
  }

  nextFrame(): Frame | null {
    const avail = this.end - this.pos;
    if (avail < 4) {
      this.compact();
      return null;
    }
    const dv = new DataView(this.buf.buffer, this.buf.byteOffset);
    const bodyLen = dv.getUint32(this.pos, true);
    if (bodyLen < BODY_FIXED_OVERHEAD) throw new Error(`invalid frame length ${bodyLen}`);
    if (bodyLen > this.maxFrameLen) throw new Error(`frame too large: ${bodyLen}`);
    if (avail < 4 + bodyLen) {
      this.compact();
      return null;
    }
    const body = this.buf.subarray(this.pos + 4, this.pos + 4 + bodyLen);
    const bdv = new DataView(body.buffer, body.byteOffset);
    const ver = bdv.getUint8(0);
    if (ver !== WIRE_VERSION) throw new Error(`unsupported wire version ${ver}`);
    const ft = bdv.getUint8(1);
    if (!knownFrameTypes.has(ft)) throw new Error(`unknown frame type ${ft}`);
    const flags = bdv.getUint16(2, true);
    const cid = bdv.getBigUint64(4, true);
    const hopCount = bdv.getUint8(12);
    const hopLimit = bdv.getUint8(13);
    if (hopLimit === 0 || hopCount >= hopLimit) throw new Error(`hop limit exceeded ${hopCount}/${hopLimit}`);
    if (bdv.getUint16(18, true) !== 0) throw new Error('reserved high bits must be zero');
    const pathLen = bdv.getUint32(20, true);
    if (pathLen > bodyLen - BODY_FIXED_OVERHEAD) throw new Error(`invalid path length ${pathLen}`);
    const path = identityDecoder.decode(body.subarray(24, 24 + pathLen));
    const keyOff = 24 + pathLen;
    if (keyOff + 4 > bodyLen) throw new Error("missing type key length");
    const keyLen = bdv.getUint32(keyOff, true);
    if (keyLen > bodyLen - BODY_FIXED_OVERHEAD - pathLen) throw new Error(`invalid type key length ${keyLen}`);
    const typeKey = identityDecoder.decode(body.subarray(keyOff + 4, keyOff + 4 + keyLen));
    const payload = body.slice(keyOff + 4 + keyLen);
    const sequence = bdv.getUint32(14, true);
    this.pos += 4 + bodyLen;
    this.compact();
    return { ft, flags, cid, hopCount, hopLimit, path, typeKey, payload, sequence };
  }

  /** Reuse small buffers; release large frames without retaining their capacity
   * for a tiny tail. Geometric shrink keeps bulk-frame copying linear. */
  private compact(): void {
    if (this.pos === 0) return;
    if (this.pos === this.end) {
      if (this.buf.length > 65536) this.buf = new Uint8Array(0);
      this.pos = this.end = 0;
    } else if (this.buf.length > 65536 && this.end - this.pos <= this.buf.length / 4) {
      const unread = this.end - this.pos;
      const smaller = new Uint8Array(Math.max(4096, unread * 2));
      smaller.set(this.buf.subarray(this.pos, this.end));
      this.buf = smaller;
      this.pos = 0;
      this.end = unread;
    }
  }
}

/** 随机种子后按 u64 递增，避免在途请求因重复随机数被覆盖。 */
const cidSeed = new Uint32Array(2);
if (globalThis.crypto?.getRandomValues) globalThis.crypto.getRandomValues(cidSeed);
else { cidSeed[0] = Math.floor(Math.random() * 0x100000000); cidSeed[1] = Math.floor(Math.random() * 0x100000000); }
let cidCounter = (BigInt(cidSeed[1]) << 32n) | BigInt(cidSeed[0]);
export function newCid(): bigint {
  cidCounter = BigInt.asUintN(64, cidCounter + 1n);
  return cidCounter;
}

// ---------------- 错误体（[u16 code][u16 rsv][detail utf8]） ----------------

export const ErrCode = {
  ActorNotFound: 1,
  Timeout: 2,
  Stopped: 3,
  NotRemotable: 4,
  CodecError: 5,
  UnknownTypeKey: 6,
  RouteUnreachable: 7,
  ConnectionLost: 8,
  DirectoryStale: 9,
  Overloaded: 10,
  NoCommonCodec: 11,
  ProtocolViolation: 12,
  Forbidden: 13,
} as const;

export function encodeErrPayload(code: number, detail: string): Uint8Array {
  uint(code, 13, "error code");
  if (code === 0) throw new Error("unknown error code");
  if (typeof detail !== "string") throw new Error("error detail must be a string");
  const d = enc.encode(detail);
  if (identityDecoder.decode(d) !== detail) throw new Error("invalid error detail Unicode");
  const out = new Uint8Array(4 + d.length);
  new DataView(out.buffer).setUint16(0, code, true);
  out.set(d, 4);
  return out;
}

export function decodeErrPayload(b: Uint8Array): { code: number; detail: string } {
  if (b.length < 4 || b[2] || b[3]) throw new Error("invalid error envelope");
  const code = new DataView(b.buffer, b.byteOffset).getUint16(0, true);
  if (code < 1 || code > 13) throw new Error("unknown error code");
  return { code, detail: identityDecoder.decode(b.subarray(4)) };
}

// ---------------- ASK reply_to 前缀 ----------------

export function withReplyToPrefix(replyTo: string, payload: Uint8Array): Uint8Array {
  validateIdentity(replyTo); bytes(payload);
  const rb = enc.encode(replyTo);
  if (rb.length + 4 + payload.length > MAX_FRAME_LEN) throw new Error("ASK prefix too large");
  const out = new Uint8Array(4 + rb.length + payload.length);
  new DataView(out.buffer).setUint32(0, rb.length, true);
  out.set(rb, 4);
  out.set(payload, 4 + rb.length);
  return out;
}

/** Decode an ASK prefix and return an independent payload, including for Buffer input. */
export function splitReplyTo(b: Uint8Array): { replyTo: string; payload: Uint8Array } | null {
  if (b.length < 4) return null;
  const rlen = new DataView(b.buffer, b.byteOffset).getUint32(0, true);
  if (b.length < 4 + rlen) return null;
  const replyTo = identityDecoder.decode(b.subarray(4, 4 + rlen));
  validateIdentity(replyTo);
  return {
    replyTo,
    // Uint8Array includes Node Buffer, whose slice() aliases the input.
    payload: new Uint8Array(b.subarray(4 + rlen)),
  };
}

// ---------------- 握手 TLV（tag u8 + len u16 LE + value） ----------------

export const TlvTag = {
  NODE_ID: 1,
  REALM: 2,
  CLUSTER: 3,
  CAPABILITIES: 4,
  MAX_FRAME_LEN: 5,
  TOPOLOGY_ROLE: 6,
  HOP_LIMIT: 7,
  DIRECT_ADDR: 8,
  CHOSEN_CODEC: 9,
} as const;

export const CAPS_PB_ONLY = 0x02;
/** Negotiation only: callers supply encoded bytes and their matching type keys. */
export type WireCodec = "pb" | "bin";
function codecCapability(codec: WireCodec): number {
  if (codec === "pb") return CAPS_PB_ONLY;
  if (codec === "bin") return 0x01;
  throw new Error("unsupported payload codec");
}

function tlv(tag: number, value: Uint8Array): Uint8Array {
  if (value.length > 65535) throw new Error("TLV exceeds u16 length");
  const out = new Uint8Array(3 + value.length);
  out[0] = tag;
  new DataView(out.buffer).setUint16(1, value.length, true);
  out.set(value, 3);
  return out;
}

function u32le(v: number): Uint8Array {
  const out = new Uint8Array(4);
  new DataView(out.buffer).setUint32(0, v, true);
  return out;
}

export function handshakeBody(nodeId: string, codec: WireCodec = "pb"): Uint8Array {
  validateIdentity(nodeId, true);
  const parts = [
    tlv(TlvTag.NODE_ID, enc.encode(nodeId)),
    tlv(TlvTag.CAPABILITIES, u32le(codecCapability(codec))),
    tlv(TlvTag.MAX_FRAME_LEN, u32le(DEFAULT_HANDSHAKE_FRAME_LEN)),
    tlv(TlvTag.TOPOLOGY_ROLE, new Uint8Array([0])),
    tlv(TlvTag.HOP_LIMIT, new Uint8Array([8])),
  ];
  const total = parts.reduce((n, p) => n + p.length, 0);
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

export function handshakeAckBody(nodeId: string, codec: WireCodec = "pb"): Uint8Array {
  const base = handshakeBody(nodeId, codec);
  const cc = tlv(TlvTag.CHOSEN_CODEC, enc.encode(codec));
  const out = new Uint8Array(base.length + cc.length);
  out.set(base);
  out.set(cc, base.length);
  return out;
}

/** Return every TLV in order with independently owned value bytes. */
export function parseTlv(body: Uint8Array): Array<[number, Uint8Array]> {
  const out: Array<[number, Uint8Array]> = [];
  let p = 0;
  const dv = new DataView(body.buffer, body.byteOffset);
  while (p < body.length) {
    if (body.length - p < 3) throw new Error("truncated TLV header");
    const tag = body[p]!;
    const len = dv.getUint16(p + 1, true);
    if (len > body.length - p - 3) throw new Error("truncated TLV value");
    // Preserve owned values for Buffer inputs as well as plain Uint8Array.
    out.push([tag, new Uint8Array(body.subarray(p + 3, p + 3 + len))]);
    p += 3 + len;
  }
  return out;
}

/** Strict ACK negotiation for the explicitly selected payload stack. */
export function decodeHandshakeAck(body: Uint8Array, codec: WireCodec = "pb"): { maxFrameLen: number; nodeId: string } {
  const capability = codecCapability(codec);
  const fields = new Map<number, Uint8Array>();
  // Retain at most nine borrowed fields. Unknown/duplicate extensions must
  // not allocate one object per TLV; validate all structure before duplicates.
  const dv = new DataView(body.buffer, body.byteOffset);
  let duplicate = 0;
  for (let p = 0; p < body.length;) {
    if (body.length - p < 3) throw new Error("truncated TLV header");
    const tag = body[p]!;
    const len = dv.getUint16(p + 1, true);
    if (len > body.length - p - 3) throw new Error("truncated TLV value");
    if (tag >= 1 && tag <= 9) {
      if (fields.has(tag)) { if (!duplicate) duplicate = tag; }
      else fields.set(tag, body.subarray(p + 3, p + 3 + len));
    }
    p += 3 + len;
  }
  if (duplicate) throw new Error(`duplicate handshake tag ${duplicate}`);
  function field(tag: number, size?: number): Uint8Array {
    const value = fields.get(tag);
    if (!value || (size !== undefined && value.length !== size)) throw new Error(`missing/invalid handshake tag ${tag}`);
    return value;
  }
  const nodeId = identityDecoder.decode(field(1)); validateIdentity(nodeId, true);
  for (const tag of [2,3,8]) if (fields.has(tag)) validateIdentity(identityDecoder.decode(field(tag)));
  const caps = new DataView(field(4,4).buffer, field(4,4).byteOffset, 4).getUint32(0,true);
  const limit = new DataView(field(5,4).buffer, field(5,4).byteOffset, 4).getUint32(0,true);
  if (field(6,1)[0] > 3 || (fields.has(7) && field(7,1)[0] === 0) || limit < 28) throw new Error("invalid handshake bounds");
  // Historical missing choice means bin; it is valid only for a bin caller.
  const chosen = fields.has(9) ? identityDecoder.decode(field(9)) : "bin";
  if (!(caps & capability) || chosen !== codec) throw new Error("no negotiated payload codec");
  return { nodeId, maxFrameLen: Math.min(limit, MAX_FRAME_LEN) };
}
