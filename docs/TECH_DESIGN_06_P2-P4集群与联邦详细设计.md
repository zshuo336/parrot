# 06 · P2–P4 集群与联邦实现详细设计（可直接编码级）

> 状态：**实现设计（编码输入）** · 2026-10-02 · **2026-10-04 修订注**：本文全部内容有效；联邦扩展（跨集群 Directory/RESOLVE 帧/拓扑三模式/中继降级）见 [07](./TECH_DESIGN_07_异构联邦协议设计.md) §5/§6（P5/P6 阶段）；erlang 网关已由 POC 实证（`poc/remote-poc/erlang-gw/`，与 akka 网关同族），P3 正式化；MQTT 桥接（ParrotMqttBridge，07 §4.5）与 LiveKit 双平面桥（07 §8.3）列入 P3/P5；SYSTEM_EVENT 帧在 07 §2.2 扩展承载 RouteGossip。**帧布局/握手/错误体一律以 07 §2 为准**（本文 §3.1.3 的"定长头 24B 手写"指定长头 frame_len→reserved 段，加 path_len 共 28B body 固定开销；TS lite 的 DataView 解析按 07 §2.1 布局实现）。背压与调度配置的代码锚点：`parrot/src/thread/config.rs`（ADR-12/14 实现载体）。
> **实现规约**：全文受 [07 §14.6 E5 工业级实现规约](./TECH_DESIGN_07_异构联邦协议设计.md)上位约束（依赖选型/内聚耦合/机制策略分离/结构化/三态可读）——任何编码实现必须逐条对照。
> 上游：[04 架构](./TECH_DESIGN_04_远程与集群架构.md) §13 P2–P4 · 前置：[05 P1 远程](./TECH_DESIGN_05_P1远程实现详细设计.md)
> 本文按交付顺序编排：P2 集群核心（membership/receptionist/QUIC/mTLS/akka 网关）→ P3 边缘（lite/durable/ray）→ P4 深化（sharding/singleton/批量帧）
> 粒度与 05 对齐：签名级 + 状态机 + 线上格式 + 测试矩阵。P2 起的设计在 P1 落地后可能微调，标注 ⚠ 复核点。


---

# 第一部分 · P2 集群核心

## P2.0 工作分解

| 任务 | 内容 | 估算 |
|---|---|---|
| K1 SWIM membership | 状态机 + gossip 往返 + 故障检测 | 5 天 |
| K2 Receptionist | 本地表 + 事件流 + gossip 携带 | 3 天 |
| K3 QUIC transport | quinn 实现 Transport trait | 3 天 |
| K4 mTLS | 证书加载/校验/轮换钩子 | 2 天 |
| K5 akka 网关(JVM) | BridgeActor + 协议编解码 + golden vectors | 8 天（Scala/Java） |
| K0 远程 spawn 管理协议 | SYSTEM_EVENT(0x20) + AdminCommand::SpawnLocal/AdminStop + PropsRef 注册表（§P2.6） | 2 天 |
| K6 集成语义复跑 | RC + membership 故障注入用例 | 3 天 |
| 缓冲 | 联调（双语言三组件） | 3 天 |

## P2.1 SWIM Membership（K1）

### 2.1.1 为什么 SWIM（论证）

| 备选 | 检测延迟 | 带宽 | 脑裂处理 | 判定 |
|---|---|---|---|---|
| 全互 ping | O(n) 连接 | 高 | 无内建 | 否 |
| 心跳中心化 | 快 | 低 | 中心单点 | 否 |
| **SWIM** | O(log n) 轮 | 每轮每成员 O(1) 消息 | 感染态协议天然最终一致 | **采用** |
| 外部 etcd | 快 | 低 | 依赖部署 | 云内可选，不进核心（04 §7.1） |

对标先例：akka cluster（改进 SWIM）、HashiCorp memberlist（go SWIM 库，生产验证）。我们实现 SWIM 的保守子集 + memberlist 的两个实用改进（间接探活 + 感染式状态传播），**不实现** memberlist 的 push/pull 全量同步（n≤100 节点周期 gossip 足够；⚠ 复核点：节点数 >200 时补 push/pull）。

### 2.1.2 成员状态机

```
Alive ──(探活失败×probe_multiplier)──▶ Suspect ──(suspect_timeout)──▶ Dead
  ▲                                      │
  └────────(refute：本人收到 Suspect 自己)─┘        Dead 节点保留 TTL 后移出表
```

```rust
pub enum MemberStatus { Alive, Suspect, Dead }
pub struct Member {
    pub node_id: String,
    pub addr: NodeAddr,
    pub status: MemberStatus,
    pub incarnation: u64,        // refute 计数：收到"怀疑自己"时 +1 广播 Alive
    pub status_until_ms: u64,    // Suspect 截止；Alive/Dead 为清理时间
    pub metadata: Bytes,         // P2：codec 能力位；P3：设备能力标签
}
```

### 2.1.3 协议参数（默认值，全部可配）

```rust
pub struct SwimConfig {
    pub probe_interval: Duration,      // 500ms（每轮随机选一个目标直接 ping）
    pub probe_timeout: Duration,       // 500ms
    pub indirect_probes: usize,        // 3（超时则请 k 个随机成员间接探活）
    pub suspect_timeout: Duration,     // 3s（被 Suspect 后未 refute 即 Dead）
    pub gossip_interval: Duration,     // 200ms（每轮向 fanout=3 随机成员发增量）
    pub dead_retention: Duration,      // 24h
}
```

### 2.1.4 wire（复用 SYSTEM_EVENT 帧，P1 预留位启用）

```
SYSTEM_EVENT payload（bincode）:
  MembershipGossip { events: Vec<MemberEvent>, seen_from: NodeId }
  MemberEvent = MemberUpsert(Member) | MemberRemove(String)
规则：
  - 状态变更（含 incarnation 变化）才产生事件；收到更新事件按 (incarnation, status优先级 Dead>Suspect>Alive) 偏序合并
  - Suspect 自己 → incarnation += 1 广播 Alive（refute）
  - gossip 携带全量 member 表压缩摘要（u64 xor 集合指纹）做对账，漂移则触发定向全量
```

### 2.1.5 检测与收敛指标（测试门禁）

- 单节点 kill -9：**≤3.5s** 内集群多数派标记 Suspect→Dead（probe 0.5s×3 间接 + suspect 3s 内）
- 网络分区恢复：gossip 自动收敛，无人工介入
- 脑裂文档化：SWIM 是可用性优先（AP）；集群级互斥操作（singleton，P4）用多数派租约，不依赖 membership 强一致

## P2.2 Receptionist（K2）

### 2.2.1 API（落 parrot-api context，双引擎 context 转发实现）

```rust
// ActorContext trait 增量（context.rs）
fn receptionist_register(&mut self, key: ReceptionistKey);
fn receptionist_deregister(&mut self, key: &ReceptionistKey);
fn receptionist_subscribe<'a>(&'a mut self, key: ReceptionistKey)
    -> BoxedFuture<'a, ActorResult<ReceptionistStream>>;

pub struct ReceptionistKey(String);         // 命名空间规范："edge/rpa"、"cloud/orchestrator"
pub struct ReceptionistStream { mpsc: Receiver<ReceptionistEvent> }
pub enum ReceptionistEvent {
    Registered { key: ReceptionistKey, remote_path: String },   // 上线（路径即 RemoteActorRef 材料）
    Unregistered { key: ReceptionistKey, remote_path: String }, // 下线（含节点 Dead 触发）
}
```

### 2.2.2 实现结构

```rust
pub struct Receptionist {
    local: RwLock<HashMap<ReceptionistKey, Vec<String>>>,     // 本节点注册（actor 路径）
    subscribers: RwLock<HashMap<ReceptionistKey, Vec<mpsc::Sender<ReceptionistEvent>>>>,
}
```

- 注册/注销：本地表写 + 产生 `SystemEvent::ReceptionistSync` 进 gossip 增量。
- 订阅：先回放当前快照（全集群该 key 的注册项）再续事件流（防窗口丢失；快照与事件流之间的重复由订阅端按 `remote_path` 去重）。
- 节点 Dead：该节点全部注册项批量 Unregistered 事件（membership 回调驱动）。
- **云侧订阅端拿到 remote_path 后经 facade 三级路由 ① 构造 RemoteActorRef**——发现与路由无缝衔接（05 §7.3）。

### 2.2.3 用例（端云核心场景验收）

```rust
// 云编排 actor 启动时：
ctx.receptionist_subscribe("edge/rpa".into()).await;
// 事件流里 Registered { remote_path: "parrot://phone-42/lite/user/rpa-main" }
// → parrot.get_actor(&ActorPath::placeholder(&remote_path)) → RemoteActorRef → ask/tell 下发
```

## P2.3 QUIC Transport（K3）

```rust
pub struct QuicTransport { /* quinn::Endpoint, alpn=["parrot/1"], 0-RTT 配置 */ }
#[async_trait]
impl Transport for QuicTransport {
    // connect: quinn Endpoint::connect(addr, "parrot/1")；每 (node) 一个 Connection，
    //          每个 ask 一个 uni 流（天然无 HOL；对齐 04 §5 决策）
    // listen: Endpoint::listen + accept bi 流作控制面（握手/心跳/SYSTEM_EVENT），
    //         uni 流作数据面（ASK/TELL/REPLY——帧格式不变，流内仍是 05 §1 编码）
}
```

- 帧编解码完全复用（流内 = 长度前缀帧序列），**Transport 实现差异只在连接/流管理**——这是 05 把帧与传输分层设计在此兑现。
- 连接迁移（端侧 IP 切换不断流）：quinn 默认支持（CID 会话），endpoint 侧无需代码，集成测试验证即可。
- QUIC 为边缘默认（04 决策）；云内 parrot 节点间 P2 起也允许 QUIC（配置项，默认 TCP——云内延迟最优）。

## P2.4 mTLS（K4）

- tokio-rustls（TCP）/ quinn rustls 集成（QUIC）统一证书加载：`RemoteConfig.tls: Option<TlsConfig { cert_path, key_path, ca_path }>`。
- 握手帧字段不变（TLS 层之下已认证）；node_id 必须匹配证书 CN/SAN，不匹配断连（防身份伪造）。
- 端侧（P3）：设备首注册流程签发短期证书（云端 CA，简单 CSR over management API）+ 到期轮换（⚠ 复核点：轮换窗口的双证书容忍）。
- P2 交付内建自签工具 `parrot-remote-cli cert gen`（开发环境 10 分钟可用安全）。

## P2.5 akka 网关（K5，JVM 侧）

### 2.5.1 部署形态与组件

```
JVM 进程（与存量 akka 系统同 JVM，extension 加载）：
  parrot-protocol-jvm/
  ├── ParrotTransportExtension.scala   # Netty TCP server，说 05 §1 帧（含 golden vectors 单测）
  ├── BridgeActor.scala                # akka typed actor：AskFrame→AskPattern→ReplyFrame
  ├── ReceptionistBridge.scala         # akka receptionist key ↔ parrot ReceptionistEvent 双向
  └── Codec.scala                      # pb 栈（prost 生成的 JVM 对应：protobuf-java）
```

### 2.5.2 消息桥接规则

| parrot 帧 | akka 动作 |
|---|---|
| ASK(pb 栈) | `AskPattern.ask(ref, msg, timeout=整体剩余时间)` → REPLY / REPLY_ERR(akka Status.Failure → RemoteActorError) |
| TELL | `ref ! msg`（at-most-once 对齐） |
| STOP | 映射受限：akka 无外部 stop 语义 → REPLY_ERR(UnsupportedOperation)，文档明示（**唯一语义缺口**） |
| REPLY | 对 akka 发起的 ask 回程（BridgeActor 记录 cid→CompletableFuture） |

- 路径映射：`parrot://akka-gw-1/jvm/user/{akkaPath}` ↔ 内部查 `ActorSystem.actorFor/selection`（typed 用 receptionist 注册表优先）。
- **pb 栈在 parrot 侧同步交付**（`pb:` TYPE_KEY + prost；.proto 文件由消息 crate 的 build.rs 生成或手维护——⚠ 复核点：derive 宏 + prost 双轨的编译链复杂度，P2 初期手维护 .proto，量少）。
- golden vectors：JVM 侧单测逐字节对齐 Rust vectors（互操作 CI：GitHub Actions matrix 跑双端一致性）。

### 2.5.3 验收

- parrot actor ask akka actor（echo/cpu 两场景）RTT <300µs（含网关一跳，同机回环）
- akka actor 死亡 → parrot 侧 is_alive=false + receptionist Unregistered 事件
- 协议一致性 CI（golden vectors 双语言矩阵绿）

---

## P2.6 远程 spawn 管理协议（K0）

> 定位：**管理面**机制，非数据面。解决"远端 actor 由谁 spawn"——05 §7.2 明示 P1 红线（不做远程 spawn），本节为其 P2 承接。与 P4 sharding 的自动放置（数据面按 affinity_key 哈希）是**两条独立通道**：K0 是显式管理指令（人工/编排系统发起，运维/编排动作，不进 ASK/TELL 数据面帧），sharding 是隐式自动放置。实现锚点：`parrot-remote/src/admin.rs`（DEV_02 §0）。

### 2.6.1 协议承载与语义模型

复用 `SYSTEM_EVENT (0x20)` 帧类型，payload 为 `AdminCommand`（bincode 栈，仅 Rust↔Rust 管理通道；SYSTEM_EVENT 第三载荷形态，前两形态：MembershipGossip / ReceptionistSync）：

```rust
// parrot-remote/src/admin.rs（DEV_02 §0 权威实现锚点）
pub enum AdminCommand {
    /// 目标节点收到后在本节点执行 spawn（target_node=自己；非自己则拒绝）
    SpawnLocal { req_id: u64, props: PropsRef, path: String, reply_to: String },
    /// 管理性远程 stop（比数据面 STOP 帧多回执；日常停 actor 用 STOP 即可）
    AdminStop  { req_id: u64, target_path: String, reply_to: String },
}
pub enum AdminReply {
    Spawned { req_id: u64, path: String },
    Failed  { req_id: u64, code: ErrCode, detail: String },
    Stopped { req_id: u64 },
}
```

- 公共 API：`RemoteActorSystem::spawn_named(node, path, props)` / `admin_stop(path)`（DEV_02 §0.3）。
- `PropsRef` 是**注册表键**（字符串），不是序列化 Props——"props 不跨线"。目标节点凭 name 查本地 `PropsFactory` inventory 注册表（`inventory::collect!` 编译期登记，工厂闭包与引擎选择权都在目标节点本地），与 04 §8.1 网关"协议不是指针"同族原理。未注册名 → `Failed(NotRemotable, "props '{name}' not registered")` 快速失败。
- 回执走 `AdminReply`（req_id 配对，非数据面 REPLY 帧）。
- 安全：AdminCommand 仅走 mTLS 通道（P2.4 之后强制）且证书 role=admin 校验（运维面权限域，与 receptionist ACL 数据面权限不同轨），集群内 realm 可发。

### 2.6.2 语义边界

| 关注点 | 范围 |
|---|---|
| 幂等 | SpawnLocal 按 path 幂等：同 path 已存在 → `Failed(ProtocolViolation, "path exists")`（不静默复用） |
| 失败语义 | at-most-once 次指令（不自动重试）；发起方决定重发 |
| 与 sharding 关系 | K0 显式 spawn（指定节点）；P4 sharding 自动放置（路由层加哈希环，`spawn_named` API 签名不变）。两者不共享状态，sharding 不感知 K0 spawn 的 actor |
| 与 07 关系 | 帧类型 SYSTEM_EVENT(0x20) 是 07 §2.2 预留位，本协议是其在管理面的首个实例化（07 §5 RouteGossip 为第二实例） |

### 2.6.3 验收

- 双节点：A `spawn_named("edge-7", "/user/crawler-1", "crawler.v2")` → B 侧 actor 就位 → 返回 RemoteActorRef → ask 即达（RTT 与普通远程同量级）
- 未注册 props 名 → `Failed(NotRemotable)` 快速失败，零副作用
- 同 path 重发 → `Failed(ProtocolViolation, "path exists")`
- 非 admin 证书连接发 AdminCommand → REPLY_ERR(Forbidden)
- 管理性 stop 回执到达且目标 actor 已 stop

---

# 第二部分 · P3 边缘与 ray

## P3.0 工作分解

| 任务 | 内容 | 估算 |
|---|---|---|
| E1 parrot-lite TS | 协议子集 + QUIC(WebTransport)/TCP + receptionist 注册 | 6 天 |
| E2 durable tell | 云 proxy WAL + 续传 | 4 天 |
| E3 反压贯通 | proxy 有界邮箱 + 端侧慢消费 | 2 天 |
| E4 ray adapter | Python gateway + dispatcher actor | 6 天 |
| E5 ACL | receptionist key 命名空间权限 | 2 天 |
| E6 erlang 网关 | BridgeActor + 分布式 注册代理（POC 转正） | 3 天 |
| E7 MQTT 桥 | ParrotMqttBridge（QoS1 ↔ durable tell，07 §4.5） | 5 天 |

## P3.1 parrot-lite（TS）（E1）

### 3.1.1 范围（协议子集，对应 05 帧表）

实现帧：HANDSHAKE(_ACK)/HEARTBEAT(_ACK)/ASK/REPLY(_ERR)/TELL。**不实现**：STOP、SYSTEM_EVENT（边缘不进 membership 表决，静态连云端接入点）。

### 3.1.2 TS API 形态

```typescript
// 手机 RPA App 内嵌（React Native/WebView 均可）
import { ParrotLite } from "@parrot/lite";
const node = await ParrotLite.connect({
  url: "quic://gateway.example.com:443",     // WebTransport；降级 "tcp://"
  nodeId: "phone-" + deviceId,
  tls: { /* 客户端证书（设备注册签发） */ },
});
const rpa = node.spawn("rpa-main", {
  onAsk: async (msg) => handler(msg),        // 云端 ask 端侧
  onTell: (msg) => queue.push(msg),          // 云端 tell 指令
});
await node.receptionist.register("edge/rpa", "rpa-main", { capabilities: ["screen", "tap"] });
const reply = await node.ask("parrot://cloud-1/thread-main/user/orch-1", RpaReport, data);
```

### 3.1.3 实现要点

- 帧解析：DataView 定长头 24B 手写（frame_len→reserved 段；加 path_len 共 28B body 固定开销——07 §2.1 布局，hop_count/hop_limit 在偏移 16/17；golden vectors 直接移植为 jest 断言）；bin 栈 P3 不做——**边缘一律 pb 栈**（跨语言一致性，且 TS 无 bincode）。
- QUIC：浏览器/WebView 用 WebTransport API；Node 用 `@fails-components/webtransport`；不可用环境自动降级 TCP/TLS（WebSocket+二进制帧兜底——⚠ 复核点：WS 帧封装开销 2-6B，可接受）。
- 断线：指数退避重连（1s→30s）；重连成功重新 handshake + receptionist 重注册（幂等：云端按 (node_id,key,path) upsert）。
- 体积预算：dist <50KB（min+gz），零原生依赖（纯 TS）。

## P3.2 durable tell（E2）

### 3.2.1 云 proxy actor（端侧的云端代理）

```
CloudProxy（每端一个，thread 引擎，Sharded{affinity_key: node_id}——ADR-14 直接复用）：
  - 持有到端侧的 RemoteActorRef + 有界邮箱 mailbox_capacity=1024（Block 策略）
  - 下行（云→端）tell 入 proxy → 写 WAL(组提交, fsync_interval=10ms 可配) → 转发
  - 端侧 ACK（P3 扩展帧 flag ACK 位，TELL 置位时端侧处理完成回 HEARTBEAT 变体）→ WAL 截断
  - 断线：WAL 保留；重连后按序重放（端侧按 (sender_path, seq) 去重 → 业务幂等层）
```

### 3.2.2 语义与验收

- at-least-once（WAL 重放 + 去重 → 业务效果 exactly-once）
- 验收：端侧断网 5min，云端持续 tell 100 条 → 重连后 100 条全达、零丢失、零重复处理（端侧去重断言）
- ⚠ 复核点：WAL 存储引擎——P3 用 sled？不，**直接文件追加 + 简单索引**（引入嵌入式 DB 的维护成本 > 收益；单端 WAL 体积小、截断频繁）

## P3.3 反压贯通（E3）

端侧慢消费（RPA 任务执行中）→ 端侧读循环挂起 → QUIC 流窗口收紧 → 云 proxy 出站挂起 → proxy 邮箱(1024) 满 → 云侧发往该端的 tell 在 proxy.deliver 挂起 → **云编排 actor 的邮箱进而堆积触发其 Block/Error 策略**。链路验证用例：端侧人为 sleep 5s，云端 2000 tell，断言云编排 actor 收到 backpressure 错误（Error 策略时）或最终全达（Block 策略，端侧唤醒后）。

## P3.4 ray adapter（E4）

### 3.4.1 形态（Python gateway worker）

```python
# ray_gateway.py（部署在 ray head 节点侧，与 parrot 云中枢 remote 互联）
import ray, parrot_protocol_py  # 帧编解码（golden vectors 对齐）
@ray.remote
class ParrotDispatcher:
    def on_message(self, type_key: str, payload: bytes, mode: str) -> bytes:
        handler = HANDLERS[type_key]           # 业务注册：TYPE_KEY → 方法分发
        return handler(payload)                # pb 反序列化→调用→pb 序列化
# gateway 主循环：parrot ASK → dispatcher.on_message.remote(...) → ray.get → REPLY
```

### 3.4.2 语义映射表（04 §8.2 的编码级细化）

| parrot | ray | 备注 |
|---|---|---|
| ask(Msg) | `disp.on_message.remote(key, pb, "ask")` + `ray.get` | 超时由发起方控制（ray.get timeout 对齐剩余预算） |
| deliver(Msg) | `disp.on_message.remote(key, pb, "tell")` 不 get | ray 任务仍会执行（非取消语义，文档明示与本地 deliver 的差异） |
| 路径寻址 | gateway 维护 `parrot://…/ray/{name}` ↔ ActorHandle 表 | spawn 经 gateway 管理 API（`ray.get_actor` 按名发现） |
| 死亡通知 | ray actor `max_restarts` 耗尽 → gateway 转发 receptionist Unregistered | ray 自愈期间 parrot 侧 ask 超时 |

### 3.4.3 验收

- parrot 编排 actor → ray 集群派发 1000 并行计算任务（每任务 pb 消息）→ 结果回聚 parrot actor，总耗时 vs 纯 ray 基线损耗 <15%（网关一跳成本量化）

## P3.5 Receptionist ACL（E5）

- key 命名空间即权限域：`{scope}/{name}`，scope∈{edge, cloud, jvm, ray}
- 云端配置 `acl.yaml`：role（证书 CN 绑定）→ 允许 register/subscribe 的 scope 前缀
- 执行点：proxy/receptionist 拒绝未授权注册（REPLY_ERR(Forbidden)），事件流不下发未授权 key
- P3 简化为静态配置；动态 ACL（actor 化权限管理）P4+ 

---

# 第三部分 · P4 集群深化

## P4.1 Cluster Sharding（复用 ADR-14）

```rust
// shard 决策（云端 cluster 内）：
//   node = hash(affinity_key) % alive_nodes   （一致性哈希环，虚节点 256/node，防雪崩）
// EntityRef = RemoteActorRef 指向 parrot://{node}/{system}/user/entity-{affinity_key}
// 实体激活：首次消息到 shard holder 节点 → 本地 spawn（thread 引擎 Sharded 模式，
//           affinity_key 同时作为线程亲缘键——两级亲和：节点亲和 + 线程亲和）
// rebalance：membership 变更 → 哈希环更新 → 迁移标志节点对未完成消息 drain 后
//           实体 passivate（stop + 状态快照 KV）→ 新 holder 惰性再激活
```

- **不实现**：实体自动状态迁移协议（P4 范围=无状态实体或状态外部化已由业务完成；有状态迁移=akka cluster sharding 的 event-sourced 配合，超出——04 文档 §7.3 已声明）
- 验收：3 节点集群 kill 1 节点 → 5s 内其分片实体在新 holder 重建 → 消息零丢失（receptionist + 重试）

## P4.2 Cluster Singleton

- 租约制：候选节点向多数派（membership Alive 集合）周期续约（lease 10s / renew 3s）
- 持有者 Dead → 租约到期 → 候选序号最高者接管（防双主：接管等待 = lease 全额过期）
- 用途：云 proxy 分配器、全局定时器、ACL 管理者
- 验收：kill singleton 节点 → ≤13s 新 singleton 产生（lease 10s + 确认 3s）

## P4.3 批量帧与 C++ lite

- BATCH flag：单帧载荷 = N×(len+frame)，传感流场景帧头摊薄（1000 msg/s 传感 → 每秒 1 批）
- C++ lite：与 TS 同协议子集；另提供 C ABI（`pl_connect/pl_ask/pl_poll`，04 §9 的进程内路径）——Rust 主控机器人 + C++ 执行器直连
- 验收：传感流 10k msg/s 带宽降 >60%（vs 单帧）

---

# 附录 A · 全阶段测试矩阵汇总

| 层 | 用例组 | 阶段 |
|---|---|---|
| 帧 | golden vectors 逐字节（Rust/TS/JVM/py 四语言） | P1 起，逐阶段扩语言 |
| 传输 | 半包/粘包/超限断连/心跳超时/重连退避 | P1 |
| 语义 | RC1–RC8（MemoryTransport + 真实 TCP 双跑） | P1 |
| 集群 | 成员故障收敛 ≤3.5s / 分区恢复 / refute 风暴 | P2 |
| 联邦 | parrot↔akka RTT/死亡通知/协议 CI 矩阵 | P2 |
| 边缘 | 断网 5min 零丢 / 反压贯通 / 重连幂等 | P3 |
| ray | 1000 任务派发损耗 <15% | P3 |
| 深化 | sharding 迁移零丢 / singleton 接管 ≤13s / 批量带宽 | P4 |

# 附录 B · 风险增量（04 §14 之外的实现级风险）

| # | 风险 | 缓解 |
|---|---|---|
| I1 | bincode 跨版本布局漂移（无自描述） | 锁 bincode 2.x；TYPE_KEY 带布局版本后缀规范（`bin:crate::Type#v2`）；schema-diff 工具 |
| I2 | 单连接串行入站 HOL（P1 特性） | P2 拆 ingress worker 池 + 按目标 actor 哈希保序；文档明示 P1 限制 |
| I3 | inventory 在 no_std/wasm 场景不可用 | lite（TS）不走 Rust registry；Rust 侧 wasm 场景提供手动 register API 兜底 |
| I4 | akka 网关 STOP 语义缺口 | 文档显式 UnsupportedOperation；云→JVM 停止经管理面而非协议 |
| I5 | QUIC 在企业防火墙 UDP 封锁 | 配置降级链 QUIC→TCP 自动探测（连接时双试，P3 落地） |
| I6 | SWIM gossip 风暴（大规模频繁变更） | 事件合并（同成员一窗口一事件）+ 指纹对账定向同步；n>200 复核 push/pull |

# 附录 C · 与 05/04 的追溯表

| 本文条目 | 上游依据 |
|---|---|
| SWIM 参数/状态机 | 04 §7.1 |
| Receptionist API 形态 | 04 §7.2 |
| QUIC 边缘默认/HOL 论证 | 04 §5 |
| durable tell 语义 | 04 §6 |
| ray 定位收窄 | 04 §8.2 |
| akka 网关部署形态 | 04 §8.1 |
| sharding 复用 affinity_key | 04 §7.3 / ADR-14 |
| C++ C ABI 进程内 | 04 §9 |
| 全部 wire 格式 | 05 §1（SYSTEM_EVENT 于 P2 启用） |
