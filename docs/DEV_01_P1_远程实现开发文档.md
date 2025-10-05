# DEV_01 · P1 远程实现开发文档（parrot-remote crate 施工图）

> 状态：**开发文档（实施合同）** · 2026-10-04 · 基准 commit `9e35fc0`
> 设计依据：[05 P1 详细设计](./TECH_DESIGN_05_P1远程实现详细设计.md)（协议细节以其为准）+ [07 协议 1.0](./TECH_DESIGN_07_异构联邦协议设计.md) §2-§4（帧/握手/错误/传输以 07 为准）+ POC 实证（`poc/Mx-poc/` 15 测试）
> 上位约束：[07 §14.6 E5](./TECH_DESIGN_07_异构联邦协议设计.md) 实现规约（依赖选型/分层/机制策略/结构化/三态可读）
> **本文档的作用**：开发者从 §0 读到 §12 即可开工，所有签名可直接编译（已对 `9e35fc0` 核实），所有设计决策已冻结（引用条款号），施工中不做设计决策。

---

## 0. 范围与红线

**做**：parrot-remote crate 全量（帧/传输/编解码/远程 ref/入站路由/系统组装）+ RemoteMessage derive 宏 + remote-bench + 语义复跑测试。

**不做（红线，违反=返工）**：
- 远程 spawn（**落在 DEV_02 §0 K0 管理协议**——SYSTEM_EVENT 管理子通道 + PropsFactory 按名构造；P1 仅预留 SYSTEM_EVENT 帧常量）
- gossip/membership（P2）
- pb 栈实现（接口占位返回 Unimplemented；P2 随 akka 网关交付——05 §3.3 范围收窄裁定）
- 压缩/tracing/URGENT 队列（flags 位已分配，行为不启用）
- FRAGMENT/RESOLVE/INVALIDATE 帧的**处理逻辑**（常量已定义；P1 收到 RESOLVE 系帧回 ERROR 断连，FRAGMENT P1 不实现收发）
- hop 中继（hop_count 写 0/hop_limit 写 8，不做转发——P5 拓扑层）

**交付物**：`parrot-remote` crate（workspace 第 5 成员）+ `parrot-api` 最小增量（RemoteMessage trait）+ `parrot-api-derive` 增量（remote.rs）+ `parrot/tests/test_remote_semantics.rs` + `bench/remote-bench/`。

---

## 1. 任务分解（依赖顺序）

```
T1 帧（frame.rs）── 无依赖，纯函数，最先做（一切的地基）
T2 传输（transport.rs + tcp.rs）── 依赖 T1（FrameCodec）
T3 编解码（codec.rs + codec_registry.rs + RemoteMessage trait + derive 宏）── 依赖 T1（TYPE_KEY 定义）
T4 远程 ref（ref.rs + registry.rs 回调表）── 依赖 T2/T3
T5 入站路由（ingress.rs）── 依赖 T4
T6 系统组装（system.rs + node.rs + lib.rs + facade 集成）── 依赖全部
T7 语义复跑（test_remote_semantics.rs）── 依赖 T6
T8 基准（remote-bench）── 依赖 T6
```

估算与 05 §0 对齐（T1=3d T2=3d T3=3d T4=3d T5=2d T6=2d T7=2d T8=2d 缓冲 3d）。每个 T 的**测试义务**在 §3 各模块尾部列出——全绿才进下一个 T。

---

## 2. crate 骨架与依赖

```toml
# parrot-remote/Cargo.toml
[package]
name = "parrot-remote"
version = "0.1.0"
edition = "2021"

[dependencies]
parrot-api = { path = "../parrot-api" }
tokio = { version = "1", features = ["net", "time", "sync", "io-util", "rt-multi-thread", "macros"] }
tokio-util = { version = "0.7", features = ["codec"] }
bytes = "1"
bincode = "2"
serde = { version = "1", features = ["derive"] }
inventory = "0.3"
async-trait = "0.1"
thiserror = "2"
tracing = "0.1"
futures = "0.3"

# workspace Cargo.toml members 增 "parrot-remote"

[features]
default = ["tcp"]
mem  = []   # MemoryTransport 测试载体（§10.2 回环集成；RC1–RC8 mem+tcp 双跑）
tcp  = []   # TcpTransport（P1 主载体）
# quic = [] # P2 增（DEV_02 K3）
```

依赖合规：全部在 [DECISIONS_DEPENDENCIES.md](./DECISIONS_DEPENDENCIES.md) 白名单内（tokio/bincode/serde/inventory/thiserror/tracing 已裁定；`tracing` 补录一行——日志三态可读 E5.5 的载体）。**不引入** uuid（cid 用 u64，调试可读性靠 PARROT_TRACE 帧摘要，不靠 uuid）——比 05 §11 更收紧。

文件清单（05 §0 原样 + `handshake.rs` 独立成文件，理由见 §3.2）：

```
parrot-remote/src/
├── lib.rs              # //! 职责：公共 API 出口（RemoteActorSystem/RemoteConfig/spawn 辅助）
├── frame.rs            # //! 职责：Wire 1.0 帧字节 ↔ 结构，不知道任何传输细节
├── handshake.rs        # //! 职责：TLV 握手体编解码 + 协商规则
├── transport.rs        # //! 职责：Transport trait + ConnectionHandle 抽象
├── transport/tcp.rs    # //! 职责：TcpTransport（tokio TcpStream + Framed）
├── transport/memory.rs # //! 职责：MemoryTransport（tokio duplex，测试确定性）
├── node.rs             # //! 职责：NodeId/NodeAddr/NodeTable（连接状态机）
├── codec.rs            # //! 职责：CodecStack trait + bin 栈实现（pb 占位）
├── codec_registry.rs   # //! 职责：TYPE_KEY → 编解码函数表（inventory 收集）
├── envelope.rs         # //! 职责：RemoteEnvelope（线上语义单元）
├── ref.rs              # //! 职责：RemoteActorRef（ActorRef trait 实现）
├── registry.rs         # //! 职责：CallbackRegistry（cid → oneshot 回调表）
├── ingress.rs          # //! 职责：入站帧分发（ASK/TELL→本地 actor；REPLY→回调表）
├── system.rs           # //! 职责：RemoteActorSystem 组装与生命周期
└── error.rs            # //! 职责：RemoteError + 错误码表（07 §2.5）+ ActorError 映射
```

---

## 3. 逐模块施工图

### 3.1 frame.rs —— T1

**常量**（与 07 §2.1/§2.2/§2.3 逐字节一致，任何偏差=协议违规）：

```rust
//! 职责：Wire 1.0 帧字节 ↔ 结构，不知道任何传输细节（07 §2.1）。

pub const PROTOCOL_VERSION: u8 = 0x01;
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024; // 16 MiB
pub const DEFAULT_HOP_LIMIT: u8 = 8;
pub const FIXED_HEADER_SIZE: usize = 24;  // frame_len→reserved 定长段
pub const BODY_FIXED_OVERHEAD: usize = 28; // + path_len(4)

pub mod frame_type {
    pub const HANDSHAKE: u8 = 0x01;
    pub const HANDSHAKE_ACK: u8 = 0x02;
    pub const HEARTBEAT: u8 = 0x03;
    pub const HEARTBEAT_ACK: u8 = 0x04;
    pub const ASK: u8 = 0x10;
    pub const REPLY: u8 = 0x11;
    pub const REPLY_ERR: u8 = 0x12;
    pub const TELL: u8 = 0x13;
    pub const STOP: u8 = 0x14;
    pub const FRAGMENT: u8 = 0x15;      // P1 不实现，仅常量（解码遇之报 UnknownFrameType）
    pub const SYSTEM_EVENT: u8 = 0x20;  // P1 收到即断连（协议违规）
    pub const RESOLVE_Q: u8 = 0x21;     // P1 同上
    pub const RESOLVE_R: u8 = 0x22;
    pub const INVALIDATE: u8 = 0x23;
    pub const ERROR: u8 = 0x7F;
}

pub mod flags {
    pub const COMPRESSED_ZSTD: u16 = 1 << 0; // P2
    pub const TRACING: u16 = 1 << 1;         // P2
    pub const BATCH: u16 = 1 << 2;           // P4
    pub const URGENT: u16 = 1 << 3;          // P2
    pub const TELL_ACK: u16 = 1 << 4;        // P3
    pub const APP_ENCRYPTED: u16 = 1 << 5;   // 1.0 可选
}
```

**核心类型**（`Frame` 拥有 path/type_key/payload；`FrameHeader` 只是定长段视图）：

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_len: u32,
    pub version: u8,
    pub frame_type: u8,
    pub flags: u16,
    pub correlation_id: u64,
    pub hop_count: u8,
    pub hop_limit: u8,
    // reserved u48：低 32bit = 端到端 seq（TELL 重排用，0=不参与——
    // 与旧实现/异构网关字节兼容）；高 16bit 仍保留 0（非 0 拒帧）
    pub seq: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub path: String,
    pub type_key: String,
    pub payload: Bytes,
}

impl Frame {
    /// 编码进 buf（含帧头）。错误：path/type_key 超 u32、总长超 MAX_FRAME_LEN。
    pub fn encode(&self, buf: &mut BytesMut) -> Result<(), FrameError>;
    /// 从 buf 解码一帧；不足一帧返回 Ok(None) **不消费**（POC RC1 锁定行为）。
    pub fn decode(buf: &mut BytesMut) -> Result<Option<Frame>, FrameError>;
    /// 构造辅助（header 字段用默认值填充 hop 0/limit 8/flags 0）
    pub fn ask(cid: u64, path: &str, type_key: &str, payload: Bytes, reply_to: Option<&str>) -> Frame;
    pub fn tell(path: &str, type_key: &str, payload: Bytes) -> Frame;
    pub fn reply(cid: u64, path: &str, type_key: &str, payload: Bytes) -> Frame;
    pub fn reply_err(cid: u64, path: &str, code: ErrCode, detail: &str) -> Frame;
    pub fn stop(path: &str) -> Frame;
    pub fn heartbeat() -> Frame;
    pub fn heartbeat_ack() -> Frame;
    pub fn error_frame(code: ErrCode, detail: &str) -> Frame;
}
```

**注意**：`reply_to` 的传递方式——P1 采用**类型键约定**：ASK 帧的 `type_key` 字段照常是消息类型；`reply_to` 路径写入 `path` 前缀交换：**ASK.path = 目标路径，reply_to 放 payload 头部**（`[u32 reply_to_len][reply_to_bytes][payload...]`，仅 ASK 帧）。这是对 05 §4（RemoteEnvelope.reply_to 字段）的落地方案——05 的 RemoteEnvelope 是语义模型，线上承载见本条。解码侧 ingress 收 ASK 后先剥 reply_to 前缀。

**FrameError（三态可读，E5.5）**：

```rust
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("帧超限：声明长度 {len} > MAX_FRAME_LEN={max}（偏移 0）")]
    TooLarge { len: u32, max: u32 },
    #[error("版本不符：期望 {expect} 实得 {got}（偏移 4）")]
    VersionMismatch { expect: u8, got: u8 },
    #[error("未知帧类型 0x{got:02X}（偏移 5）——两端协议版本漂移")]
    UnknownFrameType { got: u8 },
    #[error("长度不自洽：frame_len={flen} 但 path_len={plen}+key_len={klen}+固定28 超出（偏移 24）")]
    MalformedLengths { flen: u32, plen: u32, klen: u32 },
    #[error("UTF-8 解码失败（字段 {field}）：{source}")]
    Utf8 { field: &'static str, source: std::str::Utf8Error },
    #[error("reserved 非 0（偏移 18，实得 0x{got:012X}）——发送端实现有误")]
    ReservedNotZero { got: u64 },
    #[error("hop_count={count} ≥ hop_limit={limit}（偏移 16/17）——丢弃并回 RouteUnreachable")]
    HopExceeded { count: u8, limit: u8 },
}
```

每条错误带偏移/期望/实际——`Display` 直接人类可读；`ErrCode::from(&FrameError)` 机器可读（映射到 12 ProtocolViolation / 6 UnknownTypeKey 等）。

**golden vectors（T1 测试义务的一部分，写死在 `frame.rs` 尾部 `#[cfg(test)]`）**：每 frame_type 至少 1 组 `[u8]` 逐字节断言 + 每错误路径 1 组畸形向量（截断/超限/坏版本/坏 UTF-8/reserved 脏/HopExceeded）。向量同时导出为 `pub const GOLDEN_VECTORS: &[GoldenVector]`（P2 JVM/TS 引用同一组——07 §2.6）。

**T1 测试义务**：`frame_roundtrip`（全 frame_type 编码→解码==原）、`frame_partial`（1..N-1 字节各试一次，全部 Ok(None) 且 buf 未消费）、`frame_golden`、`frame_malformed`（每错误路径）。

### 3.2 handshake.rs —— T1（与 frame.rs 同任务）

TLV 编解码（07 §2.4 冻结格式）：

```rust
//! 职责：TLV 握手体编解码 + 能力协商规则（07 §2.4，X3 裁定：一次定死）。

pub mod tlv_tag {
    pub const NODE_ID: u8 = 1;
    pub const REALM: u8 = 2;
    pub const CLUSTER: u8 = 3;
    pub const CAPABILITIES: u8 = 4;  // u32 LE：bit0 bin / bit1 pb / bit2 zstd / bit3 quic / bit4 ws
    pub const MAX_FRAME_LEN: u8 = 5; // u32 LE
    pub const TOPOLOGY_ROLE: u8 = 6; // u8：0 普通/1 hub/2 border/3 directory
    pub const HOP_LIMIT: u8 = 7;     // u8
}

pub struct HandshakeBody { /* node_id/realm/cluster/capabilities/max_frame_len/topology_role/hop_limit */ }
pub struct HandshakeAckBody { /* 同族字段 + chosen_codec */ }

impl HandshakeBody {
    pub fn encode_tlv(&self, buf: &mut BytesMut);        // [tag][u16 len][bytes] 序列
    pub fn decode_tlv(payload: &[u8]) -> Result<Self, HandshakeError>;
}
pub fn negotiate_caps(a: u32, b: u32) -> Result<u32, ErrCode>; // 按位与；0 公共栈 → ErrCode::NoCommonCodec
```

错误：未知 tag **跳过不报错**（前向兼容——旧端不认识新 tag 应忽略而非断连）；重复 tag / 必填缺失（node_id/capabilities/max_frame_len/topology_role）→ `HandshakeError`。

**T1 测试义务**：`handshake_tlv_roundtrip`、`handshake_unknown_tag_skipped`、`handshake_missing_required`、`handshake_negotiation`（正常/无公共栈/版本错三路径，用 mock transport 走完整时序）。

### 3.3 transport.rs + transport/tcp.rs + transport/memory.rs —— T2

```rust
//! 职责：Transport trait——连接建立的机制；载体（tcp/quic/ws/mem）是策略（E5.3）。

#[async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn connect(&self, addr: &NodeAddr) -> Result<ConnectionHandle, RemoteError>;
    async fn listen(&self, bind: SocketAddr) -> Result<(), RemoteError>;
    async fn accept(&self) -> Result<ConnectionHandle, RemoteError>;
    fn scheme(&self) -> &'static str; // "tcp" | "quic"(P2) | "ws"(P3) | "mem"
}

/// 一条已建立的连接。写侧 mpsc 串行化；读侧由 ConnectionTask 驱动（§3.6 ingress）。
pub struct ConnectionHandle {
    pub node_id: String, // 握手后填充（connect 侧在握手完成前为空）
    tx: mpsc::Sender<Frame>,       // 出站队列（容量 1024，天然反压）
    pub info: ConnectionInfo,      // 本地/对端 SocketAddr、scheme、established_at
}
```

**TcpTransport 实现要点**（05 §2.2）：
- `Framed<TcpStream, FrameCodec>`；`FrameCodec` 是 `tokio_util::codec` 的薄封装（encode→`Frame::encode`，decode→`Frame::decode`）
- 写任务：单任务循环 `rx.recv() → framed.send(frame).await → flush`；channel 关闭即退出
- `set_nodelay(true)` 必须（LAN µs 级 RTT 前提）
- 心跳由 ConnectionTask 驱动（§3.6），Transport 不含定时器——**机制（连接）与策略（定时参数）分离**

**MemoryTransport**（测试用，`tokio::io::duplex(64 * 1024)` 双管道）：`mem://test-{n}` 端点；connect/listen/accept 与 TCP 同语义。**所有语义测试（T7）首选 mem 跑，TCP 跑第二遍**——CI 无端口依赖。

**T2 测试义务**：`tcp_connect_handshake_roundtrip`（双端各 spawn transport，A connect B listen，完成握手交换 node_id）、`tcp_partial_write`（对端慢读 1B/次，帧不丢不坏）、`mem_transport_parity`（同一组帧序列 mem/tcp 结果等价）。

### 3.4 codec.rs + codec_registry.rs + RemoteMessage —— T3

**parrot-api 增量**（`message.rs` 尾部追加，最小公共面）：

```rust
/// 远程可达消息契约（05 §3.1）。实现由 #[derive(RemoteMessage)] 生成，禁止手写 impl。
pub trait RemoteMessage: Message + Serialize + DeserializeOwned {
    /// 格式："bin:{crate}::{Type}#v{n}" / "pb:{package}.{Message}"（07 §2.1 type_key 规范）
    const TYPE_KEY: &'static str;
}
pub use inventory; // 转出口（宏生成的 inventory::submit! 需要）
```

**codec.rs**：

```rust
pub enum CodecStack { Bin, Pb } // Pb P1 占位：encode/decode 返回 ErrCode::Unimplemented
```

**codec_registry.rs**（05 §3.2 原样，含懒静态）：

```rust
pub struct CodecRegistry { /* by_key + by_type_id 双索引 */ }
impl CodecRegistry {
    pub fn global() -> &'static Self;              // OnceLock，首次访问 collect inventory
    pub fn register(entry: CodecEntry);            // 重复 TYPE_KEY panic（strict 模式）
    pub fn key_of(&self, tid: TypeId) -> Option<&'static str>;
    pub fn get(&self, key: &str) -> Option<&CodecEntry>;
    pub fn dump(&self) -> Vec<(String, CodecStack)>; // schema-diff 工具的钩子（P2）
}
```

**derive 宏**（`parrot-api-derive/src/remote.rs` 新文件）：

```rust
#[proc_macro_derive(RemoteMessage, attributes(remote))]
// 生成：
//   impl RemoteMessage for T { const TYPE_KEY: &'static str = "bin:{crate}::{Type}#v1"; }
//   inventory::submit! { CodecRegistration { type_key, type_id: TypeId::of::<T>(), encode, decode } }
// 属性：#[remote(key = "pb:pkg.Msg")] 显式覆盖键
// 编译错误指引：检测不到 Serialize derive 时生成带修复行号的 error!（降低采纳摩擦，05 §8）
```

**版本后缀规则**（06 I1）：默认键 `#v1`；字段布局变更时用户显式 `#[remote(key = "bin:mycrate::Msg#v2")]`——schema-diff 工具（P2）对拍。

**T3 测试义务**：`codec_registry_register_lookup`、`codec_duplicate_key_panics`、`codec_unknown_type_notremotable`、`derive_remote_message_generates_key`（在 parrot-api-derive-tests 里加 fixtures crate 用宏）、`bin_stack_roundtrip`（含嵌套 enum/Vec/大字符串）。

### 3.5 envelope.rs + registry.rs（回调表）—— T4 前置

envelope.rs 即 05 §4 的 RemoteEnvelope（语义模型，不直接上线——上线承载是 Frame + ASK payload 头部 reply_to 约定，§3.1）。

registry.rs（回调表，05 §6.2）：

```rust
//! 职责：cid → oneshot 映射；ask 的等待端在这里挂起。

pub struct CallbackRegistry {
    next_cid: AtomicU64,
    slots: Mutex<HashMap<u64, oneshot::Sender<ReplyPayload>>>,
    capacity: usize, // 默认 65536，超出拒绝新 ask（E2.2 资源边界）
}
pub enum ReplyPayload { Ok(Bytes, String /*type_key*/), Err(ErrCode, String) }

impl CallbackRegistry {
    pub fn next_cid(&self) -> u64;
    pub fn insert(&self, cid: u64, tx: oneshot::Sender<ReplyPayload>) -> Result<(), RemoteError>; // 满=QueueFull
    pub fn remove(&self, cid: u64) -> bool;
    pub fn complete(&self, cid: u64, payload: ReplyPayload) -> bool; // false=迟到回复→metric+drop（ADR-10）
    pub fn fail_all(&self, code: ErrCode, detail: &str) -> usize;    // 断连时清空（POC 实证路径）
}
```

**T4 前置测试义务**：`callback_insert_remove_complete`、`callback_capacity_rejects`、`callback_fail_all`、`callback_late_reply_dropped`。

### 3.6 ref.rs + ingress.rs —— T4/T5

**RemoteActorRef**（实现 `parrot_api::address::ActorRef`——**以真实签名为准**，`parrot-api/src/address.rs:136`，非 05 伪码）：

```rust
#[derive(Clone)]
pub struct RemoteActorRef {
    path: String,                        // parrot://node/system/user/uuid
    node_id: String,
    inner: Arc<RemoteInner>,             // nodes: NodeTable / callbacks / weak system
}

#[async_trait]
impl ActorRef for RemoteActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send_with_timeout(msg, None)
    }
    fn send_with_timeout<'a>(&'a self, msg: BoxedMessage, timeout: Option<Duration>)
        -> BoxedFuture<'a, ActorResult<BoxedMessage>>
    { /* §3.1 ASK 流程：encode_outgoing → cid → oneshot 注册 → 发帧 → await/超时清理 */ }
    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> { /* TELL */ }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> { /* STOP 帧 + 固定 5s 等待本地确认（P1 简化） */ }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> { /* NodeTable status==Connected（弱一致，文档明示） */ }
    fn path(&self) -> String { self.path.clone() }
    fn clone_boxed(&self) -> BoxedActorRef { Box::new(self.clone()) }
    fn as_any(&self) -> &dyn Any { self }
}
```

**ask 完整时序**（实现必读）：

```text
1. encode_outgoing(msg) → (type_key, payload)；失败（NotRemotable）立即返回，不发帧（RC6）
2. cid = callbacks.next_cid()；(tx, rx) = oneshot()
3. callbacks.insert(cid, tx)？满 → ErrCode::Overloaded 快速失败（E2.2）
4. frame = Frame::ask(cid, path, key, payload, reply_to=parrot://{self_node}/_remote/reply)
5. nodes.send(node_id, frame).await？失败 → 回调已清（ConnectionLost）→ 返回错
6. match timeout { None → rx.await, Some(d) → tokio::time::timeout(d, rx) }
   - 超时：callbacks.remove(cid)；Err(TimeoutDetail("remote ask {path} after {d:?}"))（ADR-10：调用方放弃，对端可能仍处理——RC4）
   - rx Err（连接任务死）：ConnectionLost
7. ReplyPayload::Ok(bytes, key) → registry.get(key).decode(bytes) → BoxedMessage
   （对端解码类型与本地不同 → CodecError；这正是 RC3 串扰防护）
```

**ingress.rs 处理矩阵**（05 §6.1 原样 + 落地细节）：

| 帧 | 动作 |
|---|---|
| ASK | 剥 payload 头 reply_to → 本地 `facade.get_actor(path)`（§3.7 集成）→ miss → REPLY_ERR(code 1 ActorNotFound, path) → hit → `local.send_with_timeout(decoded, None)`（**远端不二次超时**，07 §7）→ Ok→REPLY(cid, reply_to, key_of(回复类型), encoded)；Err→REPLY_ERR(code 由 §3.8 映射表) |
| TELL | deliver；无回程；目标 miss → 记 metric（死信计数）+ debug 日志（无回程信道） |
| STOP | local.stop()；无回程 |
| REPLY/REPLY_ERR | callbacks.complete(cid, ...)；miss → metric late_reply_dropped |
| HEARTBEAT(_ACK) | 更新 last_seen；ACK 直接吞 |
| SYSTEM_EVENT/RESOLVE*/FRAGMENT | P1：ERROR 帧 + 断连（协议违规，红线内行为） |
| ERROR | tracing::warn + 断连 |

**ConnectionTask（连接驱动器，每连接一个 tokio 任务）**——transport.rs 内：

```text
loop select! {
  biased;
  _ = &mut shutdown => 发 leaving 语义（P1 无 gossip，仅 drain）→ break,
  frame_rx.recv() => framed.send(frame),            // 出站（mpsc 反压链）
  r = framed.next() => match r {
      Some(Ok(frame)) => {
          last_seen 更新;
          if frame 是 HEARTBEAT → 回 HEARTBEAT_ACK; ingress.dispatch(frame); }
      Some(Err(e)) => 断连流程; None => 对端关闭 → 断连流程;
  }
  _ = heartbeat_tick(2s) => 发 HEARTBEAT; 连续 5 次(10s)未收到任何帧 → 半开判定 → 断连流程,
}
断连流程：callbacks.fail_all(ConnectionLost) → NodeTable 状态 Disconnected → 重连任务（退避 1/2/4/8…30s ±20% 抖动）
```

**P1 入站串行**：每连接单任务顺序处理帧（HOL 是已知特性，P2 拆 worker 池——06 I2，文档明示不修补）。

**T4/T5 测试义务**：`ref_ask_mem`（回环 ask/tell/stop/is_alive）、`ref_notremotable_no_frame`、`ref_timeout_late_reply`（RC4 语义）、`ingress_dispatch_matrix`（§3.6 矩阵每行一测）。

### 3.7 system.rs + node.rs + lib.rs + facade 集成 —— T6

**RemoteConfig / RemoteActorSystem**（05 §7 原样）+ 关键集成差异（**与 05 伪码不同，以真实代码为准**）：

facade `ParrotActorSystem`（`parrot/src/system.rs:139`）**不改枚举 `ActorSystemImpl`**——remote 不是引擎。新增字段 + 方法：

```rust
// parrot/src/system.rs 新增（与 register_thread/actix_system 同族）
pub struct ParrotActorSystem { /* 既有字段 */ remote: RwLock<Option<Arc<dyn RemoteGateway>>> }
pub trait RemoteGateway: Send + Sync {                 // parrot crate 内定义（不依赖 parrot-remote！）
    fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>>;  // parrot:// 前缀 → RemoteActorRef
}
impl ParrotActorSystem {
    pub async fn register_remote_gateway(&self, gw: Arc<dyn RemoteGateway>) -> Result<(), SystemError>;
}

// internal_get_actor（system.rs:298）头部插入：
//   if path.path.starts_with("parrot://") && let Some(r) = self.remote_lookup(&path.path) { return Some(r) }
//   （在"默认系统 → 遍历"之前；本地命中优先级不变——§3.2 解析管线 ①）
```

**为什么要 RemoteGateway trait 而非直接依赖**：`parrot` crate 若 import `parrot-remote` 则形成 `parrot-remote → parrot-api` + `parrot → parrot-remote` 双向（parrot 同时依赖 parrot-api）——不构成环但引入**可选依赖耦合**（不用 remote 的用户也被拖编译）。trait 倒置后 `parrot` 只认接口，`parrot-remote` 在应用层组装时注入（E5.2 分层铁律：桥/远程件只依赖 parrot-api，网关模式同构）。

**node.rs**（05 §2.3 原样）：NodeTable/NodeState/静态种子/add_seed API；重连任务在 system.start 时 spawn。

**lib.rs 公共 API**：

```rust
pub use frame::{Frame, FrameHeader, FrameError, frame_type, flags};
pub use handshake::{HandshakeBody, HandshakeAckBody, negotiate_caps};
pub use transport::{Transport, ConnectionHandle, ConnectionInfo};
pub use transport::tcp::TcpTransport;
pub use transport::memory::MemoryTransport;
pub use codec::{CodecStack};
pub use codec_registry::CodecRegistry;
pub use envelope::RemoteEnvelope;
pub use ref_::RemoteActorRef;      // ref 是关键字，模块名 ref_，导出名不变
pub use registry::CallbackRegistry;
pub use node::{NodeAddr, NodeTable, NodeId};
pub use system::{RemoteActorSystem, RemoteConfig, Role};
pub use error::{RemoteError, ErrCode};
```

**T6 测试义务**：`system_start_listen_connect`（双节点 mem/tcp 各一遍）、`facade_remote_lookup`（parrot:// 经 facade 命中 RemoteActorRef）、`facade_local_first`（本地存在同名时本地优先——解析管线顺序断言）。

### 3.8 error.rs —— T3 起全程

```rust
/// 07 §2.5 十三项错误码表——逐条硬编码，禁止重排（wire 契约）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ErrCode {
    ActorNotFound = 1, Timeout = 2, Stopped = 3, NotRemotable = 4,
    CodecError = 5, UnknownTypeKey = 6, RouteUnreachable = 7, ConnectionLost = 8,
    DirectoryStale = 9, Overloaded = 10, NoCommonCodec = 11, ProtocolViolation = 12,
    Forbidden = 13,
}

impl ErrCode {
    /// 接收侧映射（07 §2.5 第三列，05 §9 策略不变：不新增 ActorError 变体）
    pub fn to_actor_error(self, detail: String) -> ActorError { /* match 13 行 */ }
    /// 发送侧映射：本地 ActorError → (ErrCode, detail)
    pub fn from_actor_error(e: &ActorError) -> (Self, String) { /* match 11 变体 */ }
}
```

REPLY_ERR payload 编解码：`[u16 code][u16 rsv=0][utf-8 detail]`——放 error.rs（`encode_err_payload`/`decode_err_payload`）。

**测试义务**：`errcode_table_frozen`（13 项逐一断言数值与字符串名——防手滑重排）、`errcode_roundtrip_actor_error`（11 变体 ↔ code 双向）。

---

## 4. 集成锚点汇总（对 9e35fc0 核实；master 演进后重核此表）

| 锚点 | 位置 | 用途 |
|---|---|---|
| `ActorRef` trait | `parrot-api/src/address.rs:136` | RemoteActorRef 实现（send/send_with_timeout/deliver/stop/path/is_alive/clone_boxed/as_any——**八方法全签名已核**） |
| `ActorPath` | `parrot-api/src/address.rs:55`（target: WeakActorTarget + path: String） | facade get_actor 入参；RemoteGateway 查询用 path.path 字符串 |
| `BoxedMessage` 等别名 | `parrot-api/src/types.rs:48-89` | `Box<dyn Any + Send>`；encode_outgoing 的输入 |
| `WeakActorTarget` | `parrot-api/src/types.rs:55`（= `Arc<dyn ActorRef>`） | ActorPath.target；远程路径不持有 target（placeholder 模式，address.rs:77 已有先例） |
| facade `ParrotActorSystem` | `parrot/src/system.rs:139`；`internal_get_actor:298` | 新增 remote 字段/RemoteGateway/register_remote_gateway；get_actor 头部插远程分支 |
| 注册族 API | `system.rs:183`（register_thread_system）等 | register_remote_gateway 与之同族（命名/签名风格对齐） |
| `ActorError` 11 变体 | `parrot-api/src/errors.rs:55` | ErrCode 双向映射的输入 |
| derive crate | `parrot-api-derive/src/lib.rs`（mod actor/message/typed） | 新增 mod remote |
| 背压/调度配置 | `parrot/src/thread/config.rs` | ingress 反压链的远端行为来源（ADR-12） |

---

## 5. 语义复跑（T7：test_remote_semantics.rs）

RC1–RC8 逐条（05 §10.3 原样），双跑（mem + tcp 127.0.0.1）：

| 用例 | 断言要点（实现提示） |
|---|---|
| RC1 恰好一次 | 128 并发 ask 跨网，回复数==128，cid 无重复命中（CallbackRegistry 计数器断言） |
| RC2 FIFO | 同源连续 100 TELL 保序（单连接串行入站保证；对端 actor 记序号验证） |
| RC3 回复路由 | 双 actor 交叉 ask，cid 无串扰（RC1 的加强版） |
| RC4 超时语义 | ask 1ms 超时 + 对端 500ms 慢处理 → 调用方 Timeout；对端实际完成；迟到 REPLY 查表 miss → late_reply_dropped +1 |
| RC5 死信 | stop 后 send → REPLY_ERR(code 3 Stopped) 跨网等价 |
| RC6 NotRemotable | 未注册类型 ask → 本地立即错（mock transport 断言零帧发出） |
| RC7 断连恢复 | drop 底层 duplex/tcp → ask 得 ConnectionLost；重连后 ask 成功且 cid 单调不回绕 |
| RC8 反压贯通 | 对端 Block 策略 + 慢消费 → 发送方 deliver 挂起时长 > 慢消费窗口（挂起时间断言） |

---

## 6. 基准（T8：remote-bench）

场景（对齐 engine_stress 命名与 Report 结构，engine 列 "remote-tcp"）：seq-ask / conc-ask c8/c64 / tell-throughput / pingpong。双进程 127.0.0.1（真实 TCP），预热 3s，采样 ≥30s，p50/p99/max+吞吐。

**门禁（E1.9，超标阻塞发布）**：ask p50 <150µs、tell <60µs。基线对比表自动生成（remote 税 = RTT − 本地 thread↔actix 基线 ~2-5µs）。

**CLI 形态**（DEV_00 §4.2 引用的两个二进制，此处定义 owner）：
- `remote-bench --gate`：跑上述场景并按门禁判定 pass/fail（exit code 0/1），供 CI 与 DEV_00 §3.4 验收命令直接调用。
- `dump-vectors`：将 `GOLDEN_VECTORS` 导出为 `docs/vectors/wire1.json` 格式（stdout），供 DEV_00 §6 冻结断言 `diff docs/vectors/wire1.json <(cargo run --bin dump-vectors)` 使用。

---

## 7. DoD（全部满足才算 P1 完成）

1. `cargo test --workspace --release` 全绿（632 基线 + parrot-remote/derive 增量全过）
2. RC1–RC8 mem+tcp 双跑绿
3. remote-bench 门禁达标
4. golden vectors 冻结（`GOLDEN_VECTORS` pub 导出 + 四语言共享文件 `docs/vectors/wire1.json` 落库）
5. `PARROT_TRACE=frame` 冒烟：帧摘要单行（ft/cid/hop/path/key/len）逐字段可读（E5.5）
6. 覆盖率：parrot-remote 行 ≥85% / 分支 ≥75%（E1.8）
7. 零 warning（`RUSTFLAGS="-D warnings"`，E1.6）；unsafe 零新增（E1.7，本项目不含 unsafe）
8. 04 §13 P1 勾选 + ADR-18 状态草案→已实施(P1 部分) + 本文 §4 锚点表随 master 重核

---

## 8. 实现注意事项（POC 已踩坑 + 设计未明说）

1. **ref 是 Rust 关键字**：模块文件名 `ref.rs` 时 `mod ref` 非法——用 `mod ref_` + `pub use ref_::RemoteActorRef`。
2. **Bytes vs Vec<u8>**：Frame.payload 用 `bytes::Bytes`（零拷贝切片）；Frame::decode 内部 `buf.split_to(n).freeze()`——避免每次解码复制。
3. **async_trait 必须**：ActorRef trait 已是 `#[async_trait]`（address.rs），RemoteActorRef 实现同标注。
4. **tokio::test 多节点**：mem 测试用 `#[tokio::test(flavor = "multi_thread")]`——单线程 flavor 下双节点任务可能死锁（POC 教训）。
5. **握手竞态**：connect 侧发 HANDSHAKE 后**必须先等 ACK 再放行数据帧**（ConnectionHandle 在握手完成前不返回）；accept 侧同理。P1 用两条 mpsc 严格控制时序，勿用 sleep 同步。
6. **cid 回绕**：AtomicU64 单调，2^64 不现实回绕；但**重连后 cid 不重置**（RC7 断言）——回调表随连接清空但计数器保留，防新旧连接 cid 撞车。
7. **late reply 的 metric**：`late_reply_dropped_total` 必须从 P1 就有（E1.10 可观测内建）——否则 RC4 只能靠日志断言。
8. **facade 集成的 default_system 空串分支**：`internal_get_actor` 在无 default 时用空串哨兵（system.rs:298 现状）——远程分支必须放它**之前**，否则 parrot:// 路径在无引擎注册时静默 miss。
9. **STOP 的回程**：P1 的 stop 发帧后不等对端确认（fire-and-forget）+ 本地 5s 兜底——对端 ingress 的 stop 是异步生效，`is_alive` 可能短暂为 true（弱一致，明示）。
10. **测试里的双 NodeInstance**：同进程双 RemoteActorSystem 各持独立 CallbackRegistry/NodeTable——**共享 CodecRegistry 全局单例**（这是设计行为：TYPE_KEY 全进程唯一）。
11. **tracing 初始化**：测试 binary 里手动 `tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env())`——别依赖外部初始化（bench/测试都可能独立跑）。
12. **golden vectors 的 payload 语义**：向量里 payload 是任意字节（不必是合法消息编码）——frame 层不解析 payload（分层：帧管字节，codec 管语义）。

---

## 9. 交付顺序与提交粒度

```
PR-1: T1+T2（frame/handshake/transport + 全部单测）        —— 独立可合（纯增量）
PR-2: T3（codec/registry/derive 宏 + parrot-api 增量）      —— 独立可合
PR-3: T4+T5（ref/ingress/registry 回调表）                 —— 依赖 PR-1/2
PR-4: T6（system/node/lib + facade 集成 + parrot 增量）     —— 依赖 PR-3
PR-5: T7+T8（语义复跑 + bench + vectors 落库 + DoD 收尾）  —— 依赖 PR-4
```

每 PR 携带：代码 + 该 T 测试义务全绿 + `//! 职责：` 头注释（E5.4）+ 锚点表更新（若 master 演进）。
