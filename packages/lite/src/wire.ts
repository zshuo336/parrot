/**
 * Parrot Wire 1.0 帧编解码（TS 栈——DEV_03 §2 / E1）。
 *
 * 布局（07 §2.1，与 Rust/JVM/Python/Erlang 逐字节一致）：
 *   外层: [u32 body_len LE]
 *   body: [ver u8][ft u8][flags u16 LE][cid u64 LE]
 *         [hop_count u8][hop_limit u8][rsv u48]
 *         [path_len u32 LE][path][key_len u32 LE][key][payload]
 *
 * 实现注意（§10.1）：cid 生成用双 u32 拼接——JS Number 无 u64 精度，
 * DataView 全 LE 手写（无 Buffer 依赖——浏览器兜底形态零 polyfill）。
 */

export const WIRE_VERSION = 1;

export const FT = {
  HANDSHAKE: 0x01,
  HANDSHAKE_ACK: 0x02,
  HEARTBEAT: 0x03,
  HEARTBEAT_ACK: 0x04,
  SYSTEM_EVENT: 0x05,
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
}

const enc = new TextEncoder();
const dec = new TextDecoder();

export function buildFrame(
  ft: number,
  cid: bigint,
  path: string,
  typeKey: string,
  payload: Uint8Array,
  flags = 0,
  hopCount = 0,
  hopLimit = 8,
): Uint8Array {
  const pathB = enc.encode(path);
  const keyB = enc.encode(typeKey);
  const bodyLen = 28 + pathB.length + keyB.length + payload.length;
  const out = new Uint8Array(4 + bodyLen);
  const dv = new DataView(out.buffer);
  dv.setUint32(0, bodyLen, true);
  dv.setUint8(4, WIRE_VERSION);
  dv.setUint8(5, ft);
  dv.setUint16(6, flags, true);
  dv.setBigUint64(8, cid, true);
  dv.setUint8(16, hopCount);
  dv.setUint8(17, hopLimit);
  // rsv u48 @18..24 = 0
  dv.setUint32(24, pathB.length, true);
  out.set(pathB, 28);
  const keyOff = 28 + pathB.length;
  dv.setUint32(keyOff, keyB.length, true);
  out.set(keyB, keyOff + 4);
  out.set(payload, keyOff + 4 + keyB.length);
  return out;
}

/** 非消费式半包解码器。 */
export class FrameDecoder {
  private buf: Uint8Array = new Uint8Array(0);

  feed(data: Uint8Array): void {
    const merged = new Uint8Array(this.buf.length + data.length);
    merged.set(this.buf);
    merged.set(data, this.buf.length);
    this.buf = merged;
  }

  nextFrame(): Frame | null {
    if (this.buf.length < 4) return null;
    const dv = new DataView(this.buf.buffer, this.buf.byteOffset);
    const bodyLen = dv.getUint32(0, true);
    if (this.buf.length < 4 + bodyLen) return null;
    const body = this.buf.subarray(4, 4 + bodyLen);
    const bdv = new DataView(body.buffer, body.byteOffset);
    const ver = bdv.getUint8(0);
    if (ver !== WIRE_VERSION) throw new Error(`unsupported wire version ${ver}`);
    const ft = bdv.getUint8(1);
    const flags = bdv.getUint16(2, true);
    const cid = bdv.getBigUint64(4, true);
    const hopCount = bdv.getUint8(12);
    const hopLimit = bdv.getUint8(13);
    const pathLen = bdv.getUint32(20, true);
    const path = dec.decode(body.subarray(24, 24 + pathLen));
    const keyOff = 24 + pathLen;
    const keyLen = bdv.getUint32(keyOff, true);
    const typeKey = dec.decode(body.subarray(keyOff + 4, keyOff + 4 + keyLen));
    const payload = body.slice(keyOff + 4 + keyLen);
    this.buf = this.buf.slice(4 + bodyLen);
    return { ft, flags, cid, hopCount, hopLimit, path, typeKey, payload };
  }
}

/** cid 生成：双 u32 拼接（§10.1——不用 Number 算术防精度丢失）。 */
export function newCid(): bigint {
  const a = BigInt(Math.floor(Math.random() * 0xffffffff) >>> 0);
  const b = BigInt(Math.floor(Math.random() * 0xffffffff) >>> 0);
  return (b << 32n) | a;
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
  const d = enc.encode(detail);
  const out = new Uint8Array(4 + d.length);
  new DataView(out.buffer).setUint16(0, code, true);
  out.set(d, 4);
  return out;
}

export function decodeErrPayload(b: Uint8Array): { code: number; detail: string } {
  if (b.length < 4) return { code: ErrCode.ProtocolViolation, detail: "<undecodable>" };
  const code = new DataView(b.buffer, b.byteOffset).getUint16(0, true);
  return { code, detail: dec.decode(b.subarray(4)) };
}

// ---------------- ASK reply_to 前缀 ----------------

export function withReplyToPrefix(replyTo: string, payload: Uint8Array): Uint8Array {
  const rb = enc.encode(replyTo);
  const out = new Uint8Array(4 + rb.length + payload.length);
  new DataView(out.buffer).setUint32(0, rb.length, true);
  out.set(rb, 4);
  out.set(payload, 4 + rb.length);
  return out;
}

export function splitReplyTo(b: Uint8Array): { replyTo: string; payload: Uint8Array } | null {
  if (b.length < 4) return null;
  const rlen = new DataView(b.buffer, b.byteOffset).getUint32(0, true);
  if (b.length < 4 + rlen) return null;
  return {
    replyTo: dec.decode(b.subarray(4, 4 + rlen)),
    payload: b.slice(4 + rlen),
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
  CHOSEN_CODEC: 8,
} as const;

export const CAPS_PB_ONLY = 0x02;

function tlv(tag: number, value: Uint8Array): Uint8Array {
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

export function handshakeBody(nodeId: string): Uint8Array {
  const parts = [
    tlv(TlvTag.NODE_ID, enc.encode(nodeId)),
    tlv(TlvTag.CAPABILITIES, u32le(CAPS_PB_ONLY)),
    tlv(TlvTag.MAX_FRAME_LEN, u32le(1 << 20)),
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

export function handshakeAckBody(nodeId: string): Uint8Array {
  const base = handshakeBody(nodeId);
  const cc = tlv(TlvTag.CHOSEN_CODEC, enc.encode("pb"));
  const out = new Uint8Array(base.length + cc.length);
  out.set(base);
  out.set(cc, base.length);
  return out;
}

export function parseTlv(body: Uint8Array): Array<[number, Uint8Array]> {
  const out: Array<[number, Uint8Array]> = [];
  let p = 0;
  const dv = new DataView(body.buffer, body.byteOffset);
  while (p + 3 <= body.length) {
    const tag = body[p]!;
    const len = dv.getUint16(p + 1, true);
    if (p + 3 + len > body.length) break;
    out.push([tag, body.slice(p + 3, p + 3 + len)]);
    p += 3 + len;
  }
  return out;
}
