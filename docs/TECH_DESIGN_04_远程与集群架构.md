# 04 · 远程 Actor / 集群 / 异构互通架构设计

> 状态：**设计提案（待评审）** · 2026-10-02 · **2026-10-04 修订**（场景全景并入虚拟量化机器人；拓扑段对齐 [07 联邦协议 1.0](./TECH_DESIGN_07_异构联邦协议设计.md) 三模式；MQTT 桥接定位见 07 §4.5；帧格式以 07 §2 为准；**工程要求（世界级/工业级/电信级/百万级节点——适用于整个系统，含进程内核心）见 07 §14，实现规约（依赖选型/内聚耦合/机制策略分离/结构化/三态可读）见 07 §14.6 E5**——本文 §12 性能预算与 §14 风险登记是 07 §14 E 系列的输入）
> 前置：进程内双引擎互通已实证（`tests/test_cross_engine_poc.rs`，X1–X8 全绿，ADR-1 统一消息擦除 + 统一 `ActorRef` trait 是互操作的根基）。
> 范围：远程模式（对标 akka-remote）、集群模式（对标 akka-cluster）、异构 actor 联邦（ray/akka/erlang/边缘轻量系统/MQTT 生态）、端云一体场景。
> 数据基准：第十一轮包装税实测提醒——**任何跨边界设计都必须把"热路径零损耗"作为硬约束**。

---

## 1. 愿景与场景全景

```
┌─────────────────────────── 云端（数据中心）───────────────────────────┐
│  parrot 中枢（thread 引擎=算力/actix 引擎=IO 编排）                     │
│    ├── ray 集群（大规模并行计算任务）      ── 联邦网关 ──┐             │
│    ├── akka 集群（存量 JVM 业务系统）      ── 联邦网关 ──┤             │
│    ├── erlang/OTP（高可用电信级存量系统）  ── 联邦网关 ──┤             │
│    └── parrot-cluster（membership/receptionist/分片）  ──┘             │
│  联邦控制面（07 §6.4，全部 parrot actor 自举）：                        │
│    RelayHub 中继 · Directory 目录(Raft) · RouteReflector 路由           │
└──────────────▲──────────────────────────▲──────────────────────────────┘
               │ QUIC（弱网/断连容忍）      │ QUIC/TLS
      ┌────────┴────────┐        ┌────────┴─────────┐        ┌──────────────┐
      │ 手机 RPA 端      │        │ 物理机器人        │        │ 虚拟量化机器人 │
      │ parrot-lite(TS) │        │ parrot-lite(C++) │        │ 容器化 parrot │
      │ 接云指令/执行/上报│        │ 任务执行/传感上报  │        │ 交易执行/行情  │
      └─────────────────┘        │ 主动决策请求       │        │ 订阅/低延迟决 │
                                 └──────────────────┘        │ 策环/云端回测  │
                                                              └──────────────┘
      软件虚拟人 = 容器化 parrot 节点（标准 remote，无特殊处理）
      存量 MQTT 设备 = 经 ParrotMqttBridge 入联邦（07 §4.5，零改造接入）
      流媒体终端 = 经 ParrotLiveKitBridge 入联邦（07 §8.3，信令 actor 化 +
                   媒体 WebRTC 旁路：摄像头/数字人/行情播报/屏幕共享皆
                   "LiveKit 语义终端 + parrot 可编程节点"双重身份）
```

四类端侧实体平级（协议层无特权路径，差异只在部署位置与流量特征）：

| 端类型 | 形态 | 特征 | 关键需求 |
|---|---|---|---|
| 手机 RPA 端 | parrot-lite(TS) | 移动网络、频繁断连 | QUIC 0-RTT 重连 + durable tell |
| 物理机器人 | parrot-lite(C++) / Rust 主控+C ABI | 实时传感/执行 | 低延迟 + 批量帧（传感流摊薄帧头） |
| **虚拟量化机器人** | 容器化 parrot 节点 | 部署在交易所机房/边缘 DC；行情与订单流高频 | **亚毫秒 ask + 行情订阅高频 tell 流 + 断连期本地自治** |
| 软件虚拟人 | 容器化 parrot-lite(TS) 或全量 parrot | 资源充足 | 标准 remote，无特殊处理 |

虚拟量化机器人典型链路：`云端策略 actor --ask--> parrot://quant/edge-1/hft-exec/user/order-gw`（下单执行）；行情回传 `行情源 --durable tell--> 云端信号 actor`（断连不丢）。与手机 RPA 的唯一差别是选 TCP（同机房亚毫秒）而非 QUIC、心跳更密——纯配置差异，零新协议概念。

五类互通需求（用户需求 4 的完整矩阵）：

| 互通对 | 边界类型 | 方案 |
|---|---|---|
| parrot-thread ↔ parrot-actix | **进程内**（已实现） | 统一 `BoxedActorRef`，零拷贝 `Box<dyn Any>` |
| parrot ↔ parrot（跨节点） | 跨进程 | parrot-remote 协议（本文档主体） |
| parrot ↔ akka | 跨进程+跨语言 | 协议网关（JVM 端 parrot-protocol SDK） |
| parrot ↔ ray | 跨进程+跨语言 | ray adapter（语义映射 actor↔task） |
| parrot ↔ 边缘 lite | 跨进程+跨网络 | parrot-lite（协议子集实现）+ QUIC |
| parrot ↔ MQTT 设备 | 跨协议生态 | ParrotMqttBridge 桥接网关（07 §4.5：QoS↔投递语义映射） |
| parrot ↔ LiveKit 终端 | 跨协议生态（流媒体） | ParrotLiveKitBridge 双平面桥接（07 §8.3：信令 actor 化 + 媒体 WebRTC 旁路）——流媒体 actor 网络 |

---

## 2. 总体架构：六层模型

```
L5 应用层      业务 actor（thread / actix / ray / akka / edge-lite）
L4 联邦层      Federation Gateway：akka-bridge(JVM) / ray-adapter(py) / 语义映射
L3 集群层      Membership(SWIM gossip) · Receptionist(key 发现) · Cluster Sharding · Singleton
L2 远程层      RemoteActorRef · 路由决策(本地→节点表→receptionist) · 编解码 · 投递语义
L1 传输层      Transport trait：TcpTransport / QuicTransport / (管理面 gRPC 可选)
L0 线缆协议    帧格式 · 握手/版本协商 · mTLS · 压缩(lz4/zstd 可选)
```

设计铁律（从既有工程教训导出）：

1. **热路径零损耗**：进程内互通不走任何新增抽象层（现有 `ActorRef` trait 不改签名，`RemoteActorRef` 只是它的又一个实现——就像 `ThreadActorRef`/`ActixActorRef` 一样）。第十一轮证明包装税集中在每消息路径；remote 层的编解码、路由查表只在"确认目标是远程"之后才介入。
2. **统一门面不变**：`ParrotActorSystem` 仍是唯一入口；注册的子系统从 `Thread/Actix` 扩展出 `Remote(node)`。业务代码拿到的一律是 `BoxedActorRef`，位置透明。
3. **协议简单到能在小设备实现**：机器人 C++ / 手机 TS 都要能写得出解析器 → 帧格式用长度前缀 + 定长头，不用任何"聪明"的编码。

---

## 3. 核心问题一：消息可序列化（整个方案的根基）

**现状**：`MessageEnvelope.payload: Box<dyn Any + Send>`（`parrot-api/src/message.rs:527`，2026-10-04 核）——进程内零拷贝，但 `Any` 不可序列化。这是 remote 的第一性约束。

### 方案 A：双轨消息协议（推荐）

```rust
/// 远程可达消息的显式契约（新 trait，不碰现有 Message）
pub trait RemoteMessage: Message {
    /// schema registry 的类型键（建议 "crate::Type" 全名，带版本后缀）
    const TYPE_KEY: &'static str;
    fn encode(&self) -> ActorResult<Bytes>;            // bincode/protobuf 由宏选择
    fn decode(bytes: Bytes) -> ActorResult<Self>;      // 关联函数
}

/// 发送侧决策（伪码，位于 RemoteActorRef::send）
if let Some(codec) = registry.lookup(msg_type_id) {    // TypeId → TYPE_KEY
    let frame = codec.encode(msg)?;                    // 只在远程路径发生
    transport.send(node, frame).await
} else {
    Err(ActorError::NotRemotable(type_name))           // 明确错误，不静默
}
```

- **本地路径完全不变**：thread↔actix 互通继续走 `Any` 零拷贝（X 系列已验证）。
- 远程可达性在**编译期可得**（是否 impl `RemoteMessage`），可在类型层让 `TypedRemoteRef<M: RemoteMessage>` 的 `ask` 只接受远程消息——错误提前到编译期（备选：运行时 registry 查表报错，先实现这个，编译期约束做增强）。
- registry 由 `parrot-api-derive` 宏自动注册：`#[derive(RemoteMessage, Serialize, Deserialize)]` 生成 `inventory`/linkme 自注册项（零手工登记）。

**代价**：双接口（`Message` + `RemoteMessage`）；需要序列化格式决策——
- **bincode**：Rust↔Rust 节点最快（无 schema 依赖），跨语言不行；
- **protobuf**：跨语言必需（akka/ray/边缘 lite 都认），Rust 侧略慢；
- **推荐双栈**：`TYPE_KEY` 命名空间区分 `bin:{...}` 与 `pb:{...}`，Rust↔Rust 自动用 bincode，跨语言自动用 protobuf。编解码器接口 `MessageCodec` 留扩展位（P2 评估 rkyv 零拷贝反序列化）。

### 方案 B：全 serde 化（否决）

所有消息强制 `Serialize`，本地也走 bytes。优点：单一语义、无 NotRemotable 错误。**否决理由**：本地热路径全面倒退（装箱后还要序列化/反序列化，第十一轮刚量化过每消息路径敏感度——c64 ask -45% 的教训）；与 parrot "性能立身"定位冲突；强制所有业务消息可序列化是过强约束（含句柄/闭包的消息合法存在于本地场景）。

### 方案 C：rkyv 零拷贝全程（推迟）

wire 与本地统一 rkyv bytes，接收侧 downcast 时惰性反序列化。理论最优但复杂度高（对齐/生命周期跨 FFI 边界难），P2 重新评估，不阻塞 P1。

### 可行性结论

方案 A 与现状完全兼容、热路径零损、错误契约清晰。**序列化本身是成熟技术（bincode/protobuf/rkyv 都有生产验证），风险不在"能不能"而在"接口设计让业务方多写多少"——宏自动注册把它压到一行 derive。**

---

## 4. 核心问题二：路径与寻址

### 4.1 路径格式升级

```
本地（现状保留）：
  thread:  /user/{uuid}
  actix:   actix://{TypeName}/{uuid}

统一远程路径（新增，门面层规范）：
  parrot://{node_id}/{system_name}/user/{actor_uuid}
  示例：    parrot://cloud-1/thread-main/user/8f3a...
           parrot://edge-robot-7/lite/user/servo-arm
```

- `node_id`：集群成员 ID（见 §7），单机未组网时 = 进程启动配置的 `node_name`。
- 门面 `internal_get_actor` 路由升级为三级：**本地 registry（O(1)）→ 节点表前缀匹配（O(1) HashMap）→ receptionist key 查询（跨节点）**。现状的"遍历 fallback"（`parrot/src/system.rs:298`，2026-10-04 核：default 系统优先 + 遍历注册系统，双引擎节点经此路径远程可达——POC p4a 已实证）保留为最后兜底并标记 deprecated。
- 兼容：旧路径 `/user/x`、`actix://y` 在本地解析不变；远程 ref 必然携带 `parrot://` 前缀。

### 4.2 RemoteActorRef：位置透明的关键

```rust
pub struct RemoteActorRef {
    path: String,                    // parrot://node/system/user/uuid
    node: NodeHandle,                // 连接池句柄 + 心跳状态
    codec: Arc<MessageCodec>,        // §3 双栈编解码
    system: WeakParrotSystem,        // 本地系统回查（ask 回包路由）
}

impl ActorRef for RemoteActorRef {
    // ask = 远程 RPC：生成 correlation_id，本地 oneshot 挂起，
    //       wire 发 AskFrame{cid, path, msg}，对端回 ReplyFrame{cid, result}
    // deliver = 远程投递：TellFrame，不等处理完，mailbox 接收即 ACK（at-most-once 起步）
    // is_alive = 节点连接状态 && 最近成功投递（弱一致，文档明示）
    // stop = StopFrame，对端软停该 actor
}
```

**ask 超时语义**与 ADR-10 对齐：`send_with_timeout(None)` = 无界远程 ask（文档警示弱网必配超时）；`Some(d)` 到期调用方放弃，消息可能仍被处理（与本地语义一致，跨网放大——文档强调幂等）。

---

## 5. 核心问题三：传输协议选型

| 维度 | 自定义 TCP 帧 | QUIC | gRPC |
|---|---|---|---|
| LAN RTT 开销 | **~30–80µs** | ~80–150µs | ~300µs–1ms |
| 弱网/断连恢复 | 手写重连 | **0-RTT 重连、连接迁移（IP 变更不断流）** | TCP+mTLS 慢 |
| 队头阻塞 | 单连接有（多路复用需自己分流） | **流级无 HOL** | HTTP/2 流级有 |
| 跨语言实现成本 | 低（定长头+长度前缀，C++/TS 半天） | 中（quiche/quinn 成熟库） | 低 |
| 生态/管理面 | 无 | 好 | **最好（鉴权/负载均衡/监控）** |

**决策：L1 定义 `Transport` trait，P1 实现 TcpTransport（对标 akka artery 的选型），P2 增加 QuicTransport 作为边缘默认。** gRPC 不进数据面（延迟不达标），可选用于管理面（节点注册/监控上报）。

```
Wire 帧格式（正式 1.0 以 07 §2 为准——本块为历史草案存档，码点已统一为 07 §2.2 表）：
  [u32 frame_len][u8 version][u8 frame_type][u16 flags][u64 correlation_id]
  [u8 hop_count][u8 hop_limit][u48 reserved]
  [u32 path_len][path bytes][u32 type_key_len][type_key bytes][payload bytes]

frame_type（07 §2.2 终表）: HANDSHAKE=0x01/ACK=0x02 · HEARTBEAT=0x03/ACK=0x04 ·
            ASK=0x10 / REPLY=0x11 / REPLY_ERR=0x12 / TELL=0x13 / STOP=0x14 /
            FRAGMENT=0x15 · SYSTEM_EVENT=0x20 · RESOLVE_Q/R=0x21/0x22 ·
            INVALIDATE=0x23 · ERROR=0x7F
握手：TLV 自描述体（07 §2.4）：版本协商 + node_id/realm/cluster + 能力位 + mTLS 之上
心跳：2s 间隔，5 个丢失判半开（SWIM 加速确认，见 §7）；电信档 200ms×5（07 §14.3）
```

---

## 6. 核心问题四：投递语义与可靠性

| 级别 | P1 提供范围 | 机制 |
|---|---|---|
| at-most-once | tell 默认 | fire-and-forget，连接断则丢 |
| at-least-once | ask + 超时重试（用户显式 `retry_policy`） | `MessageOptions.retry_policy` 已有字段（`parrot-api/src/message.rs:510`，2026-10-04 核），跨网生效 |
| exactly-once | **不做**，文档指导 | 幂等消息（业务键去重）+ at-least-once 组合达成业务等价（与 akka 立场一致） |

断线容忍（边缘刚需）：
- **tell 持久化队列（P3）**：`RemoteActorRef.deliver` 可选 `durable: true` → 本地落盘 WAL，节点恢复后续传（store-and-forward）。云端对端侧的 proxy actor 持有界邮箱（复用 `BackpressureStrategy`，ADR-12 的背压贯通到跨网边界）。
- 端侧上行（传感/决策请求）建议一律 ask（有回执）或 durable tell。

---

## 7. 核心问题五：发现与集群

### 7.1 Membership（三种方案对比）

| 方案 | 一致性 | 依赖 | 边缘适用 | 结论 |
|---|---|---|---|---|
| 静态配置（seeds 列表） | 无 | 无 | 可 | **P1 必做**（remote MVP 的节点表就是它） |
| SWIM gossip | 最终一致 | 无（内嵌） | 可（低带宽） | **P2 采用**（akka/HashiCorp 同款；失败检测 π/2 加速怀疑→探活→确认） |
| 外部协调（etcd/consul） | 线性 | 部署 etcd | 否（设备不能依赖） | 云内可选增强，不进核心 |

### 7.2 Receptionist（跨节点服务发现，端云场景的承重件）

```rust
// 云端编排 actor 想找"所有在线的 RPA 端"：
ctx.receptionist_register("edge/rpa");          // 端侧 lite 启动即注册（带 node/能力标签）
ctx.receptionist_subscribe("edge/rpa").await;   // 云端订阅：上线/下线事件流
// → 云拿到 BoxedActorRef 列表（实为 RemoteActorRef），直接 ask/tell
```

- 实现：每节点本地 receptionist 表 + membership gossip 携带（或 SystemEvent 帧单独同步）；变更以事件流推给订阅者。
- 这是"云端发现哪些端在线、端能力是什么"的标准答案，避免对 etcd 的依赖。

### 7.3 Cluster Sharding（P4，对标 akka cluster sharding）

- 复用现有 `SchedulingMode::Sharded { affinity_key }`（ADR-14）的概念延伸：**affinity_key 经一致性哈希决定"实体应该住在哪个节点"**，而非哪个线程。
- 节点增减 → 分片重均衡 → 实体迁移（passivation + 状态外部化到 KV/WAL 再激活）。
- Singleton：集群级单例 actor（租约-based，membership 多数派裁决）。
- **P4 再做**：P1-P3 场景（端云）用 receptionist + 静态路由已足够。

---

## 8. 异构联邦：ray / akka / 边缘 lite

### 8.1 akka 网关（JVM 侧协议实现）

**为什么必须网关**：Akka 无公开可插拔 transport SPI（artery 内部协议无文档、版本耦合），实现 parrot 协议是唯一干净路径。

```
JVM 进程：  parrot-protocol-jvm（Netty server，说 §5 帧协议）
            └── BridgeActor（akka typed）：收到 parrot AskFrame →
                本地 ask ActorRef[?] → ReplyFrame 回程
            └── 注册协议侧 receptionist 映射：akka receptionist key ↔ parrot key
```

- 语义映射：parrot ask ↔ akka `AskPattern.ask`；tell ↔ `tell`；消息体 = protobuf（TYPE_KEY `pb:` 栈）。
- 生命周期：parrot `is_alive` ↔ akka `AddressTerminated` 事件桥接。
- **工作量主体在 JVM（Scala/Java 工程师），Rust 侧零改动**（协议对称）。
- 部署形态：与存量 akka 系统同进程（extension 加载）或独立 sidecar。同进程更优（少一跳）。

### 8.2 ray 适配器（语义鸿沟最大的一个）

Ray 是 **method-call 模型**（`handle.method.remote(args)`）+ object store，不是信箱模型。适配必须显式映射：

| parrot | ray |
|---|---|
| `ask(Msg)` | `handle.on_message.remote(serialized_msg)`（ray actor 内跑一个 dispatcher：按 TYPE_KEY 解包分发到用户方法） |
| `deliver(Msg)` | `handle.on_message.remote(...)` 不取结果（fire-and-forget 近似，注意 ray 任务不会因不取结果而取消） |
| actor path | ray actor handle 无法按名寻址 → **适配器维护 path↔handle 表**（parrot 侧 spawn "ray-backed actor" 时注册） |
| 生命周期 | ray actor 死亡由 ray 自愈（max_restarts）；适配器桥接为 Terminated 事件 |

- 形态：Python 侧 gateway worker（订阅 parrot 协议 + ray client API），云端部署。
- **定位边界**：ray 参与 parrot 网络的最佳用法是"计算任务域"（parrot 编排 → ray 执行大并行任务 → 结果回 parrot actor），不追求 ray actor 完整语义等价。这个定位同时把映射复杂度砍半。

### 8.3 parrot-lite（边缘三语言：TS → C++ → Java 顺序）

- **协议子集**：仅 Handshake/Heartbeat/Ask/Reply/Tell + receptionist 注册（无 gossip——边缘节点不参与 membership 表决，只向云端注册）。二进制帧对 TS 用 `ArrayBuffer` 手写解析（千行内），C++ 同规模。
- **TS 先行**：手机 RPA（App 内嵌 JS 引擎/WebView）+ 软件虚拟人（Node 容器）覆盖两个场景；QUIC 用 WebTransport/axios-http3 视宿主能力降级到 TCP/TLS。
- **C++ 次之**：物理机器人任务系统。同时提供 **parrot-lite C ABI**（`parrot_lite_*` 系列 C 函数：`pl_connect/pl_spawn/pl_ask/pl_poll`），让机器人主控若是 Rust/C++ 混合体可进程内直调，也回应了 §9 的附加问题。
- Java 版视客户存量再排。

---

## 9. 附加问题：异构实现能否**进程内**互通？（诚实的可行性分析）

**结论先行：只有 C++（经 C ABI）值得做进程内；JVM/Python 技术上勉强、工程上否决；但即使 C++，进程内收益也仅限单一场景。**

| 组合 | 技术路径 | 可行性 | 判定 |
|---|---|---|---|
| thread ↔ actix | 同进程同堆，统一 trait | ✅ 已实现 | 事实标准 |
| parrot ↔ C++ lite | C ABI + FFI：C++ 侧 actor 挂回 parrot mailbox（消息跨界仍序列化为 bytes） | **高**——ABI 稳定、无双 runtime 纠缠（lite 无自己的调度器，`pl_poll` 由宿主线程驱动） | **做**（机器人单机内 Rust 主控 + C++ 执行器：省掉回环网络，且崩溃域仍隔离于 FFI 边界） |
| parrot ↔ akka | JNI/FFI 把 JVM 拉进进程 | 低——JVM 需完整启动、GC 线程与 tokio 抢核、JVM crash = 进程同殒、异常跨界不可传播 | **否决**（网关 sidecar 拿到 95% 收益，5% 网络开销） |
| parrot ↔ ray | ray C++ core（ray::ActorHandle）嵌入 | 极低——ray runtime 需要 raylet/GCS/object store 常驻，嵌入=进程内跑半个集群；生命周期由 ray 独裁 | **否决** |

**普适原理**：进程内互通的前提不是"同进程"，而是**同调度域**（消息投递 = 函数调用，无序列化、无双生命周期）。任何自带 runtime 的实现（JVM/Python/ray）进进程只是"把网络栈换成了 FFI 栈"，序列化一样要付，却引入崩溃域耦合。**跨语言互通的正解永远是协议，不是指针**——这也是 akka（artery）和 ray（object store RPC）自己的选择。

---

## 10. 端云场景映射（需求 5 落到组件）

| 场景 | 端侧 | 云侧 | 关键组件 |
|---|---|---|---|
| 手机 RPA | parrot-lite(TS)，receptionist 注册 `edge/rpa` + 能力标签 | 编排 actor 订阅 `edge/rpa`，指令 ask/tell 下发 | QUIC/WebTransport + 断线重连（连接迁移：IP 变更不断流）+ durable tell（指令不丢）+ 端侧慢消费 → 云 proxy 有界邮箱（Block 策略反压） |
| 物理机器人 | parrot-lite(C++) 或 Rust 主控 + C++ 执行器（C ABI 进程内） | 任务下发（tell）/ 决策请求（ask，云端 actor 返回指令）/ 传感流（durable tell 高频→建议批量帧） | 同上 + P2 评估批量帧（单帧多消息，传感流摊薄帧头） |
| 软件虚拟人 | 容器内 parrot-lite(TS) 或全量 parrot（资源够则） | 同 RPA 模式 | 标准 remote 节点，无特殊处理 |
| **虚拟量化机器人** | 容器化全量 parrot（交易所机房/边缘 DC），receptionist 注册 `quant/edge` | 云端策略 actor ask 下单执行（`quant/edge-1/.../order-gw`）；行情源 durable tell 回传云端信号 actor；回测任务可派发 ray 域 | **TCP 直连（同机房亚毫秒）优先，跨 DC 走 border 中继（hybrid）**；断连期本地自治（durable tell WAL 续传）；心跳间隔加密（1s）；行情流批量帧（06 P4.3）摊薄帧头 |
| 云中枢 | — | parrot-cluster（thread=算力域 / actix=IO 编排域）+ ray 网关（计算域）+ akka 网关（存量域）+ erlang 网关 | receptionist 统一编目一切端与域 |

架构要点：**云端对所有端一律只持有 `RemoteActorRef`（Receptionist 发现），端侧只连云端接入点**——端云之间星型拓扑起步（简单、可运维、NAT 不可直连），云端内 parrot 节点间 full mesh（gossip）。端与端不直连，需要协作时经云端中转 actor。

> **2026-10-04 对齐注**：本段叙述已由 [07 §5](./TECH_DESIGN_07_异构联邦协议设计.md) 形式化为三种可配置拓扑模式——**hub 中继 / mesh 直连 / hybrid（默认）**，并定义了直连失败自动降级中继的降级链与 hop_limit 环路防护。端云场景 = hub 形态（端只连云端接入点）；云内节点 = mesh 形态；虚拟量化机器人（边缘 DC 容器化 parrot）按网络条件选直连或经 border 中继 = hybrid 形态的典型用户。中继服务（RelayHub）与目录服务（Directory）全部用 parrot actor 实现（07 §6.4 自举裁定）。

---

## 11. 安全（P2 起强制）

- 数据面 mTLS：节点证书（云端内部 mTLS；端侧设备证书/密钥入安全区——KeyStore/TEE）。
- 设备身份：首次注册签发短期证书（云端 CA），token 绑定 node_id。
- 授权：receptionist key 天然是权限边界（`edge/rpa` 命名空间 ACL），P3 实现注册/订阅 ACL。
- 审计：SystemEvent 帧记录节点加入/退出/证书轮换。

---

## 12. 性能预算（目标，P1 落 bench 验证）

| 链路 | 预算 | 参照 |
|---|---|---|
| 本地 thread↔actix ask（已实测） | ~2–5µs | X-PoC |
| 同 LAN parrot↔parrot ask RTT | **<150µs**（目标 100µs） | akka artery ~60–100µs |
| 同 LAN tell 单向 | <60µs | — |
| 端↔云 ask RTT | 网络主导（协议份额 <5%） | — |
| 云内 protobuf 编解码（1KB 消息） | <10µs | bincode <2µs |

新增基准资产：`bench/remote-bench/`（Rust 双节点进程模拟 + 真双机），延续 bench 目录方法论。

---

## 13. 实施路线（建议 4 个阶段）

| 阶段 | 内容 | 工作量估算 | 出口判据 |
|---|---|---|---|
| **P0 已完成** | 进程内 thread↔actix 互通（X1–X8 八场景） | — | test_cross_engine_poc（单测试函数覆盖 X1-X8 场景矩阵）+ workspace 632 测试全绿（2026-10-04 核） |
| **P1 Remote MVP** | 帧 v1 + TcpTransport + RemoteActorRef + 静态节点表 + RemoteMessage 宏/registry + 双栈 codec + ask/tell/stop + remote-bench | 3–4 周 | 双进程 ask RTT <150µs；RC1–RC8 语义等价用例跨进程复跑通过；POC 侧 RC1-RC8 已在 `poc/Mx-poc` 15 测试中实证（2026-10-04） |
| **P2 Cluster + 联邦** | SWIM membership + receptionist + QuicTransport + mTLS + 断线重连 + **akka 网关(JVM)** + K0 远程 spawn 管理协议（06 §P2.6） | 4–6 周（含 JVM 工程） | 节点故障 30s 内集群收敛；parrot↔akka ask/tell/死亡通知互通 |
| **P3 边缘** | parrot-lite(TS) + durable tell(WAL) + receptionist ACL + 云 proxy 反压贯通 + ray adapter | 4 周（TS）；ray 2 周 | 手机端断网 5min 重连后消息零丢（durable）；ray 任务域往返跑通 |
| **P4 集群语义深化** | cluster sharding（affinity_key 哈希到节点）+ singleton + C++ lite + 批量帧 | 4 周+ | 分片重均衡无消息丢失 |

> **2026-10-04 增补**（07 §11 统一路线图）：P1-P4 之后新增 **P5 联邦**（Directory/RESOLVE 帧/拓扑三模式/中继降级/前缀 ACL/LiveKit 桥——控制面全部 parrot actor 自举）与 **P6 规模化**（gossip digest、路由/Directory 分片、50 集群仿真、百万节点地址遍历）；erlang 网关与 ParrotMqttBridge 并入 P3 交付。**全阶段受 07 §14 工程验收基线（E1 世界级质量 / E2 工业级运维 / E3 电信级容灾 / E4 百万级节点规模——整个系统的要求，含进程内核心与联邦全栈）上位约束**。详见 [07 §11/§14](./TECH_DESIGN_07_异构联邦协议设计.md)。

每阶段回归兜底：全量 workspace 测试 + X-PoC + B 系列（业务守恒在远程路径同样适用——B4 Saga 跨引擎跑）。

---

## 14. 风险登记册

| # | 风险 | 等级 | 缓解 |
|---|---|---|---|
| R1 | Any→序列化断层破坏现有 API | 高 | 双轨设计（§3A）：现有 Message 零改动；NotRemotable 是显式错误非静默 |
| R2 | akka 网关 = JVM 工程，人力/维护双栈 | 高 | 网关收敛在单一 BridgeActor 模式；协议测试向量（golden frames）双语言共享 |
| R3 | ray 语义映射陷阱（method 模型 vs 信箱） | 中 | 定位收窄为"计算任务域"（§8.2）；不做完整语义等价承诺 |
| R4 | 边缘三语言实现漂移（协议演进不同步） | 中 | golden frames 一致性测试矩阵（Rust/TS/C++ 各自跑同一向量集）；帧 v1 冻结 |
| R5 | 弱网下 ask 泛滥（超时重试风暴） | 中 | 重试预算/退避（retry_policy 已有字段承载）；文档模式指引 |
| R6 | 跨网背压缺失导致云端积压 | 中 | 云 proxy 有界邮箱 + BackpressureStrategy 贯通（ADR-12 延伸） |
| R7 | 安全后补难 | 低 | 帧头预留 flags；P1 就做 mTLS 骨架（自签起步），不做"先明文后加密" |
| R8 | 集群语义（sharding/singleton）复杂度失控 | 低 | P4 才做；P1-P3 场景不需要；先 receptionist 满足端云 |

---

## 15. 可行性总结论

1. **远程模式**：可行，无技术未知数。序列化（bincode/protobuf）、传输（TCP/QUIC）、RPC（cid+oneshot）全是成熟件；难点仅在接口设计质量（RemoteMessage 双轨 + 宏自动注册）。
2. **集群模式**：可行。SWIM/receptionist 有 akka/HashiCorp 成熟先例可对标；风险在工程量不在技术。
3. **ray/akka 联邦**：可行但不对称——akka 是协议网关（干净）、ray 是语义适配器（收窄定位后干净）；两者都**不需要 parrot 核心改动**。
4. **全局两两互通**：架构上由"统一 ActorRef + parrot 协议 + 网关"三点自动达成——任何实现只要会说 parrot 协议（原生或经网关），就与全网互通；无需 N×N 两两桥接（星型协议枢纽，这是对等网状方案的关键优势）。
5. **进程内跨语言**：仅 C++ lite（C ABI）值得且要做（正中机器人场景）；JVM/Python 否决（§9 原理）。
6. **端云场景**：全部组件在 P1–P3 覆盖；最重的端侧需求（断连容忍）由 durable tell + QUIC 连接迁移 + receptionist 组合承接。
7. **与现有资产的关系**：进程内互通（已完成）是 remote 的语义基座（ActorRef 门面不变）；ADR-14 Sharded 的 affinity_key 是集群分片的预留种子；ADR-12 背压是跨网反压的既有件——**这份架构几乎不需要推翻任何既有决策，是延伸而非重写。**

---

## 附：与既有 ADR 的关系

| 既有决策 | 本架构的角色 |
|---|---|
| ADR-1（Any 擦除） | 进程内互通根基；remote 层在其上叠双轨序列化 |
| ADR-10（send/deliver 语义） | RemoteActorRef 逐字继承（无界 ask / 显式 deliver） |
| ADR-12（背压贯通） | 延伸到跨网边界（proxy 有界邮箱） |
| ADR-14（Sharded affinity_key） | 集群分片的哈希输入（P4） |
| ADR-17（包装税红线） | 热路径零损铁律的出处 |

> 本文档为 ADR-18 的输入；评审通过后拆分为 ADR-18(remote)/19(cluster)/20(federation)。

## 实现文档索引（2026-10-02 同步产出）

- **[05 · P1 远程模式实现详细设计](./TECH_DESIGN_05_P1远程实现详细设计.md)**：可直接编码级——WBS/帧格式逐字节/Transport trait/双栈 codec/RemoteActorRef 全方法/ingress 矩阵/过程宏代码生成/RC1–RC8 测试矩阵/依赖清单/DoD。代码锚点已核实（ActorError 11 变体、Message trait、derive 入口、thread 注册表）。
- **[06 · P2–P4 集群与联邦实现详细设计](./TECH_DESIGN_06_P2-P4集群与联邦详细设计.md)**：SWIM 状态机与参数表、Receptionist API、QUIC 传输、mTLS、akka 网关（JVM 组件与桥接规则表）、parrot-lite TS API、durable tell WAL、ray adapter 映射表、sharding/singleton/批量帧；含全阶段测试矩阵与实现级风险增量。
