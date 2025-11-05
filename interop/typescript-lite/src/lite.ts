/** Single-connection Wire client. Each dial owns its decoder, timer and replies. */
import {
  WireCodec, Frame, FrameDecoder, FT, MAX_FRAME_LEN, DEFAULT_HANDSHAKE_FRAME_LEN, buildFrame, handshakeBody,
  decodeHandshakeAck, newCid, splitReplyTo, withReplyToPrefix,
  decodeErrPayload, encodeErrPayload, ErrCode, validateIdentity,
} from "./wire.js";

export interface ParrotLiteOptions {
  url: string;
  nodeId: string;
  /** Defaults to pb. bin requires the caller to supply correctly encoded binary payloads. */
  codec?: WireCodec;
  /** Custom client certificates are not implemented; use platform wss TLS. */
  tls?: { cert?: string; key?: string; ca?: string };
  wsFactory?: (url: string) => MinimalWS;
  connectTimeoutMs?: number;
  reconnectDelayMs?: number;
  maxPending?: number;
  maxPendingBytes?: number;
  maxConcurrentHandlers?: number;
  maxActors?: number;
  maxRegistrations?: number;
  maxBufferedBytes?: number;
}
export interface MinimalWS {
  readyState: number;
  /** Required for enforceable transport buffering limits on custom adapters. */
  readonly bufferedAmount: number;
  send(data: Uint8Array): void;
  close(): void;
  onopen: (() => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
  onmessage: ((ev: { data: ArrayBuffer }) => void) | null;
}
export interface HandlerContext { signal: AbortSignal }
export interface SpawnOpts {
  onAsk?: (typeKey: string, payload: Uint8Array, context: HandlerContext) => Promise<Uint8Array>;
  onTell?: (typeKey: string, payload: Uint8Array, context: HandlerContext) => void | Promise<void>;
}
interface Connection {
  ws?: MinimalWS;
  decoder: FrameDecoder;
  phase: "connecting" | "handshake" | "ready" | "closed";
  peerLimit: number;
  deadline: number;
  timer?: ReturnType<typeof setTimeout>;
  resolve: () => void;
  reject: (error: Error) => void;
  abort: AbortController;
}
interface Pending {
  connection: Connection;
  resolve: (bytes: Uint8Array) => void;
  reject: (error: Error) => void;
}
function asError(error: unknown): Error {
  try { return error instanceof Error ? error : new Error(String(error)); }
  catch { return new Error("unprintable failure"); }
}
const now = () => performance.now();
function positive(value: number | undefined, fallback: number, name: string): number {
  const n = value ?? fallback;
  if (!Number.isInteger(n) || n < 1 || n > 0x7fffffff) throw new Error(`invalid ${name}`);
  return n;
}

export class ParrotLite {
  private connection?: Connection;
  private pending = new Map<bigint, Pending>();
  private actors = new Map<string, SpawnOpts>();
  private registrations: Array<{ key: string; path: string }> = [];
  private retry?: ReturnType<typeof setTimeout>;
  private closed = false;
  private everReady = false;
  private activeHandlers = 0;
  private pendingBytes = 0;
  private backoffMs: number;
  private readonly connectTimeoutMs: number;
  private readonly reconnectDelayMs: number;
  private readonly maxPending: number;
  private readonly maxHandlers: number;
  private readonly maxPendingBytes: number;
  private readonly maxActors: number;
  private readonly maxRegistrations: number;
  private readonly maxBufferedBytes: number;
  private readonly hello: Uint8Array;
  private readonly codec: WireCodec;
  private constructor(private readonly opts: ParrotLiteOptions) {
    if (opts.tls !== undefined) throw new Error("custom TLS credentials are not supported");
    if (!/^(tcp|parrot|ws|wss):\/\//.test(opts.url)) throw new Error("unsupported transport URL");
    const url = new URL(opts.url);
    if (!url.hostname || url.username || url.password || url.hash) throw new Error("invalid transport URL");
    if (/^(tcp|parrot):/.test(opts.url) && (!url.port || url.search || (url.pathname && url.pathname !== "/")))
      throw new Error("invalid TCP address");
    this.codec = opts.codec === undefined ? "pb" : opts.codec;
    this.hello = handshakeBody(opts.nodeId, this.codec);
    this.connectTimeoutMs = positive(opts.connectTimeoutMs, 5000, "connect timeout");
    this.reconnectDelayMs = positive(opts.reconnectDelayMs, 1000, "reconnect delay");
    if (this.reconnectDelayMs > 30000) throw new Error("reconnect delay exceeds 30 seconds");
    this.backoffMs = this.reconnectDelayMs;
    this.maxPending = positive(opts.maxPending, 1024, "pending limit");
    this.maxPendingBytes = positive(opts.maxPendingBytes, MAX_FRAME_LEN, "pending byte limit");
    this.maxHandlers = positive(opts.maxConcurrentHandlers, 64, "handler limit");
    this.maxActors = positive(opts.maxActors, 1024, "actor limit");
    this.maxRegistrations = positive(opts.maxRegistrations, 1024, "registration limit");
    this.maxBufferedBytes = positive(opts.maxBufferedBytes, MAX_FRAME_LEN + 4, "buffer limit");
  }
  static async connect(options: ParrotLiteOptions): Promise<ParrotLite> {
    const node = new ParrotLite({ ...options });
    try { await node.dial(); return node; }
    catch (error) { node.close(); throw error; }
  }
  private live(c: Connection): boolean { return !this.closed && this.connection === c && c.phase !== "closed"; }
  private dial(): Promise<void> {
    if (this.closed) return Promise.reject(new Error("closed"));
    const c: Connection = {
      decoder: new FrameDecoder(MAX_FRAME_LEN), phase: "connecting", peerLimit: MAX_FRAME_LEN,
      deadline: now() + this.connectTimeoutMs, resolve: () => {}, reject: () => {}, abort: new AbortController(),
    };
    const result = new Promise<void>((resolve, reject) => { c.resolve = resolve; c.reject = reject; });
    this.connection = c;
    c.timer = setTimeout(() => this.retire(c, new Error("connection/handshake timeout")), this.connectTimeoutMs);
    void this.open(c).catch(error => this.retire(c, asError(error)));
    return result;
  }
  private async transport(c: Connection): Promise<MinimalWS> {
    if (this.opts.wsFactory) return this.opts.wsFactory(this.opts.url);
    if (/^(tcp|parrot):/.test(this.opts.url)) {
      // Variable dynamic import keeps the browser bundle dependency-free and
      // works in Node 18 ESM, without eval or a Node-22-only builtin API.
      const moduleName = "node:net";
      type Socket = {
        writableLength: number;
        write(bytes: Uint8Array): boolean;
        destroy(): void;
        on(event: string, callback: (bytes?: Uint8Array) => void): void;
      };
      const net = await import(moduleName) as { connect(options: { host: string; port: number }): Socket };
      if (!this.live(c)) throw new Error("connection cancelled");
      const url = new URL(this.opts.url);
      const sock = net.connect({ port: Number(url.port), host: url.hostname.replace(/^\[|\]$/g, "") });
      const ws: MinimalWS = {
        readyState: 0, get bufferedAmount() { return sock.writableLength; },
        send: bytes => { sock.write(bytes); }, close: () => { ws.readyState = 3; sock.destroy(); },
        onopen: null, onclose: null, onerror: null, onmessage: null,
      };
      sock.on("connect", () => { ws.readyState = 1; ws.onopen?.(); });
      sock.on("data", bytes => { if (bytes) ws.onmessage?.({ data: Uint8Array.from(bytes).buffer }); });
      sock.on("error", () => { ws.onerror?.(); });
      sock.on("close", () => { ws.readyState = 3; ws.onclose?.(); });
      return ws;
    }
    if (typeof WebSocket === "undefined") throw new Error("WebSocket unavailable; provide wsFactory");
    const ws = new WebSocket(this.opts.url); ws.binaryType = "arraybuffer";
    return ws as unknown as MinimalWS;
  }
  private async open(c: Connection): Promise<void> {
    const ws = await this.transport(c);
    if (!this.live(c)) { ws.close(); return; }
    c.ws = ws;
    (ws as { binaryType?: string }).binaryType = "arraybuffer";
    ws.onopen = () => {
      if (!this.live(c) || c.phase !== "connecting") return;
      try {
        if (now() >= c.deadline) throw new Error("connection/handshake timeout");
        c.phase = "handshake";
        this.send(c, buildFrame(FT.HANDSHAKE, newCid(), "", "", this.hello), true);
      } catch (error) { this.retire(c, asError(error)); }
    };
    ws.onmessage = event => {
      if (!this.live(c)) return;
      try {
        if (!(event.data instanceof ArrayBuffer)) throw new Error("binary ArrayBuffer required");
        const bytes = new Uint8Array(event.data);
        for (let offset = 0; offset < bytes.length && this.live(c); offset += 65536) {
          c.decoder.feed(bytes.subarray(offset, offset + 65536));
          for (let frame = c.decoder.nextFrame(); frame; frame = c.decoder.nextFrame()) {
            this.onFrame(c, frame);
            if (!this.live(c)) break;
          }
        }
      } catch (error) { this.retire(c, asError(error)); }
    };
    ws.onclose = () => this.retire(c, new Error("connection lost"));
    ws.onerror = () => this.retire(c, new Error("transport error"));
    if (ws.readyState === 1) ws.onopen();
    else if (ws.readyState !== 0) this.retire(c, new Error("transport closed before open"));
  }
  private send(c: Connection, data: Uint8Array, handshake = false): void {
    if (!this.live(c) || (!handshake && c.phase !== "ready") || c.ws?.readyState !== 1) throw new Error("not connected");
    if (data.length - 4 > c.peerLimit) throw new Error("frame exceeds peer limit");
    const buffered = c.ws.bufferedAmount;
    if (!Number.isFinite(buffered) || buffered < 0 || data.length > this.maxBufferedBytes - buffered)
      throw new Error("transport buffer overloaded");
    try { c.ws.send(data); }
    catch (error) { this.retire(c, asError(error)); throw error; }
  }
  private onFrame(c: Connection, f: Frame): void {
    validateIdentity(f.path); validateIdentity(f.typeKey);
    if (c.phase !== "ready") {
      if (c.phase !== "handshake" || f.ft !== FT.HANDSHAKE_ACK || f.flags || f.path || now() >= c.deadline)
        throw new Error("expected handshake ACK");
      c.peerLimit = Math.min(DEFAULT_HANDSHAKE_FRAME_LEN, decodeHandshakeAck(f.payload, this.codec).maxFrameLen);
      c.decoder.setMaxFrameLen(c.peerLimit);
      c.phase = "ready";
      // Restore registration before publishing readiness, exactly once for this
      // generation. A send failure rejects this dial rather than claiming ready.
      for (const r of this.registrations) this.send(c, this.registrationFrame(r.key, r.path, c.peerLimit));
      if (!this.live(c)) return;
      clearTimeout(c.timer); c.timer = undefined;
      this.everReady = true; this.backoffMs = this.reconnectDelayMs; c.resolve();
      return;
    }
    if (f.ft === FT.HANDSHAKE || f.ft === FT.HANDSHAKE_ACK) throw new Error("duplicate/unexpected handshake");
    if (f.ft === FT.HEARTBEAT || f.ft === FT.HEARTBEAT_ACK) {
      if (f.flags || f.payload.length) throw new Error("invalid heartbeat");
      if (f.ft === FT.HEARTBEAT) this.send(c, buildFrame(FT.HEARTBEAT_ACK, f.cid, "", "", new Uint8Array()));
      return;
    }
    if (f.ft === 0x24 || f.ft === FT.SYSTEM_EVENT) return;
    if (![FT.ASK,FT.TELL,FT.REPLY,FT.REPLY_ERR].some(type => type === f.ft) || (f.flags & ~10) || f.typeKey === "parrot:durable#v1")
      throw new Error("unsupported execution envelope");
    if (f.ft === FT.REPLY || f.ft === FT.REPLY_ERR) {
      const error = f.ft === FT.REPLY_ERR ? decodeErrPayload(f.payload) : undefined;
      const p = this.pending.get(f.cid);
      if (p?.connection === c) {
        if (error) p.reject(new Error(`remote err ${error.code}: ${error.detail}`)); else p.resolve(f.payload);
      }
      return;
    }
    const request = f.ft === FT.ASK ? splitReplyTo(f.payload) : undefined;
    if (request === null) throw new Error("malformed ASK prefix");
    let path = f.path;
    if (path.startsWith("parrot://")) {
      const prefix = `parrot://${this.opts.nodeId}/`;
      if (!path.startsWith(prefix)) {
        if (f.ft === FT.ASK) this.replyError(c, f, ErrCode.RouteUnreachable, "wrong destination node");
        return;
      }
      path = "/" + path.slice(prefix.length);
    }
    const actor = this.actors.get(path) ?? this.actors.get(path.replace(/^\/(?:lite\/)?user\//, ""));
    if (this.activeHandlers >= this.maxHandlers) {
      if (f.ft === FT.ASK) this.replyError(c, f, ErrCode.Overloaded, "handler limit reached");
      else throw new Error("handler limit reached");
      return;
    }
    if (f.ft === FT.ASK && !actor?.onAsk) {
      this.replyError(c, f, actor ? ErrCode.NotRemotable : ErrCode.ActorNotFound, "no ASK handler"); return;
    }
    if (f.ft === FT.TELL && !actor?.onTell) return;
    ++this.activeHandlers;
    void (async () => {
      try {
        const context = { signal: c.abort.signal };
        if (f.ft === FT.ASK) {
          const bytes = await actor!.onAsk!(f.typeKey, request!.payload, context);
          if (this.live(c)) this.send(c, buildFrame(FT.REPLY, f.cid, "", f.typeKey, bytes, 0, 0, 8, 0, c.peerLimit));
        } else await actor!.onTell!(f.typeKey, f.payload, context);
      } catch (error) {
        if (this.live(c)) {
          try {
            if (f.ft === FT.ASK) this.replyError(c, f, ErrCode.ProtocolViolation, asError(error).message);
            else this.retire(c, asError(error));
          } catch (failure) { this.retire(c, asError(failure)); }
        }
      } finally { --this.activeHandlers; }
    })();
  }
  private replyError(c: Connection, request: Frame, code: number, detail: string): void {
    // Bound user-provided exception text before encoding; encoding still checks
    // the exact negotiated frame size, including UTF-8 expansion.
    const budget = Math.max(0, Math.min(1024, c.peerLimit - 32));
    const encoder = new TextEncoder();
    const decoder = new TextDecoder("utf-8", { ignoreBOM: true });
    let text = "", used = 0;
    for (const scalar of detail) {
      const encoded = encoder.encode(scalar);
      const size = encoded.length;
      if (used + size > budget) break;
      // Application exceptions may contain lone UTF-16 surrogates. Preserve
      // their replacement bytes without weakening the strict Wire encoder.
      text += decoder.decode(encoded); used += size;
    }
    const payload = encodeErrPayload(code, text);
    this.send(c, buildFrame(FT.REPLY_ERR, request.cid, "", "", payload, 0, 0, 8, 0, c.peerLimit));
  }
  private ready(): Connection {
    const c = this.connection;
    if (!c || !this.live(c) || c.phase !== "ready") throw new Error("not connected");
    return c;
  }
  spawn(name: string, options: SpawnOpts): void {
    if (this.closed) throw new Error("closed");
    validateIdentity(name, true);
    if (!this.actors.has(name) && this.actors.size >= this.maxActors) throw new Error("actor limit reached");
    if ((options.onAsk !== undefined && typeof options.onAsk !== "function") ||
        (options.onTell !== undefined && typeof options.onTell !== "function")) throw new Error("invalid handler");
    this.actors.set(name, { ...options });
  }
  private registrationFrame(key: string, path: string, limit: number): Uint8Array {
    return buildFrame(FT.TELL, newCid(), path, `$receptionist.register:${key}`, new Uint8Array(), 0, 0, 8, 0, limit);
  }
  async register(key: string, path: string, _meta?: Record<string, unknown>): Promise<void> {
    const c = this.ready(); validateIdentity(key, true); validateIdentity(path, true);
    const exists = this.registrations.some(r => r.key === key && r.path === path);
    if (!exists && this.registrations.length >= this.maxRegistrations) throw new Error("registration limit reached");
    this.send(c, this.registrationFrame(key, path, c.peerLimit));
    if (!exists) this.registrations.push({ key, path });
  }
  async ask(path: string, typeKey: string, payload: Uint8Array, timeoutMs = 5000): Promise<Uint8Array> {
    positive(timeoutMs, 5000, "ASK timeout");
    const deadline = now() + timeoutMs;
    const c = this.ready();
    if (this.pending.size >= this.maxPending) throw new Error("pending limit reached");
    const cid = newCid();
    const frame = buildFrame(FT.ASK, cid, path, typeKey, withReplyToPrefix(`${this.opts.nodeId}/lite`, payload), 0, 0, 8, 0, c.peerLimit);
    if (now() >= deadline) throw new Error("ask timeout");
    const charge = frame.length;
    if (charge > this.maxPendingBytes - this.pendingBytes) throw new Error("pending byte limit reached");
    return new Promise<Uint8Array>((resolve, reject) => {
      const finish = (error?: Error, reply?: Uint8Array) => {
        if (this.pending.get(cid) !== p) return;
        this.pending.delete(cid); this.pendingBytes -= charge; clearTimeout(timer);
        if (error) reject(error); else resolve(reply!);
      };
      const p: Pending = { connection: c, resolve: bytes => now() >= deadline ? finish(new Error("ask timeout")) : finish(undefined, bytes), reject: error => finish(error) };
      const timer = setTimeout(() => finish(new Error("ask timeout")), Math.max(1, deadline - now()));
      this.pending.set(cid, p); this.pendingBytes += charge;
      try { this.send(c, frame); } catch (error) { finish(asError(error)); }
    });
  }
  tell(path: string, typeKey: string, payload: Uint8Array): void {
    const c = this.ready(); this.send(c, buildFrame(FT.TELL, newCid(), path, typeKey, payload, 0, 0, 8, 0, c.peerLimit));
  }
  private retire(c: Connection, error: Error): void {
    if (c.phase === "closed") return;
    c.phase = "closed"; clearTimeout(c.timer); c.timer = undefined;
    if (c.ws) {
      c.ws.onopen = c.ws.onmessage = c.ws.onclose = c.ws.onerror = null;
      try { c.ws.close(); } catch { /* logical ownership is already retired */ }
    }
    for (const p of this.pending.values()) if (p.connection === c) p.reject(error);
    c.abort.abort(); c.reject(error);
    if (this.connection === c) {
      this.connection = undefined;
      if (this.everReady && !this.closed && this.retry === undefined) {
        const wait = this.backoffMs; this.backoffMs = Math.min(wait * 2, 30000);
        this.retry = setTimeout(() => {
          this.retry = undefined;
          if (!this.closed) void this.dial().catch(() => {}); // retire owns retry scheduling
        }, wait);
      }
    }
  }
  close(): void {
    this.closed = true; clearTimeout(this.retry); this.retry = undefined;
    if (this.connection) this.retire(this.connection, new Error("closed"));
    this.actors.clear(); this.registrations = [];
  }
}
