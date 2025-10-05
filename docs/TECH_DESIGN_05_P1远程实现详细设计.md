# 05 · P1 远程模式实现详细设计（可直接编码级）

> 状态：**实现设计（编码输入）** · 2026-10-02 · **2026-10-04 全面对码修订**（锚点按最新 master+cluster 合并分支重核；帧布局的 24B 表述已由 [07 §2.1](./TECH_DESIGN_07_异构联邦协议设计.md) 裁定为 28B body 头（X1，POC 四语言实证布局：hop 字段启用 reserved 前 2 字节）；握手体由 bincode 改为自描述 TLV（X3）；REPLY_ERR 载荷由 bincode RemoteError 改为结构化错误码（X4，13 项错误表见 07 §2.5）。**凡本文与 07 §2 冲突处，以 07 为准。**）
> 上游：[04 · 远程与集群架构](./TECH_DESIGN_04_远程与集群架构.md) §13 P1 · 实证参照：`poc/Mx-poc/`（RC1-RC8 语义已在 POC 15 测试中预先验证）
> 交付物：`parrot-remote` crate（workspace 第 5 成员）+ `parrot-api` 最小增量 + `bench/remote-bench`
> **实现规约**：全文受 [07 §14.6 E5 工业级实现规约](./TECH_DESIGN_07_异构联邦协议设计.md)上位约束——依赖选型铁律（E5.1）/高内聚低耦合（E5.2）/机制策略分离（E5.3）/结构化模块化（E5.4）/三态可读可排错（E5.5）。任何编码实现必须逐条对照。
> 代码锚点（行号以 2026-10-04 工作区为准，commit `9e35fc0`）：
> `ActorError`（`parrot-api/src/errors.rs:55`，现有 11 变体：InitializationError / MessageHandlingError / Stopped / Timeout / TimeoutDetail / ActorNotFound / InternalError / ProcessMessageError / Other / Panic / ReplyChannelError；SystemError 为独立枚举）· `Message`（`message.rs:173`）· `MessageEnvelope`（`message.rs:527`，字段 id/payload/sender/options/message_type）· `MessageOptions`（`message.rs:506`，`retry_policy:510`）· derive 宏（`parrot-api-derive/src/lib.rs:228`）· thread 注册表（`parrot/src/thread/system.rs:60`）· facade `internal_get_actor`（`parrot/src/system.rs:298`，双引擎遍历——POC p4a 实证远程可达）
> 背压与调度配置现位于引擎侧：`parrot/src/thread/config.rs`（`BackpressureStrategy:84 区`/`mailbox_capacity:181`/`SchedulingMode`，ADR-12/14 的实现载体）

---

## 0. 工作分解（WBS）与文件清单

```
parrot-remote/
├── Cargo.toml
└── src/
    ├── lib.rs              # 公共 API：RemoteActorSystem, RemoteConfig, spawn_remote_* 
    ├── frame.rs            # L0 帧格式：编解码、常量、golden vectors
    ├── transport.rs        # L1 Transport trait + 连接池
    ├── transport/tcp.rs    # TcpTransport（tokio TcpStream + 长度前缀）
    ├── node.rs             # NodeId/NodeTable/静态种子配置
    ├── codec.rs            # Codec trait + bincode/protobuf 双栈 + SchemaRegistry
    ├── envelope.rs         # RemoteEnvelope（线上格式，≠MessageEnvelope）
    ├── ref.rs              # RemoteActorRef（ActorRef trait 实现）
    ├── registry.rs         # 本地回调注册表：correlation_id → oneshot
    ├── ingress.rs          # 入站路由：帧 → 本地 actor / 回调表
    ├── codec_registry.rs   # TypeId → TYPE_KEY 表（懒静态 + 宏自注册）
    ├── system.rs           # RemoteActorSystem 组装与生命周期
    ├── error.rs            # RemoteError → ActorError 映射
    └── tests/              # 单测（回环双 NodeInstance 同进程模拟）
parrot-api-derive/src/remote.rs   # #[derive(RemoteMessage)] 过程宏
bench/remote-bench/               # 双进程基准（对齐 bench/ 目录方法论）
parrot/tests/test_remote_semantics.rs  # RC1–RC8 语义跨网复跑
```

工作量分解（对齐 04 文档 P1 的 3–4 周，单人）：

| 任务 | 内容 | 估算 |
|---|---|---|
| T1 帧+传输 | frame.rs + transport + 连接池 + 回环测试 | 3 天 |
| T2 编解码 | codec.rs + registry + 双栈 + golden vectors | 3 天 |
| T3 RemoteActorRef | ref.rs + registry.rs + ask/tell/stop | 3 天 |
| T4 入站路由 | ingress.rs + facade 三级路由接入 | 2 天 |
| T5 系统组装 | system.rs + RemoteConfig + 生命周期 | 2 天 |
| T6 过程宏 | remote.rs derive + 集成测试 | 2 天 |
| T7 语义复跑 | test_remote_semantics（RC1–RC8 跨网版） | 2 天 |
| T8 基准 | remote-bench + 报告 | 2 天 |
| 缓冲 | 联调/文档/评审修正 | 3 天 |

---

## 1. L0 帧格式（frame.rs）

### 1.1 二进制布局（1.0 正式，与 07 §2.1 逐字段一致）

```
偏移  长度  字段             说明
0     4    frame_len        u32 LE，= 后续 body 总长（上限 MAX_FRAME_LEN=16MiB，握手可协商下调；超限立即断连）
4     1    version          0x01 = 协议 1.0
5     1    frame_type       见 1.2
6     2    flags            u16 LE 位域，见 1.3
8     8    correlation_id   u64 LE（ASK/REPLY/REPLY_ERR 配对；TELL=0 或去重序号）
16    1    hop_count        中继已跳数，起始 0；每经一跳 +1
17    1    hop_limit        上限，默认 8（HELLO 协商）；hop_count≥hop_limit 丢弃回 RouteUnreachable
18    6    reserved         u48：低 32bit = 端到端 seq（TELL 重排序号——
                         per 源节点单调递增、跨路径切换连续，0=不参与
                         重排即旧语义；接收端 seq 流见 06 §TELL 重排网关）；
                         高 16bit 保留 0（非 0 拒帧——未来扩展再分配）
24    4    path_len         u32 LE
28    var  path_bytes       UTF-8 逻辑地址（parrot://node/sys/user/uuid，07 §3.1）
28+p  4    type_key_len     u32 LE
24+p  var  type_key_bytes   UTF-8（"bin:{crate}::{Type}#v{n}" / "pb:{package}.{Message}"）
..    var  payload_bytes   编码后的消息体（或空）

定长头（frame_len→reserved，偏移 0–23）24 字节，其后 path_len(4B) 内联，body 固定开销合计 28 字节（= X1 实证布局，hop 字段占用 v1 草案的 reserved 前 2 字节）。字节序一律 LE。半包返回"需更多数据"不消费缓冲（POC RC1 已锁定该行为）。
```

### 1.2 frame_type 常量

```rust
pub mod frame_type {
    pub const HANDSHAKE: u8       = 0x01; // 双向，见 §1.4
    pub const HANDSHAKE_ACK: u8   = 0x02;
    pub const HEARTBEAT: u8       = 0x03; // 载荷空，仅头部
    pub const HEARTBEAT_ACK: u8   = 0x04;
    pub const ASK: u8             = 0x10; // 请求方期待 REPLY(corr_id 对应)
    pub const REPLY: u8           = 0x11; // payload = Ok 的编码；错误走 REPLY_ERR
    pub const REPLY_ERR: u8       = 0x12; // payload = 结构化错误 [u16 code][u16 rsv][utf-8 detail]（07 §2.5 十三项错误表；本文 §9 的 RemoteError→ActorError 映射策略不变）
    pub const TELL: u8            = 0x13;
    pub const STOP: u8            = 0x14;
    pub const FRAGMENT: u8        = 0x15; // 1.0：大消息分片（07 §2.2）
    pub const SYSTEM_EVENT: u8    = 0x20; // P2 预留（membership），P1 收到即断连报版本错；1.0 扩承载 RouteGossip（07 §5.3）
    pub const RESOLVE_Q: u8       = 0x21; // 1.0/联邦：目录查询（07 §6，P5 实现）
    pub const RESOLVE_R: u8       = 0x22; // 1.0/联邦：目录应答
    pub const INVALIDATE: u8      = 0x23; // 1.0/联邦：缓存失效推送
    pub const ERROR: u8           = 0x7F; // 协议级错误（版本不符/帧损坏）
}
```

### 1.3 flags 位域

```rust
pub mod flags {
    pub const COMPRESSED_ZSTD: u16 = 0b0000_0000_0000_0001; // P2 启用
    pub const TRACING: u16         = 0b0000_0000_0000_0010; // P2 启用（透传 traceparent）
    pub const BATCH: u16           = 0b0000_0000_0000_0100; // P4 批量帧
    pub const URGENT: u16          = 0b0000_0000_0000_1000; // 高优先级帧（传输层优先出队）
}
```

### 1.4 握手序列（连接建立后第一组帧，非 TLS 阶段）

```
A → B: HANDSHAKE {
    version: 0x01,
    payload: HandshakeBody { node_id: String, capabilities: u32 }  // bit0=bincode bit1=protobuf
}
B → A: HANDSHAKE_ACK {
    version: 0x01,
    payload: HandshakeAckBody { chosen_codec: u32, node_id: String }
}
规则：
  - 双方 version 必须相等，否则回 ERROR 帧后断连
  - codec 协商 = capabilities 按位与；结果为 0（无公共栈）则回 ERROR(code=NoCommonCodec) 断连
  - 握手前收任何非 HANDSHAKE 帧 → 断连（协议违规）
  - mTLS：P1 以 tokio-rustls 可选启用（自签起步）；握手帧在 TLS 之上，字段不变
```

`HandshakeBody`/`HandshakeAckBody` 采用**自描述 TLV 编码**（07 §2.4 定稿，X3 裁定）：`[u8 tag][u16 len][bytes]` 序列——tag：node_id/realm/cluster/capabilities/max_frame_len/topology_role/hop_limit。TLV 取代原 bincode 方案的理由：任何语言（含未来 TS/C++ lite 与 JVM/Erlang 网关）零依赖即可解析，消除"P2 网关期换编码"的 wire 变更窗口——**1.0 起握手体冻结为 TLV，不再有第二次变更窗口**。

### 1.5 编码函数签名

```rust
pub struct FrameHeader {
    pub frame_len: u32,
    pub version: u8,
    pub frame_type: u8,
    pub flags: u16,
    pub correlation_id: u64,
    pub path_len: u32,
    pub type_key_len: u32,
}

impl FrameHeader {
    pub const SIZE: usize = 24; // 定长头（frame_len→reserved 段）；加 path_len 共 28B body 固定开销，见 §1.1（07 X1 布局）
    pub fn encode(&self, path: &str, type_key: &str, payload_len: usize, buf: &mut BytesMut);
    pub fn decode(buf: &mut BytesMut) -> Result<(FrameHeader, Bytes), FrameError>; // codec::.Decode 模式
}

pub enum FrameError {
    TooLarge { len: u32 },          // > 16MiB
    VersionMismatch { got: u8 },    // != 0x01
    UnknownFrameType { got: u8 },
    Truncated,                      // frame_len 声明 > 实际可用
    Utf8(std::str::Utf8Error),
}
```

解码用 `tokio_util::codec`：`FrameCodec` 实现 `Encoder<Frame>` / `Decoder`，`decode` 检查 `buf.len() >= 4` 后按 `frame_len` 等整帧到齐再切（防半包）。**golden vectors**：`frame.rs` 内嵌 8 组硬编码字节序列（每种 frame_type 至少一组），单测逐字节断言 + 防手写漂移；TS/C++ lite 未来实现直接引用同组向量（文档 §8 R4 的解法）。

---

## 2. L1 传输层（transport.rs / transport/tcp.rs）

### 2.1 trait 定义

```rust
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// 建立 outbound 连接（内部池化，同 node 复用）
    async fn connect(&self, node: &NodeAddr) -> Result<ConnectionHandle, RemoteError>;
    /// 监听（server 侧）
    async fn listen(&self, bind: SocketAddr) -> Result<(), RemoteError>;
    /// 接受入站连接（spawn 处理循环）
    async fn accept(&self) -> Result<ConnectionHandle, RemoteError>;
    fn scheme(&self) -> &'static str; // "tcp" / "quic"（07 §8.1 统一命名）
}

pub struct NodeAddr {
    pub node_id: String,
    pub addr: SocketAddr,
}
```

### 2.2 TcpTransport 实现要点

- 每节点 1 条逻辑连接（P1；P2 按并发分池）：`Framed<TcpStream, FrameCodec>` + 写侧 `mpsc::channel::<Frame>(1024)` 串行化（多任务共享句柄 → 都推 mpsc → 单写任务 flush）。
- 读循环：`select!` { framed.next() / 心跳定时 / shutdown 信号 }；收到帧交给 `ingress`（§6）。
- 心跳：每 2s 发 HEARTBEAT；5 个丢失（10s）→ 标记 half-open，关闭重连（P1 重连退避 1s/2s/4s/8s 上限 30s + 抖动 ±20%）。
- Nagle 关闭（`set_nodelay(true)`）——LAN µs 级 RTT 的前提。
- 背压：出站 mpsc 满 1024 帧 → `send().await` 挂起（天然反压；URGENT flag 预留高优队列，P1 不实现）。

### 2.3 连接管理（node.rs）

```rust
pub struct NodeTable {
    nodes: RwLock<HashMap<String, NodeState>>, // node_id → state
}
pub struct NodeState {
    pub addr: NodeAddr,
    pub conn: Mutex<Option<Arc<ConnectionHandle>>>,
    pub status: AtomicU8, // 0=Disconnected 1=Connecting 2=Connected
    pub last_seen_ms: AtomicU64,
    pub chosen_codec: AtomicU32, // 握手协商结果
}
```

静态种子（P1 发现机制）：`RemoteConfig.seeds: Vec<NodeAddr>` 启动即连 + 定时重连任务（上述退避）。运行时新增节点 = 显式 API `remote.add_seed(addr)`（P2 由 gossip 自动化）。

---

## 3. 编解码层（codec.rs / codec_registry.rs）

### 3.1 RemoteMessage trait（parrot-api 增量，放 `message.rs` 尾部）

```rust
/// 远程可达消息契约。实现由 #[derive(RemoteMessage)] 生成。
pub trait RemoteMessage: Message + Serialize + DeserializeOwned {
    /// Schema registry 键。格式规范："bin:{crate}::{Type}" / "pb:{package}.{Message}"。
    const TYPE_KEY: &'static str;
}
```

**为什么绑 serde 而不是自定义 encode/decode**（对 04 文档 §3A 的修正，论证）：
1. serde 是 Rust 生态事实标准，bincode/protobuf(rkyv/json) 都有 serde 前端——一个 trait 覆盖双栈，自定义 encode/decode 要写两遍；
2. 宏生成只需引用 serde 而非生成代码体——宏更薄、编译更快；
3. 原来的 `fn encode(&self) -> Bytes` 无法表达"同一类型两种 wire 格式"（bin vs pb），`TYPE_KEY` + registry 分发天然解决。
**代价**：消息类型需 `#[derive(Serialize, Deserialize)]`（与 serde_json 等共享，通常已有）。`ActorResult` 内的错误走 `RemoteError`（§9），不要求 Result 可序列化。

### 3.2 Registry（codec_registry.rs）

```rust
pub struct CodecRegistry {
    by_key: HashMap<&'static str, CodecEntry>,   // TYPE_KEY → entry
    by_type_id: HashMap<TypeId, &'static str>,   // TypeId → TYPE_KEY（发送侧快查）
}
pub struct CodecEntry {
    pub encode: fn(&dyn Any) -> Result<Vec<u8>, RemoteError>,   // downcast 后 bincode::serialize
    pub decode: fn(&[u8]) -> Result<Box<dyn Any + Send>, RemoteError>, // 反序列化后 Box
    pub stack: CodecStack, // Bin | Pb
}
```

- **懒静态单例**：`OnceLock<CodecRegistry>`，`register()` 仅允许在注册前调用（重复 TYPE_KEY panic，测试可开 strict）。
- **宏自注册**：`#[derive(RemoteMessage)]` 生成 `inventory::submit!{ CodecRegistration { type_key, fns } }`；`CodecRegistry::global()` 首次访问时 collect。`inventory` 已是成熟方案（零运行时开销，链接期收集）；备选 `linkme`（无依赖、链接段收集）——**选 inventory**（文档更全），留 `linkme` 为一行切换。
- 查找失败 = `RemoteError::NotRemotable(type_name)` → 调用方立即收到（不静默）。

### 3.3 双栈

```rust
pub enum CodecStack { Bin, Pb }
// bin 栈：bincode::serde::encode_to_vec / decode_from_slice（bincode 2.x，配置 LittleEndian+Varint）
// pb  栈：prost 通过 serde_bridge 或手写 #[derive(Message)]——P1 仅落 bin 栈，
//        pb 栈接口占位（返回 Unimplemented），P2 随 akka 网关一起交付
```

**决策：P1 只交付 bincode 栈**。理由：pb 栈唯一消费者是 akka 网关/边缘 lite（P2/P3）；先交付接口与注册机制，避免 P1 引入 prost 编译链（proto 编译器依赖）拖慢主线。这是范围收窄，不是设计变更——TYPE_KEY 前缀 `bin:`/`pb:` 在 P1 冻结。

### 3.4 跨节点类型一致性（P1 文档化，P2 工具化）

两端 registry 必须含相同 TYPE_KEY 且字段布局一致（bincode 无自描述）。P2 提供 `parrot-remote-cli schema-diff`（把本地 registry dump 成 JSON 比对）。P1 以 golden vectors + 集成测试双端注册同类型来保障。

---

## 4. RemoteEnvelope 与线上语义（envelope.rs）

线上不传 `MessageEnvelope`（含 `Box<dyn Any>` 与 `Option<Box<dyn ActorRef>>`，不可编码）。定义：

```rust
pub struct RemoteEnvelope {
    pub correlation_id: u64,
    pub path: String,           // 目标完整路径
    pub type_key: String,
    pub payload: Vec<u8>,       // 编码后消息体
    pub reply_to: Option<String>, // ask 时的发起方路径（REPLY 回程目标）
}
```

`MessageOptions` 映射：`timeout` → 由发送方 `send_with_timeout` 本地执行（不上线）；`retry_policy` → P1 不上线（tell at-most-once）；`priority` → flags::URGENT（P2）。**信封瘦身原则：线上只传投递必需字段**，与本地 `MessageEnvelope` 的 uuid/envelope id 不对应（ADR-17 教训：不为不消费的字段付带宽））。

---

## 5. RemoteActorRef（ref.rs）—— 位置透明的关键

### 5.1 结构与 ActorRef 实现

```rust
pub struct RemoteActorRef {
    path: String,                     // parrot://node/system/user/uuid
    node_id: String,
    nodes: Arc<NodeTable>,
    registry: Arc<CallbackRegistry>,  // corr_id → oneshot（ask 回包）
    system: Weak<RemoteActorSystem>,
}

#[async_trait]
impl ActorRef for RemoteActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send_with_timeout(msg, None)
    }

    fn send_with_timeout<'a>(&'a self, msg: BoxedMessage, timeout: Option<Duration>)
        -> BoxedFuture<'a, ActorResult<BoxedMessage>>
    {
        Box::pin(async move {
            let (key, payload) = encode_outgoing(&msg)?;          // §3.2 查表；NotRemotable 立即错
            let cid = self.registry.next_cid();                   // AtomicU64 全局单调
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.registry.insert(cid, tx);                        // 超时/断连时清理
            let frame = Frame::ask(cid, &self.path, key, payload, reply_to);
            self.nodes.send(&self.node_id, frame).await?;         // 连接池写侧 mpsc
            match timeout {
                None => rx.await.map_err(|_| ActorError::InternalError("connection lost".into()))?
                          .into_actor_result(),                   // REPLY_ERR → 对端 ActorError
                Some(d) => match tokio::time::timeout(d, rx).await {
                    Ok(res) => res...,
                    Err(_elapsed) => { self.registry.remove(cid);  // 本地放弃（ADR-10 语义）
                                       Err(ActorError::TimeoutDetail(format!("remote ask {} after {:?}", self.path, d))) }
                },
            }
        })
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        // 同上但不注册回调：TELL 帧，mailbox 接收即视为完成（at-most-once）
        // 对端 mailbox 满时的行为 = 对端 BackpressureStrategy（Block 时入站连接反压，见 §6.3）
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        // STOP 帧，不期待应答；对端 ingress 查本地 registry → 转 stop 到本地 ref
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        // 节点 Connected && 无已知死亡记录（弱一致，文档明示）
    }

    fn path(&self) -> String { self.path.clone() }
    fn clone_boxed(&self) -> BoxedActorRef { Box::new(self.clone()) }
    fn as_any(&self) -> &dyn Any { self }
}
```

**关键设计点**：
- `reply_to` 用**发起节点的系统级回程路径**（`parrot://{self_node}/_remote/reply`），不是 actor 路径——ask 发起者可能是任意 task（无 actor 身份）。REPLY 帧到 ingress 后按 corr_id 查回调表（§6.2）。
- 超时后 corr_id 条目删除，**迟到 REPLY 到达时查表 miss → 记 metric + drop**（与本地"调用方放弃"语义一致，ADR-10）。
- panic 安全：`rx.await` 的 Err 分支覆盖连接任务死亡（回调表在连接重建时统一清空重置，见 §6.4）。

### 5.2 编码出口（encode_outgoing）

```rust
fn encode_outgoing(msg: &BoxedMessage) -> Result<(String, Vec<u8>), ActorError> {
    let key = CodecRegistry::global().key_of(msg.type_id())
        .ok_or_else(|| ActorError::MessageHandlingError(format!(
            "message {:?} is not remotable; derive RemoteMessage + register both ends", msg.type_id())))?;
    let entry = CodecRegistry::global().get(key)?;
    let payload = (entry.encode)(msg.as_ref())?;
    Ok((key.to_string(), payload))
}
```

---

## 6. 入站路由（ingress.rs）

### 6.1 处理矩阵

| 收到帧 | 动作 |
|---|---|
| HANDSHAKE(_ACK) | 连接任务内处理（§1.4），不进 ingress |
| HEARTBEAT(_ACK) | 更新 last_seen，丢弃 |
| ASK | 解码 payload → 本地 facade `get_actor(path)` → `local_ref.send_with_timeout(decoded, opts)` → REPLY/REPLY_ERR 回程（reply_to + corr_id） |
| TELL | 同上但 `deliver`；无回程 |
| STOP | `local_ref.stop()`；无回程 |
| REPLY/REPLY_ERR | 查回调表 corr_id → oneshot::send |
| SYSTEM_EVENT | P1：协议违规断连（P2 启用） |
| ERROR | 记日志 + 断连 |

ASK 的 opts：`MessageOptions { timeout: None, ..Default }`（远端处理超时由**发起方**整体超时控制；远端不二次超时——避免双超时竞态，文档明示）。

### 6.2 回调表（registry.rs）

```rust
pub struct CallbackRegistry {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<ReplyPayload>>>,
}
pub enum ReplyPayload { Ok(Bytes), Err(RemoteError) }
// insert/remove/next_cid 均无锁竞争热点（cid 单调，HashMap 短暂持锁）
// 容量上限 65536：超出 = 调用方已泛滥，新 ask 直接拒绝（保护内存）
```

### 6.3 本地 actor 反压传导

ASK/TELL 目标是 thread 引擎且策略为 Block 时，`local_ref.deliver().await` 会挂起 → ingress 任务挂起 → 该连接读循环挂起（单连接串行处理入站帧）→ 对端出站 mpsc 填满 → 对端调用方挂起。**这是把 ADR-12 的背压从进程内贯通到 TCP 的自然链路，零新增机制**。文档必须写明：单连接串行入站是 P1 特性（head-of-line blocking），P2 拆分为"每连接 N 个 ingress worker + 按目标 actor 哈希保序"。

### 6.4 连接生命周期

- 连接断开：回调表中该节点相关 pending 全部以 `RemoteError::ConnectionLost` 失败（防 ask 永久挂起）；NodeTable 状态回 Disconnected；重连任务按退避启动；重连成功重新握手（chosen_codec 重协商）。
- 优雅停机：先停 accept，再逐连接发完出站队列，等 pending drain（上限 5s），关连接。

---

## 7. RemoteActorSystem 组装（system.rs / lib.rs）

### 7.1 配置

```rust
pub struct RemoteConfig {
    pub node_id: String,                      // 必填，进程唯一
    pub bind: Option<SocketAddr>,             // None = 纯客户端
    pub seeds: Vec<NodeAddr>,                 // 启动即连
    pub transport: TransportKind,             // Tcp（P1）/ Quic（P2）
    pub heartbeat: HeartbeatConfig,           // interval 2s / loss_limit 5
    pub codec_whitelist: Option<Vec<&'static str>>, // 调试用：限制 TYPE_KEY
    pub max_pending_asks: usize,              // 默认 65536
}
```

### 7.2 公共 API（对齐 ParrotActorSystem 门面风格）

```rust
pub struct RemoteActorSystem { /* nodes, registry, ingress dispatcher, local facade Weak */ }

impl RemoteActorSystem {
    /// 启动：listen（若配置）+ 连接种子 + 心跳/重连任务
    pub async fn start(config: RemoteConfig, local: &ParrotActorSystem) -> Result<Arc<Self>, RemoteError>;

    /// 解析路径拿远程 ref（三级路由的远程出口；本地命中由 facade 处理，见 §8）
    pub fn remote_ref(&self, path: &str) -> Result<RemoteActorRef, RemoteError>; // 校验 parrot:// 前缀与 node 在表

    // 远程 spawn：P1 不做（范围红线）——远端 actor 由远端进程自行 spawn，本地仅持有路径。
    // P2 经 K0 管理协议提供：SYSTEM_EVENT(0x20) + AdminCommand::SpawnLocal/PropsRef 注册表（06 §P2.6 设计 / DEV_02 §0 实现）。
    pub async fn shutdown(&self) -> Result<(), RemoteError>;
}
```

**范围红线（P1）**：无远程 spawn、无 gossip、无 pb 栈、无压缩/trace。以上均有接口位但不实现——防止范围蔓延（04 文档 R8）。

### 7.3 与 ParrotActorSystem 集成（facade 三级路由落地）

`parrot/src/system.rs` 的 `internal_get_actor` 升级（保持现有签名与行为兜底）：

```rust
// 伪码：在现有"默认系统 → 遍历"之前插入两级
if let Some(remote) = self.remote.as_ref() {                    // Option<Arc<RemoteActorSystem>>，新增字段
    if path.starts_with("parrot://") {
        if let Some(r) = remote.lookup(path) { return Some(r) } // ① 前缀命中远程表
    }
    if let Some(r) = remote.receptionist_lookup_local(path) { return Some(r) } // P2 占位，P1 返回 None
}
// ② 本地默认系统（现状） → ③ 遍历 fallback（现状保留，日志降级 deprecated）
```

`ParrotActorSystem::register_remote_system(remote: Arc<RemoteActorSystem>)` 新增方法（与 register_thread/actix_system 同族）。

---

## 8. 过程宏（parrot-api-derive/src/remote.rs）

```rust
#[proc_macro_derive(RemoteMessage, attributes(remote))]
pub fn derive_remote_message(input: TokenStream) -> TokenStream {
    // 要求：输入类型已 derive(Serialize, Deserialize)（编译期检测不到则生成编译错误指引）
    // 生成：
    //   impl parrot_api::message::RemoteMessage for T {
    //       const TYPE_KEY: &'static str = "bin:{crate_name}::{Type}";
    //   }
    //   inventory::submit! { CodecRegistration {
    //       type_key: <T as RemoteMessage>::TYPE_KEY,
    //       type_id: TypeId::of::<T>(),
    //       encode: |any| bincode::serde::encode_to_vec(any.downcast_ref::<T>().unwrap(), CFG),
    //       decode: |b| Ok(Box::new(bincode::serde::decode_from_slice::<T>(b, CFG)?.0)),
    //   } }
    // 属性覆盖：#[remote(key = "pb:custom.Key")] 支持显式键（跨 crate 重命名安全）
}
```

依赖增量：`parrot-api-derive` 加 `inventory`；`parrot-api` 的 `message.rs` 加 trait 定义（§3.1）与 `pub use inventory` 转出口。宏错误信息要给出修复指引（缺 serde derive 时提示精确行号补法）——降低采纳摩擦是特性成败关键。

---

## 9. 错误模型（error.rs）

`RemoteError`（parrot-remote 内部）→ 映射 `ActorError`（不新增变体的映射策略论证：`ActorError` 是公共 API，加变体是 breaking；现有 11 变体语义足够覆盖）：

| RemoteError | → ActorError | 说明 |
|---|---|---|
| NotRemotable(type_name) | MessageHandlingError("not remotable: {t}") | 调用方编码期可避免 |
| ConnectionLost(node) | InternalError("remote connection lost: {node}") | 区分于 Timeout |
| NodeUnknown(node) | ActorNotFound(node) | 路径解析失败 |
| CodecError(String) | MessageHandlingError(...) | 编解码失败（类型不一致的典型症状） |
| ProtocolViolation(String) | InternalError(...) | 帧违规（日志 + 断连） |
| NoCommonCodec | InternalError(...) | 握手失败 |
| QueueFull | InternalError("remote outbound queue full") | 出站背压显性化 |

REPLY_ERR 帧载荷 = **结构化错误体 `[u16 code][u16 rsv][utf-8 detail]`**（07 §2.5 十三项错误码表，X4 裁定；不再是 bincode 编码的 RemoteError）。远端本地 ask 的 `ActorError` 按映射表转成标准 code + detail 字符串承载——`ActorError` 本身不跨线，`Other(anyhow)` 不可编码故收敛为 detail 文本。上表的 `RemoteError→ActorError` 映射是**接收侧**策略：收到 REPLY_ERR 后按 code 反查表生成调用方本地的 `ActorError`。

---

## 10. 测试计划（T7 对应）

### 10.1 单测（parrot-remote/tests/）

1. `frame_roundtrip`：全部 frame_type 编码→解码黄金断言（含 golden vectors 逐字节）
2. `codec_registry`：注册/查找/重复键 panic/未知类型 NotRemotable
3. `handshake_negotiation`：版本错/无公共栈/正常协商三路径
4. `heartbeat_timeout`：注入丢帧（mock transport）验证 half-open 判定

### 10.2 回环集成（同进程双 NodeInstance + 内存 transport）

`MemoryTransport`（`tokio::io::duplex` 双管道）跑 §10.3 全部语义用例——CI 无端口依赖、确定性调度。

### 10.3 语义等价复跑（test_remote_semantics.rs，RC1–RC8 跨网版）

| 用例 | 断言 |
|---|---|
| RC1 恰好一次 | N 并发 ask 跨网，回复数 == N，无重复 corr_id 命中 |
| RC2 FIFO | 同发送者连续 TELL 跨网到同一 actor，处理序保序（单连接串行入站保证） |
| RC3 回复路由 | 双 actor 交叉 ask，回复 corr_id 无串扰 |
| RC4 超时语义 | ask 1ms 超时 + 对端慢处理 500ms → 调用方 Timeout；对端实际处理完成；迟到回复 drop + metric |
| RC5 死信 | stop 后 send → REPLY_ERR(Stopped 语义) 跨网等价 |
| RC6 NotRemotable | 未注册类型 ask → 立即错误（不发帧，本地失败） |
| RC7 断连恢复 | 传输中断期间 ask → ConnectionLost；重连后恢复（cid 不复用错乱） |
| RC8 反压贯通 | 对端 Block 策略 + 慢消费者 → 发送方 deliver 挂起（挂起时间 > 人为延迟验证） |

### 10.4 性能基准（bench/remote-bench，T8）

- 双进程（真实 TCP 回环 127.0.0.1）：seq-ask / conc-ask c8/c64 / tell-throughput / pingpong（对齐 engine_stress 场景名与统计口径——同 Report 结构，engine 列 "remote-tcp"）
- 预算门禁（04 §12）：LAN ask RTT <150µs（p50）、tell <60µs；超标 = 阻塞发布
- 与本地 thread↔actix 基线对比表自动生成（remote 税 = RTT - 本地基线）

---

## 11. Cargo 与依赖增量

```toml
# parrot-remote/Cargo.toml
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
# uuid 不引入（DEV_01 §2 裁定：cid 用 u64，调试可读性靠 PARROT_TRACE 帧摘要）
futures = "0.3"
# P1 可选：tokio-rustls + rustls-pemfile（mTLS 自签起步）

# workspace Cargo.toml members 增 "parrot-remote"
# parrot-api-derive 增 inventory = "0.3"
```

---

## 12. 交付判据（DoD）

1. `cargo test --workspace --release` 全绿（当前基线 632，2026-10-04 核 + parrot-remote 新增）
2. RC1–RC8 全绿（MemoryTransport 与真实 TCP 双跑）
3. remote-bench 达预算门禁（ask p50 <150µs / tell <60µs，127.0.0.1）
4. golden vectors 冻结入库（后续 TS/C++ lite 直接引用）
5. 文档：README（快速上手 10 行内跑通双节点 ask）+ 本文档修订版
6. 04 文档 §13 P1 行勾选 + ADR-18 状态草案→已实施(P1 部分)
