# DEV_02 · P2 集群核心开发文档（SWIM / Receptionist / QUIC / mTLS / akka 网关 / pb 栈）

> 状态：**开发文档（实施合同）** · 2026-10-04 · 基准 commit `9e35fc0` + DEV_01 交付后
> 设计依据：[06 P2-P4 详细设计](./TECH_DESIGN_06_P2-P4集群与联邦详细设计.md) 第一部分（P2.0-P2.5）+ [07 §4](./TECH_DESIGN_07_异构联邦协议设计.md)（传输/帧以 07 为准）
> 前置：DEV_01 全部 DoD 通过（parrot-remote crate 存在且 RC1-RC8 绿）
> 上位约束：[07 §14.6 E5](./TECH_DESIGN_07_异构联邦协议设计.md)；依赖白名单（quinn/rumqttc 不在本阶段——本阶段新增仅 quinn + prost + tokio-rustls，均已在决策档）

---

## 0. 范围与红线

**做**：SWIM membership（K1）+ Receptionist（K2）+ QuicTransport（K3）+ mTLS（K4）+ akka JVM 网关（K5）+ pb 编码栈（随 K5）+ **K0 远程 spawn 管理协议（admin.rs）** + 集成语义复跑（K6）。

**不做（红线）**：
- RouteGossip / Directory / RESOLVE 帧（P5——SYSTEM_EVENT 只承载 MembershipGossip 与 ReceptionistSync 与 AdminCommand）
- digest/push-pull 全量同步（n≤200 全量 gossip；06 I6 ⚠ 转 P6）
- **跨节点自动选址 spawn**（K0 只做"指定节点按名 spawn"；"任意节点自动选址"是 P4 sharding 实体激活语义，依赖 D1 哈希环——勿提前实现）
- sharding/singleton（P4）
- 网关多连接 accept（mesh 模式网关是 P5 项；P2 网关挂 hub/单连接形态——07 §8.2）

**交付物**：parrot-remote 内新模块（swim.rs / receptionist.rs / transport/quic.rs / tls.rs / **admin.rs**）+ `parrot-protocol-jvm/`（独立 Maven 项目）+ parrot-api 的 Receptionist context API + pb 栈（codec.rs 的 Pb 分支实现）+ 集成测试。

---

## 0. K0 · 远程 spawn 管理协议（admin.rs）——DEV_01 红线的承接落点

> 承接关系：DEV_01 §0 红线"远程 spawn → P2 管理协议"与 TECH_DESIGN_05 §7.2"P1 无远程 spawn，P2 经 SYSTEM_EVENT/管理协议提供"——**本节即该 P2 管理协议**（此前 DEV_02 红线误写"P4+"，已修正）。

### 0.1 语义模型

远程 spawn 的本质：**运维/编排动作**（谁在哪个节点放什么 actor），不是消息语义。因此不进 ASK/TELL 数据面帧，走 SYSTEM_EVENT 管理子通道（第三载荷形态）：

```rust
//! 职责：远程 spawn/stop 管理协议——SYSTEM_EVENT 管理载荷 + 目标节点 AdminService。
//! 权限域：运维面（证书 role=admin 校验），与 receptionist ACL（数据面）不同轨。

// SYSTEM_EVENT (0x20) payload 第三形态（前两形态：MembershipGossip / ReceptionistSync）
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

### 0.2 关键设计：PropsRef——"props 不跨线"

actor 本体（闭包/结构体）不可序列化，跨线传的是**构造器注册名**：

```rust
/// 节点启动时注册本地可远程 spawn 的工厂（inventory 自注册——与 RemoteMessage 宏同族机制）
pub struct PropsFactory {
    pub name: &'static str,   // 如 "crawler.v2"
    pub spawn: fn(&mut ContextSeed) -> BoxedFuture<'static, ActorResult<BoxedActorRef>>,
    pub engine: EngineKind,   // 工厂内部指定 thread/actix——发起方无权选引擎
}
inventory::collect!(PropsFactory);

// 发起方：remote.spawn_named(node: "edge-7", path: "/user/crawler-1", props: "crawler.v2").await
// 目标节点：SpawnLocal → inventory 查名 → 本地 spawn → AdminReply::Spawned(path) 回 reply_to
```

**理由**：与 04 §8.1 网关"协议不是指针"同族原理——跨线传"如何构造"的引用而非对象本体。JVM/Erlang 网关同理（props 名映射对端语言构造注册表）。未注册名 → `Failed(NotRemotable, "props '{name}' not registered")` 快速失败。

### 0.3 API（RemoteActorSystem 增量）

```rust
impl RemoteActorSystem {
    /// 按名在指定节点 spawn（集群内指定节点形态；P4 sharding 后自动选址只是路由层加哈希环，本 API 签名不变）
    pub async fn spawn_named(&self, node: &str, path: &str, props: &str) -> Result<RemoteActorRef, RemoteError>;
    /// 管理性 stop（带回执）
    pub async fn admin_stop(&self, path: &str) -> Result<(), RemoteError>;
}
```

### 0.4 K0 测试义务

- `spawn_named_e2e`（目标节点注册工厂 → 发起方 spawn_named → 返回 RemoteActorRef → ask 即达）
- `spawn_named_unknown_props`（未注册名 → Failed(NotRemotable) 快速失败，零副作用）
- `admin_stop_with_receipt`（回执到达且目标 actor 已 stop）
- `spawn_path_conflict`（同路径已存在 → Failed(ProtocolViolation, "path exists") ）
- `admin_requires_role`（非 admin 证书的连接发 AdminCommand → REPLY_ERR(Forbidden)）

---

## 1. 任务分解

```
K0 管理协议（远程 spawn）── 依赖 DEV_01（SYSTEM_EVENT 帧已定义）；最小实现可与 K1 并行
K1 SWIM ── 无依赖（SYSTEM_EVENT 帧已在 DEV_01 定义）
K2 Receptionist ── 依赖 K1（事件搭 gossip 车传播）
K3 QUIC ── 无依赖（Transport trait 已就位）
K4 mTLS ── 依赖 K3（quinn rustls 集成）+ DEV_01 TCP 路径
K5 akka 网关 + pb 栈 ── 依赖 K3/K4（网关用 QUIC/TCP + TLS 接入）——可与 K1/K2 并行
K6 集成复跑 ── 依赖全部
```

估算（06 P2.0 + K0 增补）：K0=2d K1=5d K2=3d K3=3d K4=2d K5=8d(JVM) K6=3d 缓冲 3d。

---

## 2. K1 · SWIM Membership（swim.rs）

### 2.1 数据结构

```rust
//! 职责：集群成员表（成员表即路由表——06 §2.1.4 的同体设计）。

pub enum MemberStatus { Alive, Suspect, Dead }

pub struct Member {
    pub node_id: String,
    pub addr: NodeAddr,
    pub status: MemberStatus,
    pub incarnation: u64,      // refute 计数
    pub status_until_ms: u64,  // Suspect 截止 / Dead 清理时间
    pub metadata: Bytes,       // capabilities 位（握手 §2.4 tag4 同源）
}

pub struct SwimConfig {
    pub probe_interval: Duration,   // 500ms
    pub probe_timeout: Duration,    // 500ms
    pub indirect_probes: usize,     // 3
    pub suspect_timeout: Duration,  // 3s
    pub gossip_interval: Duration,  // 200ms
    pub gossip_fanout: usize,       // 3
    pub dead_retention: Duration,   // 24h
}
```

### 2.2 状态机（06 §2.1.2 原样）

```
Alive ──(直接+间接探活均失败×probe_multiplier)──▶ Suspect ──(suspect_timeout 未 refute)──▶ Dead
  ▲                                                │
  └──────(refute：本人收到 Suspect 自己 → incarnation+1 广播 Alive)──┘   Dead 保留 TTL 后移出
```

**SwimActor 不是 actor**——它是 parrot-remote 内的 tokio 任务集（probe 循环 + gossip 循环 + 合并器），原因：membership 是**基础设施**（早于任何 actor 系统可用），不能依赖引擎调度（自举悖论规避——07 §6.4 同理）。与 actor 系统的交互仅经 `NodeTable`（成员变更 → NodeTable 增删 → facade 路由自动生效）。

### 2.3 wire（SYSTEM_EVENT payload，bincode）

```rust
// SYSTEM_EVENT (0x20) payload = bincode(MembershipGossip)
pub struct MembershipGossip {
    pub events: Vec<MemberEvent>,
    pub seen_from: String,           // 发送者 node_id
    pub digest: u64,                 // 全表 xor 指纹（对账用）
    pub full_sync: Option<Vec<Member>>, // 指纹不匹配时对端回全量（定向，非广播）
}
pub enum MemberEvent { Upsert(Member), Remove(String) }
```

合并规则（偏序）：收到 Upsert 时 `(incarnation, status 优先级 Dead>Suspect>Alive)` 比较——新事件仅在偏序更大时覆盖并继续传播；否则丢弃（防旧事件回环）。

### 2.4 K1 测试义务

- `swim_state_machine`：Alive→Suspect→Dead→TTL 清理全路径 + refute 回 Alive（注入时钟）
- `swim_convergence_kill9`：3 节点 mem 集群 kill 1 → **≤3.5s** 多数派标 Dead（门禁值）
- `swim_partition_heal`：bus.partition（POC 已有工具）30s → 愈合后 ≤30s 收敛、无人工介入
- `swim_gossip_merge`：乱序/重复/旧 incarnation 事件合并幂等
- `swim_refute_storm`：双向 Suspect 不产生无限 refute（incarnation 上限 + 退避）

---

## 3. K2 · Receptionist（receptionist.rs + parrot-api 增量）

### 3.1 API（parrot-api context 增量）

```rust
// parrot-api/src/context.rs trait ActorContext 增量（双引擎 context 转发实现）
fn receptionist_register(&mut self, key: ReceptionistKey);
fn receptionist_deregister(&mut self, key: &ReceptionistKey);
fn receptionist_subscribe<'a>(&'a mut self, key: ReceptionistKey)
    -> BoxedFuture<'a, ActorResult<ReceptionistStream>>;

pub struct ReceptionistKey(String);   // 命名空间规范 "{scope}/{name}"，scope∈{edge,cloud,jvm,ray,media}
pub enum ReceptionistEvent {
    Registered   { key: ReceptionistKey, remote_path: String },
    Unregistered { key: ReceptionistKey, remote_path: String },
}
```

### 3.2 实现要点

- 本地表 + 订阅者表（06 §2.2.2）；注册产生 `SystemEvent::ReceptionistSync` 搭 MembershipGossip 同车（同一 gossip 循环，不另起任务）
- 订阅先回放快照再续流；订阅端按 remote_path 去重
- 节点 Dead → 该节点全部注册项批量 Unregistered（K1 回调驱动）
- remote_path 即 facade 可解析路径——订阅端 `parrot.get_actor(placeholder(remote_path))` 直接得 RemoteActorRef（DEV_01 §3.7 集成闭环）

### 3.3 K2 测试义务

- `receptionist_register_subscribe_flow`（本节点注册→订阅者收到）
- `receptionist_cross_node`（双节点 mem：A 注册 → B 订阅收到 remote_path → B ask A 成功——**端云核心场景的最小闭环**）
- `receptionist_dead_node_cleanup`（节点 Dead → Unregistered 批量推送）

---

## 4. K3 · QuicTransport（transport/quic.rs）

```rust
pub struct QuicTransport { /* quinn::Endpoint, alpn=["parrot/1"], 0-RTT */ }
```

实现要点（06 §2.3）：
- connect：`Endpoint::connect(addr, "parrot/1")`；**每连接一个 bi 流作控制面（握手/心跳/SYSTEM_EVENT），每个 ask 一个 uni 流作数据面**（帧格式不变——流内仍是 Wire 1.0 字节序列）
- listen：accept bi 流 → 握手 → 控制面任务；accept uni 流 → 数据面分发
- 连接迁移验证：集成测试改 IP（容器网络）不断流
- 依赖增量：`quinn = "0.11"` + `rustls`（quinn 自带 feature）——已在决策档

### K3 测试义务

- `quic_connect_handshake`（与 TCP 同断言集——Transport 抽象的意义）
- `quic_uni_stream_per_ask`（并发 ask 各走独立流，无 HOL——对比 TCP 单流串行的吞吐差异断言）
- `quic_migration`（端点 IP 变更后连接存活）
- `quic_tcp_parity`（DEV_01 T2 的 parity 测试扩一列）

---

## 5. K4 · mTLS（tls.rs）

- `RemoteConfig.tls: Option<TlsConfig { cert_path, key_path, ca_path }>`（06 §2.4）
- TCP：tokio-rustls 包 TcpStream（Framed 不变）；QUIC：quinn rustls 集成
- **node_id ↔ 证书 CN/SAN 强绑定**：握手完成后校验 TLV node_id == 证书 CN，不匹配断连（防身份伪造）
- 工具：`parrot-remote-cli cert gen`（自签 CA+节点证书，开发环境 10 分钟可用）

### K4 测试义务

- `tls_handshake_success` / `tls_reject_bad_ca` / `tls_node_id_cn_mismatch`（断连+错误码）
- `cert_gen_tool`（CLI 冒烟）

---

## 6. K5 · akka 网关 + pb 栈

### 6.1 pb 栈（parrot-remote codec.rs 的 Pb 分支实现）

- prost 依赖（决策档已裁定）；`.proto` 文件 P2 手维护（06 ⚠ 复核点：初期量少）
- TYPE_KEY `pb:{package}.{Message}`；`#[remote(key = "pb:...")]` 显式键（DEV_01 §3.4 已支持）
- CodecRegistry 不变——pb entry 与 bin entry 同表

### 6.2 JVM 网关（parrot-protocol-jvm/，Maven 项目）

```
parrot-protocol-jvm/
├── src/main/scala/parrot/protocol/jvm/
│   ├── ParrotTransportExtension.scala   # Netty TCP/QUIC(Netty-Quic) server，说 Wire 1.0（含 golden vectors 单测）
│   ├── BridgeActor.scala                # akka typed：ASK→AskPattern→REPLY/REPLY_ERR；TELL→ref !；STOP→UnsupportedOperation（I4 缺口保留）
│   ├── ReceptionistBridge.scala         # akka receptionist key ↔ parrot ReceptionistEvent 双向
│   └── Codec.scala                      # protobuf-java 对应；golden vectors 对齐
└── src/test/scala/...                   # vectors 逐字节断言 + 双向 ask 集成
```

路径映射：`parrot://{gw}/jvm/user/{akkaPath}` ↔ akka selection/receptionist（06 §2.5.2）。

### 6.3 K5 测试义务与门禁

- JVM 单测：golden vectors 逐字节（读 `docs/vectors/wire1.json`——DEV_01 冻结的同一组）
- 集成（Rust 侧驱动）：`akka_interop_ask_echo/cpu`（RTT <300µs 同机回环，门禁）
- `akka_death_notification`（JVM actor 死 → parrot is_alive=false + Unregistered 事件）
- CI 矩阵：GitHub Actions 双语言一致性（06 §2.5.3）

---

## 7. K6 · 集成语义复跑

RC1-RC8 在三节点 mem 集群 + gossip 开启下复跑（断言不变，环境加噪：随机 kill 一节点后 N ask 仍达多数派成员）；QUIC+TLS 全链路冒烟（ask/echo 过 akka 网关）。

---

## 8. DoD

1. workspace 全绿（632+P1 增量+P2 增量）；JVM 侧 `sbt test`/`mvn test` 绿
2. `swim_convergence_kill9` ≤3.5s；`akka_interop` RTT <300µs
3. golden vectors Rust/JVM 双语言矩阵绿（CI）
4. 集成锚点：`ActorContext` 增量落 parrot-api（receptionist 三方法）；thread/actix context 各自转发实现
5. 04 §13 P2 勾选 + ADR-18 状态更新

---

## 9. 实现注意事项

1. **SwimActor 不用引擎**：见 §2.2——自举顺序：Transport ready → SWIM 起来 → 成员进 NodeTable → 引擎系统才可注册。测试里用 `tokio::time::pause` 注入时钟（POC 已用此模式）。
2. **gossip 单车原则**：MembershipGossip 与 ReceptionistSync **同一循环同一车**（一帧多事件），不另起定时器——带宽预算（≤50KB/s/节点）靠这个。
3. **quinn 0.11 API**：`Endpoint::client/server` + `Connection::open_uni/accept_uni`；uni 流数据面记得 `finish()` 半关闭。
4. **JVM 侧帧解码**：Netty ByteToMessageDecoder 复刻 `Frame::decode` 半包语义（不足一帧 return，不消费 readerIndex）——golden vectors 的 partial 用例必须移植。
5. **证书轮换**（P3 完整）：P2 只做双证书容忍（CA 链同时信任新旧）。
6. **Receptionist key 校验**：非法字符（空格/`*`/`#`）启动即拒——ACL 前置（P3.5 铺垫）。
7. **K5 并行策略**：JVM 网关与 K1-K4 完全并行（不同语言不同仓）——集成只需 vectors 文件与端口约定，这是 golden vectors 先行的回报。
