/**
 * ParrotLite 客户端（DEV_03 §2.2 API 形态）。
 *
 * 传输降级链（§2.3）：WebTransport → WebSocket+二进制帧（兜底）——
 * Node/浏览器运行时探测自动选。本 P3 阶段先落 WS 兜底载体（Node 18+
 * 全可用），WebTransport 探测位预留（P4 CI matrix 扩展）。
 *
 * 断线重连：指数退避 1s→30s；重连成功重 handshake + receptionist 重注册
 * （云端按 (node_id,key,path) upsert 幂等）。
 */

import {
  FrameDecoder,
  FT,
  buildFrame,
  handshakeBody,
  handshakeAckBody,
  newCid,
  splitReplyTo,
  withReplyToPrefix,
  decodeErrPayload,
} from "./wire.js";

export interface ParrotLiteOptions {
  url: string; // "ws://gateway:8080"（P3 兜底载体）；"quic://" 预留
  nodeId: string;
  tls?: { cert?: string; key?: string; ca?: string };
  /** 测试注入（不注入则用全局 WebSocket）。 */
  wsFactory?: (url: string) => MinimalWS;
}

/** dial 所需的最小 WebSocket 面（测试桩实现此接口即可）。 */
export interface MinimalWS {
  readyState: number;
  send(data: Uint8Array): void;
  close(): void;
  onopen: (() => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
  onmessage: ((ev: { data: ArrayBuffer }) => void) | null;
}

export interface SpawnOpts {
  onAsk?: (typeKey: string, payload: Uint8Array) => Promise<Uint8Array>;
  onTell?: (typeKey: string, payload: Uint8Array) => void | Promise<void>;
}

interface Pending {
  resolve: (b: Uint8Array) => void;
  reject: (e: Error) => void;
}

export class ParrotLite {
  private ws?: MinimalWS;
  private dec = new FrameDecoder();
  private pending = new Map<bigint, Pending>();
  private actors = new Map<string, SpawnOpts>();
  private registrations: Array<{ key: string; path: string }> = [];
  private backoffMs = 1000;
  private closed = false;
  private isTcp = false; // tcp:// 直连形态（DEV_08：Node 环境跳过 WS 层）

  private constructor(private opts: ParrotLiteOptions) {}

  static async connect(opts: ParrotLiteOptions): Promise<ParrotLite> {
    const node = new ParrotLite(opts);
    await node.dial();
    return node;
  }

  private url(): string {
    // quic:// 前缀暂映射 WS 兜底（WebTransport 探测在 P4）
    return opts_url(this.opts.url);
  }

  private async dial(): Promise<void> {
    // DEV_08：tcp:// 且 Node net 可用 → 原生 socket 直连（省 WS 帧/握手
    // 开销与 utf8 校验；浏览器环境自动回落 WS 兜底链）。动态探测不加
    // @types/node（bundle 预算 50KB min+gz——类型层零依赖）。
    const raw = this.opts.url;
    const wantTcp = raw.startsWith("tcp://") || raw.startsWith("parrot://");
    if (wantTcp && !this.opts.wsFactory) {
      // 动态 require（tsc 零依赖：构造器形态绕开模块解析——bundle 不含
      // node:net，浏览器运行时抛错走 catch 回落 WS）
      type NodeSocket = {
        write: (b: Uint8Array) => boolean;
        destroy: () => void;
        on: (ev: string, cb: (buf?: { buffer: ArrayBuffer; byteOffset: number; length: number }) => void) => void;
      };
      type NodeNet = { connect: (port: number, host: string) => NodeSocket };
      let net: NodeNet | null = null;
      try {
        // Node ≥22: process.getBuiltinModule（ESM 安全）；旧 Node/浏览器:
        // Function 构造的 CommonJS require。均失败 → WS 兜底。
        const g = globalThis as unknown as {
          process?: { getBuiltinModule?: (m: string) => NodeNet };
        };
        if (typeof g.process?.getBuiltinModule === "function") {
          net = g.process.getBuiltinModule("net") ?? null;
        } else {
          const dynamicRequire = new Function("m", "return require(m)") as (m: string) => NodeNet;
          net = dynamicRequire("net");
        }
      } catch {
        net = null; // 浏览器/无 Node —— 走 WS 兜底
      }
      if (net) {
        const u = new URL(raw.replace(/^parrot/, "tcp"));
        const sock = net.connect(Number(u.port), u.hostname);
        const adapter: MinimalWS = {
          readyState: 0,
          send: (d) => void sock.write(d),
          close: () => void sock.destroy(),
          onopen: null,
          onclose: null,
          onerror: null,
          onmessage: null,
        };
        sock.on("connect", () => {
          adapter.readyState = 1;
          adapter.onopen?.();
        });
        sock.on("data", (buf?: { buffer: ArrayBuffer; byteOffset: number; length: number }) => {
          if (buf) adapter.onmessage?.({ data: buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.length) });
        });
        sock.on("close", () => adapter.onclose?.());
        sock.on("error", () => adapter.onerror?.());
        await new Promise<void>((res, rej) => {
          adapter.onopen = () => res();
          adapter.onerror = () => rej(new Error("tcp connect failed"));
        });
        this.ws = adapter;
        this.isTcp = true;
        this.startReceive(adapter);
        return;
      }
    }
    const ws = this.opts.wsFactory
      ? this.opts.wsFactory(this.url())
      : (new WebSocket(this.url()) as unknown as MinimalWS);
    (ws as { binaryType?: string }).binaryType = "arraybuffer";
    await new Promise<void>((res, rej) => {
      ws.onopen = () => res();
      ws.onerror = () => rej(new Error("ws connect failed"));
    });
    this.ws = ws;
    this.isTcp = false;
    this.startReceive(ws);
  }

  private startReceive(ws: MinimalWS): void {
    ws.onmessage = (ev) => this.onBytes(new Uint8Array(ev.data as ArrayBuffer));
    ws.onclose = () => this.reconnect();
    // 握手（pb-only caps——07 §8.1）
    this.send(buildFrame(FT.HANDSHAKE, newCid(), "", "__handshake__", handshakeBody(this.opts.nodeId)));
  }

  private send(data: Uint8Array): void {
    if (!this.ws || this.ws.readyState !== 1) throw new Error("not connected");
    this.ws.send(data);
  }

  private onBytes(data: Uint8Array): void {
    this.dec.feed(data);
    for (let f = this.dec.nextFrame(); f; f = this.dec.nextFrame()) {
      this.onFrame(f.ft, f.cid, f.path, f.typeKey, f.payload);
    }
  }

  private onFrame(ft: number, cid: bigint, path: string, typeKey: string, payload: Uint8Array): void {
    if (ft === FT.HANDSHAKE_ACK) {
      this.backoffMs = 1000; // 重连成功重置
      // 重注册（幂等 upsert——云端 (node_id,key,path) 主键）。
      // 快照遍历：register 会 push registrations——直接迭代会自增长死循环
      for (const r of [...this.registrations]) {
        void this.register(r.key, r.path).catch(() => {});
      }
      return;
    }
    // 心跳应答（hub 2s 探活——10s 无 ACK 判死；lite 必须回）
    if (ft === FT.HEARTBEAT) {
      this.send(buildFrame(FT.HEARTBEAT_ACK, cid, "", "", new Uint8Array(0)));
      return;
    }
    // ROUTE_HINT（0x24）：hub 注入直连地址。lite 走 ws/简化 tcp——
    // 直连学习暂不启用，吞帧（前向兼容，不断连）。
    if (ft === 0x24) {
      return;
    }
    if (ft === FT.ASK) {
      const actor = this.actors.get(path) ?? this.actors.get(stripPrefix(path));
      const rt = splitReplyTo(payload);
      const real = rt ? rt.payload : payload;
      const replyTo = rt?.replyTo ?? "";
      void (async () => {
        try {
          const handler = actor?.onAsk;
          if (!handler) throw new Error(`no actor at ${path}`);
          const out = await handler(typeKey, real);
          this.send(buildFrame(FT.REPLY, cid, "", typeKey, out));
        } catch (e) {
          this.send(
            buildFrame(
              FT.REPLY_ERR,
              cid,
              "",
              "",
              encodeErrLite(String(e)),
            ),
          );
        }
      })();
      return;
    }
    if (ft === FT.TELL) {
      const actor = this.actors.get(path) ?? this.actors.get(stripPrefix(path));
      void actor?.onTell?.(typeKey, payload);
      return;
    }
    if (ft === FT.REPLY) {
      this.pending.get(cid)?.resolve(payload);
      this.pending.delete(cid);
      return;
    }
    if (ft === FT.REPLY_ERR) {
      const { code, detail } = decodeErrPayload(payload);
      this.pending.get(cid)?.reject(new Error(`remote err ${code}: ${detail}`));
      this.pending.delete(cid);
    }
  }

  /** 注册本地 actor（lite 侧 spawn 语义）。 */
  spawn(name: string, opts: SpawnOpts): void {
    this.actors.set(name, opts);
  }

  /** receptionist 注册（断线重连自动重注册）。 */
  async register(key: string, path: string, _meta?: Record<string, unknown>): Promise<void> {
    this.registrations.push({ key, path });
    // TELL 形式的注册事件（桥/gateway 侧约定 type_key）
    this.send(buildFrame(FT.TELL, newCid(), path, `$receptionist.register:${key}`, new Uint8Array(0)));
  }

  /** 远程 ask。 */
  async ask(path: string, typeKey: string, payload: Uint8Array, timeoutMs = 5000): Promise<Uint8Array> {
    const cid = newCid();
    const body = withReplyToPrefix(`${this.opts.nodeId}/lite`, payload);
    this.send(buildFrame(FT.ASK, cid, path, typeKey, body));
    return new Promise<Uint8Array>((resolve, reject) => {
      const t = setTimeout(() => {
        this.pending.delete(cid);
        reject(new Error("ask timeout"));
      }, timeoutMs);
      this.pending.set(cid, {
        resolve: (b) => {
          clearTimeout(t);
          resolve(b);
        },
        reject: (e) => {
          clearTimeout(t);
          reject(e);
        },
      });
    });
  }

  /** 远程 tell。 */
  tell(path: string, typeKey: string, payload: Uint8Array): void {
    this.send(buildFrame(FT.TELL, newCid(), path, typeKey, payload));
  }

  private reconnect(): void {
    if (this.closed) return;
    const wait = this.backoffMs;
    this.backoffMs = Math.min(this.backoffMs * 2, 30_000); // 1s→30s
    setTimeout(() => {
      if (!this.closed) void this.dial().catch(() => this.reconnect());
    }, wait);
  }

  close(): void {
    this.closed = true;
    this.ws?.close();
    for (const p of this.pending.values()) p.reject(new Error("closed"));
    this.pending.clear();
  }
}

function opts_url(u: string): string {
  if (u.startsWith("quic://")) return u.replace(/^quic/, "ws"); // 降级链（§2.3）
  if (u.startsWith("tcp://")) return u.replace(/^tcp/, "ws"); // 浏览器兜底（Node 走原生直连）
  return u;
}

function stripPrefix(path: string): string {
  // "parrot://gw/lite/user/x" → "x"（网关侧路径映射兼容）
  const idx = path.indexOf("/user/");
  return idx >= 0 ? path.slice(idx + "/user/".length) : path;
}

function encodeErrLite(detail: string): Uint8Array {
  // 复用 wire 的错误体（避免循环引用——内联实现）
  const d = new TextEncoder().encode(detail);
  const out = new Uint8Array(4 + d.length);
  new DataView(out.buffer).setUint16(0, 6, true); // UnknownTypeKey 族
  out.set(d, 4);
  return out;
}
