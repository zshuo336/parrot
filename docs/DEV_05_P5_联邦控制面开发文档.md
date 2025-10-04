# DEV_05 · P5 联邦控制面开发文档（拓扑三模式 / Directory / RelayHub / RouteReflector / LiveKit 桥）

> 状态：**开发文档（实施合同）** · 2026-10-04 · 基准 commit `9e35fc0` + DEV_01-04 交付后
> 设计依据：[07](./TECH_DESIGN_07_异构联邦协议设计.md) §5（拓扑）/§6（目录）/§8.3（LiveKit）——**本阶段是 07 的主体实现**；POC p4b（hub 中继）实证
> 前置：DEV_02 DoD（SWIM/Receptionist）+ DEV_03（ACL——前缀 ACL 的权限地基）
> 上位约束：[07 §14.6 E5](./TECH_DESIGN_07_异构联邦协议设计.md)；Raft 自研裁定（决策档：范围严格限定日志复制+选主，est. 1.5k 行，jepsen 式验证）

---

## 0. 范围与红线

**做**：拓扑三模式配置与降级链（F1）+ hop 中继字段启用 + RouteGossip（F2）+ RelayHub actor（F3）+ 自研 Raft 内核（F4）+ DirectoryStore/DirectoryApi（F5）+ RouteReflector（F6）+ 节点缓存三态 + RESOLVE/INVALIDATE 帧处理（F7）+ 前缀 ACL（F8）+ ParrotLiveKitBridge（F9）。

**不做（红线）**：
- Directory 分片（P6——单分片 Raft 3/5 副本先行）
- gossip digest（P6）
- APP_ENCRYPTED flag（1.0 可选，需求驱动再启）

---

## 1. 任务分解

```
F1 拓扑配置/降级链 ── 依赖 DEV_01（hop 字段已写 0/8）
F2 hop 启用 + RouteGossip ── 依赖 F1 + DEV_02（gossip 车）
F3 RelayHub actor ── 依赖 F2（前缀路由表）——POC PrefixRouter/relay_frame 的 actor 化
F4 Raft 内核 ── 独立（纯库，est 1.5k 行，先于 F5 单独测试）
F5 DirectoryStore/Api ── 依赖 F4 + F1（border 角色）
F6 RouteReflector ── 依赖 F2/F5
F7 节点缓存 + RESOLVE/INVALIDATE ── 依赖 F5
F8 前缀 ACL ── 依赖 F7 + DEV_03 E5
F9 LiveKit 桥 ── 依赖 F7（房间 actor 跨集群可达）——可与 F5-F8 后半并行
```

估算：F1=2d F2=3d F3=4d F4=8d F5=6d F6=3d F7=4d F8=2d F9=10d 缓冲 5d。

---

## 2. F1+F2 · 拓扑三模式与 RouteGossip

### 2.1 配置（07 §5.1 原样）

```toml
[topology]
mode = "hybrid"                 # hub | mesh | hybrid（默认）
relay_fallback = true
[hub]
bind = "tcp://0.0.0.0:9700"
advertise = "tcp://node-7:9700"
[mesh]
direct_min_qos = "lan"
```

### 2.2 hop 字段启用（DEV_01 冻结位激活）

- 发送端 hop_count=0、hop_limit=握手协商值（默认 8）
- **中继节点**（RelayHub）转发时 hop_count+=1；≥hop_limit 丢弃回 REPLY_ERR(code 7 RouteUnreachable)
- 直连节点透传不改（hop 仅中继链路维护——07 §3.3）

### 2.3 RouteGossip（SYSTEM_EVENT 扩展载荷）

```rust
pub struct RouteGossip {
    pub entries: Vec<RouteEntry>,   // 增量
    pub digest: u64,                // 前缀表 xor 指纹（对账同 SWIM 模式）
}
pub struct RouteEntry { pub prefix: String, pub next_hop: String, pub cost: u32, pub version: u64 }
```

随 MembershipGossip 同车（06 I6 带宽原则）；匹配最长前缀（POC `PrefixRouter::resolve` 已实证，转正）。

---

## 3. F3 · RelayHub actor（07 §6.4 第一行）

```rust
// parrot-remote/src/roles/hub.rs
// RelayHub：hub 节点常驻 thread 引擎 actor
// 收 ASK/TELL → 前缀路由表最长匹配 → 换 cid 转发 + 回程映射表（cid_map: HashMap<u64, (orig_cid, reply_to)>）
// REPLY 回程：查 cid_map → 还原 orig_cid → 回原发送方
// 无自有状态（表=gossip 收敛视图）→ 可任意多实例
```

POC p4b 已实证全路径（JVM→hub→Erlang 中继 + cid 改写 + 回程映射）——转正工作=actor 化 + RouteGossip 表驱动（POC 是静态前缀表）。

测试：`hub_relay_matrix`（07 §11 P5 出口判据："p4b 扩展为三模式矩阵"——hub/mesh/hybrid 三配置 × ask/tell × 断 hub 降级）。

---

## 4. F4 · 自研 Raft 内核（`parrot-remote/src/raft/`——独立子模块）

### 4.1 范围（决策档裁定的严格边界）

**只做**：leader 选举 + 日志复制 + 单一 commit 点。**不做**：membership 变更协议（Raft 第 6 节 joint consensus——用重启重配置替代，Directory 3/5 副本变更走运维流程）、snapshot 压缩（条目量小，log 全量重放可接受）。

```rust
//! 职责：Raft 日志复制 + 选主（仅此；通用化=违反复核点裁定）
pub struct RaftNode<S: StateMachine> { /* term/voted_for/log[]/commit_index/peers */ }
pub trait StateMachine {
    type Cmd: Serialize + DeserializeOwned;
    fn apply(&mut self, cmd: &Self::Cmd) -> Self::Ret;
}
// 传输复用 Transport trait（Raft RpcFrame = ASK/REPLY 语义承载，cid=proposal id）
```

### 4.2 测试义务（jepsen 式，E1.8）

- `raft_election_basic`：3 节点 kill leader → 3s 内选出新主（07 §14.3 双杀行门禁）
- `raft_log_replication`：并发 1000 提交全达多数派；kill 恢复后追平
- `raft_partition_no_split_brain`：对称分区 30s——少数派侧零提交、多数派侧继续；愈合后一致
- `raft_determinism`：同日志序列两节点 apply 后状态相等（状态机确定性）

---

## 5. F5 · DirectoryStore / DirectoryApi（07 §6.4 第二行）

```rust
// parrot-remote/src/roles/directory.rs
// DirectoryStore：thread 引擎，Raft 复制状态机
//   状态：HashMap<String /*parrot://…/node*/, DirEntry { endpoints: Vec<String>, version: u64, ttl_s: u32 }>
//   Cmd：Upsert(node, endpoints) / Remove(node) / BorderDeclare(prefix) / KeyAggregate(key, nodes)
// DirectoryApi：actix 引擎，RESOLVE_Q 应答 + INVALIDATE 推送（订阅该前缀的节点）
```

- 注册来源：节点 graceful 启动/离开 + border 声明 + Receptionist key 聚合上报（06 §2.2 集群内语义不动）
- **角色配置**：`RemoteConfig.roles: Vec<Role>`（Hub | Directory | Border | Plain）——roles 进 parrot-remote 可选组件（07 §6.4 实现落点）

---

## 6. F7 · 节点缓存 + RESOLVE/INVALIDATE 帧处理

```rust
// parrot-remote/src/cache.rs
pub struct ResolveCache {
    entries: HashMap<String, CacheEntry>,  // 逻辑地址前缀 → 端点集
}
pub struct CacheEntry { endpoints: Vec<String>, version: u64, state: CacheState, fetched_at: Instant }
pub enum CacheState { Fresh, Stale, Invalid }   // TTL 60s；INVALIDATE 置 Invalid；命中 Stale 可用+降级标记
```

解析管线 ④ 落地（07 §3.2）：miss/stale → RESOLVE_Q → Directory（或就近 border 代理）→ RESOLVE_R{endpoints, version} → 写缓存 → 直连；直连失败回源一次 → 仍失败中继降级（F1 降级链）。

DEV_01 红线解除：RESOLVE_Q/R/INVALIDATE 的常量已在帧层定义，本阶段实现处理逻辑。

测试：`resolve_cache_hit_zero_rtt`（命中零网络开销——性能预算 §12 最后一行）、`invalidate_push`（Directory 条目变更 → 订阅节点缓存 Invalid → 下次 ask 重新 RESOLVE）、`directory_down_stale_service`（Directory 全灭 → stale 缓存续服务 300s 窗口内 ask 成功——07 §9）。

---

## 7. F8 · 前缀 ACL（07 §10 增补）

Directory 下发路由前缀附带 ACL：`{prefix, allow: [realm/cluster]}`；border/hub 转发前校验——跨 realm 默认拒绝。测试：`cross_realm_denied`（07 §11 P5 出口判据）。

---

## 8. F9 · ParrotLiveKitBridge（07 §8.3 双平面桥）

### 8.1 结构

```text
ParrotLiveKitBridge（parrot 节点，topology_role=Border）：
  RoomSupervisor actor（thread）——每房间：监督树生命周期
    ├── Participant actor（actix）——IO 编排：信令转发/状态推送
    ├── Track actor——轨道元数据 + Receptionist 注册 media/track/{room}/{track}
    └── Policy actor——订阅权限（F8 ACL 联动）
  信令路径：Parrot Wire（ASK/TELL）⇄ LiveKit WebSocket/JSON-RPC（livekit-api rust crate）
  媒体路径：桥内 WebRTC 端口直通（livekit-webrtc）——绝不进邮箱（吞吐/语义/加密三重否决，07 §8.3.1）
  AI 分支（可选）：旁挂转码 worker——WebRTC→解码→1-10 fps 张量流 TELL 进 actor 网（降采样硬门槛）
```

### 8.2 测试义务（07 §11 P5 出口判据："LiveKit 房间经联邦 actor 编排订阅播放"）

- `livekit_room_actor_lifecycle`（房间开关=监督树起停）
- `livekit_cross_cluster_subscribe`（`quant/eu-1` 轨道 → `media/cn-1` 订阅：信令 3 跳 actor，媒体 0 跳——wireshark 验证媒体不经桥转发路径的 actor 节点）
- `livekit_ai_downsample`（旁挂 worker 降采样后 TELL 频率 ≤10fps 断言）

---

## 9. DoD（07 §11 P5 出口判据全量）

1. workspace 全绿 + raft jepsen 式套件绿
2. 两套集群经 Directory 互访全链路（compose 双集群拓扑演示脚本）
3. p4b 扩展三模式矩阵（hub/mesh/hybrid × 降级）全过
4. `cross_realm_denied` 过；`directory_down_stale_service` 300s 窗口过
5. LiveKit 三测试过（真实 livekit-server 容器）
6. 07 §11 P5 勾选；ADR-21 附实施记录

---

## 10. 实现注意事项

1. **RelayHub 的 cid 改写表容量**：`cid_map` 有界（65536，LRU 淘汰）——防慢回程方撑爆 hub 内存（E2.2 边界全显式）。
2. **Raft 的时钟**：选举计时用注入时钟（测试确定性）；生产 tokio interval——**勿用系统时间差判断任期**（时钟跳变混沌用例）。
3. **DirectoryStore 与 DirectoryApi 的引擎分工是硬约束**：Store=thread（Raft CPU 密集）、Api=actix（高并发 IO）——07 §6.4 表格是验收项不是建议。
4. **RESOLVE 的就近代理**：border 收到 RESOLVE_Q 可代理转发给 Directory（client 只需认识 border）——降级链的一部分，别漏测 border 代理路径。
5. **LiveKit 信令并发的 actix 适配**：Participant actor 的信令转发是高频 IO——用 DEV_01 前的 actix arbiter 池经验（ENGINE_STRESS_REPORT 的 IO 场景数据）。
6. **Raft 传输复用的帧类型**：Raft RPC 用 ASK/REPLY 承载（cid=proposal id）——不新增帧类型码点（07 X2 纪律：新帧只占空闲码点，Raft 无需新码点）。
