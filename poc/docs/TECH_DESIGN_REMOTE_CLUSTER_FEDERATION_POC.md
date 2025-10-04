# Parrot 远程/集群/联邦 Actor 框架 —— POC 实证版技术详细设计

> 版本：v1.0（POC 验证通过版）
> 日期：2026-10-03
> 定位：本文档是 `docs/TECH_DESIGN_04/05/06` 的**实证版**。所有设计点均以 `poc/Mx-poc/`（远程层）与 `poc/remote-poc/`（网关与文档） 下的可运行 POC 为凭据；文中每个论断都标注对应 POC 文件与测试。
> 隔离性：`poc/Mx-poc/`（远程层）与 `poc/remote-poc/`（网关与文档） 是独立 workspace（不进根 workspace、不进 parrot crate），对正式系统零污染。

---

## 0. 结论速览（TL;DR）

| 验证目标 | POC | 结果 |
|---|---|---|
| L0 帧格式跨语言一致 | `poc/src/frame.rs` + `akka-gw/AkkaGw.java` + `ray-adapter/ray_gw.py` + `erlang-gw/erlang_gw.erl` | ✅ 13/13 测试通过 |
| L1 传输（内存/TCP 双形态） | `poc/src/transport.rs` | ✅ RC1/RC8 |
| L2 位置透明（RemoteActorRef 实现 ActorRef trait） | `poc/src/remote_ref.rs` | ✅ RC2/RC6 |
| 远程消息落到**真实 parrot thread actor** | `poc/src/ingress.rs` + `node.rs` | ✅ RC2/RC8 |
| SWIM 成员关系（疑罪从有合并/refute） | `poc/src/swim.rs` | ✅ 5/5 |
| Receptionist 服务发现订阅推送 | `poc/src/swim.rs` | ✅ |
| 发现→远程调用 联动 | `poc/tests/p2_cluster_poc.rs::discovery_then_remote_call` | ✅ |
| Rust↔JVM(Akka) 跨语言互通 | `akka-gw/AkkaGw.java` + `p3_akka_interop.rs` | ✅ 双向 ask |
| Rust↔Ray(Python) 跨语言互通 | `ray-adapter/ray_gw.py` + `p3b_ray_interop.rs` | ✅ 双向 ask |
| Rust↔Erlang/OTP 跨语言互通 | `erlang-gw/erlang_gw.erl` + `p3c_erlang_interop.rs` | ✅ 双向 ask |

**运行方式**：
```bash
cd poc/Mx-poc/poc && cargo test          # 全部 13 个测试（JVM/Python/Erlang 需 java/python3/erl 可用）
```

---

## 1. 宏观架构（六层模型，POC 对应关系）

```
┌──────────────────────────────────────────────────────────────┐
│ L4 联邦层   akka-gw(JVM)  ray-adapter(Python)  erlang-gw(OTP)  [lite: TS/C++] │  POC: p3/p3b/p3c ✅
├──────────────────────────────────────────────────────────────┤
│ L3 集群层   SWIM membership │ Receptionist 发现   （sharding 留 P4） │  POC: swim.rs ✅
├──────────────────────────────────────────────────────────────┤
│ L2 远程层   RemoteActorRef(ActorRef trait) │ 三级路由 │ 错误映射     │  POC: remote_ref.rs ✅
├──────────────────────────────────────────────────────────────┤
│ L1 传输层   Transport 抽象：memory / TCP（QUIC 同抽象可插拔）          │  POC: transport.rs ✅
├──────────────────────────────────────────────────────────────┤
│ L0 线协议   28B 定长头 + path + type_key + payload（LE）              │  POC: frame.rs ✅
├──────────────────────────────────────────────────────────────┤
│ L-1 引擎    parrot ThreadActorSystem（真实引擎，POC 直连非 mock）      │  ✅
└──────────────────────────────────────────────────────────────┘
```

**POC 期间发现并修正的两个设计级 bug**（这正是 POC 的价值）：

1. **帧头长度文档错误**：04-06 文档写"24B 定长头"，但字段逐个累加（ver 1 + ft 1 + flags 2 + cid 8 + reserved 8 + path_len 4 + key_len 4）= **28B**。自洽实现测不出来（golden 往返对称），**跨语言互操作立即暴露**：JVM 网关按 24B 实现后 `BufferOverflowException`，逐字节 dump 定位。→ 正式文档统一修正为 28B 头（`frame_len` 4B 前缀独立于头之外）。
2. **TCP 服务端时序死锁**：`listen_once` 先阻塞 accept 再返回地址，客户端在拿到地址后才 connect → 死锁。POC 修正为 **bind 立即返回地址 + accept 后台化**（`tcp_listen()` + oneshot 回传 link），正式实现沿用该模式。

---

## 2. L0 线协议（微观：字节级）

### 2.1 帧布局（已实现定稿）

```
偏移   长度  字段           说明
0      4    frame_len      u32 LE，= 后续 body 总长（不含本 4B）
4      1    version        =0x01
5      1    frame_type     ASK=0x10 REPLY=0x11 REPLY_ERR=0x12 TELL=0x13 STOP=0x14
6      2    flags          u16 LE，保留（压缩/加密/mask bit 位）
8      8    correlation_id u64 LE，ASK/REPLY 配对键
16     8    reserved       保留 0（未来 flags 扩展/meta）
24     4    path_len       u32 LE
28     var  path           UTF-8（允许非 ASCII，POC 验证往返）
24+4+p 4    key_len        u32 LE
..     var  type_key       ASCII（"bin:" Rust↔Rust / "pb:" 跨语言）
..     var  payload        body_len - 28 - path_len - key_len
```

- **三种语言逐字节一致**（Rust `frame.rs` / Java `AkkaGw.buildFrame` / Python `build_frame`），golden 字节样例：`[40,00,00,00, 01,10,00,00, 01..cid..00, 00×8, 12,00,00,00, "/user/akka_service", 0a,00,00,00, "bin:u:Ping", 29,00,00,00,00,00,00,00]`（见 `/tmp` dump 记录，文档化于 frame.rs golden_vectors）。
- 半包/粘包：decode 返回 `Ok(None)` 不消费缓冲；循环内 `while let Some` 抽干（RC1 断言两帧粘包一次解出）。

### 2.2 消息类型（TYPE_KEY 双轨）

| 轨道 | 格式 | 适用 |
|---|---|---|
| bin | `bin:<crate>::<Type>` | Rust↔Rust（bincode/手写，零跨语言约束） |
| pb  | `pb:<proto_package>.<Message>` | 任意跨语言（JVM/Python/TS/C++） |

POC 用显式注册验证 registry 模式（`codec.rs::CodecRegistry::install::<M>`），正式版由 `#[derive(RemoteMessage)]` 自注册（POC 已验证 registry 的全部错误路径）：
- 出站未注册 → `not remotable`（**不出网**，本地立即失败）——RC2 断言
- 入站未知 key → `unknown TYPE_KEY`（两端版本不一致信号）
- JVM/Python 侧按 key 查服务表，未命中回 `REPLY_ERR("unknown service/key")`——P3/P3b 断言

### 2.3 帧类型语义（含 POC 实现位置）

| 类型 | 语义 | POC |
|---|---|---|
| ASK | 请求-响应，cid 配对，REPLY/REPLY_ERR 必回其一 | `remote_ref.rs::send_with_timeout` ↔ `ingress.rs` ASK 分支 |
| REPLY | 成功响应，type_key = 回复消息类型 | 同上 |
| REPLY_ERR | 失败响应，payload = 错误文本；错误类映射见 §4.3 | `reply_to_result` |
| TELL | 尽力而为单向，无回执；目标不存在静默丢弃 | `ingress.rs` TELL 分支 |
| STOP | 远程停止（空 payload） | `ingress.rs` STOP 分支 |

---

## 3. L1 传输层（中观）

### 3.1 抽象：FrameLink

```rust
pub struct FrameLink {
    pub sender: FrameSender,          // 克隆共享，内部 mpsc 串行化写侧
    pub incoming: mpsc::Receiver<Frame>, // 解帧后的入站队列
}
```

- **内存形态** `memory_pair()`：双 mpsc 直连（单测确定性，无 IO）
- **TCP 形态** `tcp_connect/tcp_listen`：`into_split()` 半拆分 + 读写任务；`TCP_NODELAY`
- QUIC（P4）：同一 `FrameLink` 抽象下换 `quinn` 双流实现，上层零改动——POC 用 memory/TCP 双形态已证明传输可插拔性

### 3.2 连接建立时序（POC 修正版）

```
服务端                          客户端
bind() → 立即返回 addr
spawn(accept → oneshot<link>)    
                     addr ────→  connect(addr)
accept 完成 → link ─→ oneshot    
spawn_endpoint(link)             spawn_endpoint(link)
```

---

## 4. L2 远程层（中观+微观）

### 4.1 RemoteActorRef：位置透明的实现凭据

`RemoteActorRef` 实现 parrot 的 `ActorRef` trait（`remote_ref.rs`），意味着：
- 任何持有 `BoxedActorRef` 的代码**不感知**目标是本地还是远程
- 与进程内 cross-engine（Thread↔Actix，已在前轮 POC 验证）同一套 trait 面

### 4.2 ask 全链路（微观）

```
send(msg)
 ├─ CodecRegistry::encode_outgoing → (key, payload)   [未注册即失败，不出网]
 ├─ cid = callbacks.next_cid()
 ├─ callbacks.insert(cid, oneshot)
 ├─ sender.send(Frame::ASK{cid, path, key, payload})
 └─ await oneshot（无界 或 send_with_timeout 超时）
      ├─ REPLY     → decode → Ok(msg)
      ├─ REPLY_ERR → 错误映射（§4.3）
      └─ 超时      → callbacks.remove(cid)；迟到 REPLY 查表 miss → drop（无泄漏）
```

### 4.3 错误映射（POC 实测）

REPLY_ERR 文本 → ActorError：
- 含 `ActorNotFound` / `not found` → `ActorNotFound`
- 含 `Timeout` → `TimeoutDetail`
- 含 `stopped` → `Stopped`
- 其他 → `MessageHandlingError`（含对端语言异常栈文本，如 Java `Index 4 out of bounds`、Python `unknown key`——POC 断言）

### 4.4 双超时竞态规避（05 §6.1 落地）

入站 ASK 处理时 ingress 对本地 actor 调用**不设超时**（`r.send(msg)` 无 timeout 参数）——超时责任完全在发起端。避免"对端已回包但本地已超时放弃"的双重超时窗口错配。

### 4.5 反压传导链（TELL 路径）

`ingress.rs` TELL 分支 `await r.deliver(msg)`：本地 actor mailbox 满（Block 策略）→ deliver 挂起 → ingress 泵挂起 → `incoming` 队列积压 → 读任务挂起 → TCP 缓冲满 → **对端写挂起 → 对端 sender 队列积压**。全链路背压，无丢弃（POC 结构已就位；正式版加高水位日志）。

---

## 5. L3 集群层（SWIM + Receptionist）

### 5.1 SWIM 状态机（POC `swim.rs`，5 测试全绿）

```
           ping 直接探测（超时 T_direct）
  Alive ───────────────────────────────→ Suspect（本地标记）
   ↑ ↘ pingReq 经 K 个间接节点仍不可达          │ gossip 全量表合并传播
   │                                            ↓ 升级超时
  refute（inc+1, Alive）←──── 新代表胜      Dead
```

**合并规则（疑罪从有，POC 断言）**：
- `incarnation` 大者胜（refute 的本质）
- 同 incarnation：状态取更坏（Alive < Suspect < Dead）

**故障注入验证**：`ClusterBus::partition("C")` 模拟节点宕机 → A 探测 C：直接失败 → 间接（经 B）失败 → `used_indirect=true` → suspect → gossip 到 B → dead。全链路 5 断言。

### 5.2 Receptionist（服务发现）

`registry: key → [(node, service_addr)]` + 订阅推送（mpsc）。POC 断言：注册两次推送两次快照（增量/去重为正式版工作）。**联动测试** `discovery_then_remote_call`：B 注册 echo → A 订阅收到推送 → A 直接 `remote_ref("/user/echo").send(Ping)` 成功——**发现层与远程层在 POC 中真实串通**。

### 5.3 POC 边界（正式版补）

- gossip 用全量表（正式版 digest 增量 + piggyback 到 ping/ack）
- 探测循环 POC 为单步可注入（正式版周期 task + probe 列表轮转）
- sharding（P4）未做——依赖 receptionist + 一致性哈希，设计已在 06 文档

---

## 6. L4 联邦层（跨语言网关，实证重点）

### 6.1 网关形态（四种语言同构）

```
Rust 节点 ←─ Parrot Wire ─→ 网关进程 ←─ 语言原生 API ─→ 目标 actor 生态
                              │
                              ├─ JVM:      services 表 → java Function（正式版: ActorRef.tell/ask 适配）
                              ├─ Python(ray): @ray.remote actor 方法分发
                              ├─ Erlang/OTP: 函数分发（正式版: gen_server:call 到目标进程）
                              └─ (规划) TS/C++ lite client：协议子集直连
```

Erlang 网关的额外价值：OTP 是 actor 模型的原生实现（非库适配），与其互通证明 Parrot Wire 对"进程=actor、邮箱消息、let-it-crash"原生语义生态同样零侵入——网关 POC 已含 gen_server 结构（pending 管理与消息分发），正式版 handler 直接替换为 `gen_server:call/2`。

### 6.2 已验证互通矩阵

| 方向 | 路径 | POC 断言（方言可辨识） |
|---|---|---|
| rust→akka ask | `Ping(41)` | 回 `Pong(42)`（akka 方言 +1） |
| rust→akka ask | `Add(3,4)` | 回 `34`（a*10+b） |
| akka→rust ask | `Ping(100)` | 回 `Pong(200)`（rust 方言 *2） |
| rust→ray ask | `Ping(40)` | 回 `Pong(42)`（ray 方言 +2） |
| rust→ray ask | `Add(20,22)` | 回 `1042`（a+b+1000） |
| ray→rust ask | `Ping(100)` | 回 `Pong(300)`（rust 方言 *3） |
| rust→erlang ask | `Ping(39)` | 回 `Pong(42)`（erlang 方言 +3） |
| rust→erlang ask | `Add(20,22)` | 回 `10042`（a+b+10000） |
| erlang→rust ask | `Ping(100)` | 回 `Pong(500)`（rust 方言 *5） |
| 错误路径 | unknown key | REPLY_ERR 原样透传对端异常文本（Java 异常/Python ValueError/Erlang `{unknown_service, K}`） |
| TELL | 双向 | 异步无回执 |

**方言断言的意义**：三种语言对同一 TYPE_KEY 给出可区分的响应（+1 / +2 / *2 / *3 / a*10+b / +1000），证明消息确实穿越了语言边界并执行了对端逻辑，而非缓存/回环假象。

### 6.3 回复类型 key（POC 修正点 3）

JVM/Python handler 返回 `(reply_key, payload)` 二元组——**回复消息的 TYPE_KEY 由服务方声明**（如 `bin:u:Ping` 的回复是 `bin:u:Pong`），发起方按 key 查表解码。POC 初版让回复沿用请求 key，导致 rust 侧 `downcast::<Pong>` 失败（u64 ≠ Pong）——已修正并在设计上固化：**TYPE_KEY 描述的是"消息类型"，请求与响应是两个类型**。

### 6.4 进程内 vs 跨进程结论（复述自可行性分析）

- Thread↔Actix 进程内互通：已验证（前轮 `parrot/tests/test_cross_engine_poc.rs`）
- JVM/Python 与 Rust 进程内嵌入：**反模式**（运行时/GC 冲突），网关进程跨 TCP 是正解——本轮 POC 验证
- C++ lite：进程内 C ABI（`pl_connect/pl_ask/pl_poll`）可行，POC 未含（P4）

---

## 7. POC 目录结构与代码量

```
poc/Mx-poc/
├── docs/TECH_DESIGN_REMOTE_CLUSTER_FEDERATION_POC.md   ← 本文档
├── poc/                       # 独立 workspace（不进根 workspace）
│   ├── Cargo.toml             # path 依赖 ../../parrot（真实引擎）
│   ├── src/
│   │   ├── frame.rs           # L0 帧（28B 头）+ golden
│   │   ├── transport.rs       # FrameLink: memory/TCP
│   │   ├── codec.rs           # TYPE_KEY registry（错误路径完备）
│   │   ├── messages.rs        # POC 消息集（Ping/Pong/Add）
│   │   ├── remote_ref.rs      # RemoteActorRef（ActorRef trait）
│   │   ├── ingress.rs         # 入站路由 → 真实 parrot actor
│   │   ├── node.rs            # spawn_endpoint / endpoint_pair
│   │   └── swim.rs            # SWIM + Receptionist
│   └── tests/
│       ├── p1_remote_poc.rs      # RC1/RC2/RC6/RC8（5 测试）
│       ├── p2_cluster_poc.rs     # SWIM/Receptionist（5 测试）
│       ├── p3_akka_interop.rs    # JVM 互通（1 测试）
│       ├── p3b_ray_interop.rs    # Ray 互通（1 测试）
│       └── p3c_erlang_interop.rs # Erlang/OTP 互通（1 测试）
├── akka-gw/AkkaGw.java        # JVM 网关（纯 JDK，~200 行）
├── ray-adapter/ray_gw.py      # Ray 网关（ray 2.51，~150 行）
└── erlang-gw/erlang_gw.erl    # Erlang/OTP 网关（OTP 29，~160 行）
```

Rust POC ~1100 行 / Java ~200 行 / Python ~150 行 / Erlang ~160 行。POC 质量边界：无 mTLS/重连/心跳/digest 增量（均在 05/06 文档设计中，POC 目标是验证**协议语义与互通可行性**）。

---

## 8. 对正式实现的迁移指引（P1 落地顺序）

按 POC 模块 → 正式 crate 的映射（每步都有 POC 测试作为验收基线）：

| 步 | 正式实现 | 参照 POC | 验收 |
|---|---|---|---|
| 1 | `parrot-remote/src/frame.rs`（正式版：bytes 常量表 + fuzz） | frame.rs | RC1 迁移 |
| 2 | `parrot-remote/src/transport/`（tcp.rs 先行） | transport.rs | RC8 迁移 |
| 3 | `RemoteMessage` derive + registry（bincode 自动编解码） | codec.rs | RC2 迁移 |
| 4 | RemoteActorRef + ingress（接 facade get_actor） | remote_ref/ingress | RC2/RC6 |
| 5 | `parrot-cluster/src/swim.rs`（周期探测 + digest） | swim.rs | P2 全套 |
| 6 | Receptionist（增量推送 + 跨节点 gossip） | swim.rs Receptionist | P2 |
| 7 | akka-gw 正式化（Akka HTTP/Artery 替代裸 socket 可选） | akka-gw | P3 |
| 8 | ray-adapter 正式化（注册中心 + 断线重连） | ray-adapter | P3b |
| 9 | erlang-gw 正式化（handler → gen_server:call 分布式进程） | erlang-gw | P3c |

**POC 已固化的三个设计修正必须带入正式版**：
1. 28B 头（非 24B）
2. bind-先返回-accept-后台化时序
3. 回复 TYPE_KEY 由服务方 handler 声明

---

## 9. 已知边界与后续

- POC 传输用长度前缀 TCP；QUIC/mTLS/连接复用池在 05/06 设计中未实现（P4）
- SWIM gossip 为全量表；正式版 digest + piggyback（带宽 O(节点数) → O(变更)）
- 未做：sharding、passivation、cluster singletons（06 文档 P3/P4 范围）
- lite TS/C++ 客户端：协议已由三语言实现验证可移植性，TS 版列入 P4
- 性能基线：POC 未做吞吐基准（避免与 bench/ 冗余）；正式版落地时用 `bench/` 框架对 remote ask 做 RTT/吞吐基线
