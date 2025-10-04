# DEV_04 · P4 集群深化开发文档（sharding / singleton / 批量帧 / C++ lite）

> 状态：**开发文档（实施合同）** · 2026-10-04 · 基准 commit `9e35fc0` + DEV_01-03 交付后
> 设计依据：[06](./TECH_DESIGN_06_P2-P4集群与联邦详细设计.md) 第三部分（P4.1-P4.3）
> 前置：DEV_02 DoD（SWIM/Receptionist 是 sharding/singleton 的地基）
> 上位约束：[07 §14.6 E5](./TECH_DESIGN_07_异构联邦协议设计.md)

---

## 0. 范围与红线

**做**：Cluster Sharding（D1，复用 ADR-14）+ Cluster Singleton（D2，租约制）+ 批量帧 BATCH（D3）+ C++ lite 与 C ABI（D4）。

**不做（红线）**：
- 实体自动状态迁移协议（06 P4.1 明示：P4=无状态实体或状态外部化已由业务完成；有状态迁移=akka event-sourced 配合，超出范围）
- Directory/RESOLVE（P5）
- gossip digest（P6）

---

## 1. 任务分解

```
D1 sharding ── 依赖 SWIM 成员表 + ADR-14 Sharded 调度
D2 singleton ── 依赖 SWIM（多数派租约）
D3 BATCH ── 依赖 DEV_01 帧层（flags bit2 已分配）
D4 C++ lite / C ABI ── 独立（协议子集同 TS lite）
```

估算：D1=5d D2=3d D3=2d D4=6d 缓冲 3d。

---

## 2. D1 · Cluster Sharding（parrot-remote/src/sharding.rs）

### 2.1 机制（06 P4.1）

```rust
//! 职责：实体→节点的放置与惰性激活；一致性哈希环是策略，实体语义是机制。

pub struct ShardCoordinator {         // 每节点一份（gossip 收敛视图），无中心
    ring: HashRing,                   // 一致性哈希环：虚节点 256/node（防雪崩）
    entities: RwLock<HashMap<String, EntityState>>,
}
pub struct EntityState { node: String, status: EntityStatus } // Active/Migrating/Passivated

// 消息路径：
// ask(parrot://…/user/entity-{key}) → facade 命中 ShardRouter（注册为 /user/entity-* 前缀处理器）
// → ring.node(hash(key)) → 目标节点本地 spawn（thread 引擎 Sharded{affinity_key:key}——两级亲和）
```

### 2.2 rebalance

membership 变更 → 哈希环更新 → 迁出节点对未完成消息 drain → 实体 passivate（stop + 可选状态快照 KV 回调）→ 新 holder 惰性再激活。

### 2.3 测试义务（06 验收）

- `sharding_kill_node`：3 节点 kill 1 → **5s 内**其分片实体在新 holder 重建 → 消息零丢失（receptionist + 重试）
- `sharding_affinity_l1`：同 key 连续消息命中同线程（ADR-14 断言复用）
- `sharding_rebalance_drain`：迁移期消息不丢不重（drain 窗口断言）

---

## 3. D2 · Cluster Singleton（parrot-remote/src/singleton.rs）

租约制（06 P4.2）：候选节点向多数派（Alive 集合）周期续约（lease 10s / renew 3s）；持有者 Dead → 租约到期 → 候选序号最高者接管（接管等待 = lease 全额过期，防双主）。

用途：云 proxy 分配器、全局定时器、ACL 管理者。

测试：`singleton_takeover`——kill singleton 节点 → **≤13s** 新 singleton（lease 10s+确认 3s，门禁）；分区两侧 singleton 唯一性（多数派租约在非多数侧不可续约）。

---

## 4. D3 · 批量帧（BATCH）

- flags::BATCH 置位时 payload = `N × [u32 len][frame bytes]`（06 P4.3）——帧层扩展 `Frame::batch(frames)`/`Frame::iter_batch()`
- 发送侧：传感流场景（1000 msg/s）攒批窗口 5ms 或 64 帧触发
- 测试：`batch_bandwidth`——10k msg/s 传感流带宽降 **>60%**（vs 单帧，门禁）

---

## 5. D4 · C++ lite 与 C ABI（`native/cpp-lite/`）

### 5.1 C++ lite（协议子集同 TS：HANDSHAKE/HEARTBEAT/ASK/REPLY/TELL + receptionist 注册）

- 单头文件 + 单实现（`parrot_lite.h/cpp`，≤2000 行）；帧解析 memcpy+reinterpret（固定头 28B 布局）
- golden vectors 断言（GoogleTest 或 assert 宏最小集）

### 5.2 C ABI（机器人主控混合体：Rust 主控 + C++ 执行器进程内直连——04 §9）

```c
// parrot_lite.h——稳定 C ABI（符号零破坏演进）
int  pl_connect(const char* url, const char* node_id, const char* tls_dir);
int  pl_register(const char* key, const char* path, const char* caps_json);
int  pl_ask(const char* target_path, const char* type_key, const uint8_t* payload, size_t len,
            uint8_t** out, size_t* out_len, int timeout_ms);   // 调用方 free(*out)
int  pl_tell(const char* target_path, const char* type_key, const uint8_t* payload, size_t len);
void pl_poll(int timeout_ms);   // 事件泵（onAsk 回调派发）
void pl_close(void);
```

### 5.3 测试义务

- vectors（C++ 侧断言同一 `docs/vectors/wire1.json`）
- `cpp_interop_ask`（Rust 节点 ↔ C++ lite 双向，同 akka 门禁口径）
- `cabi_smoke`（Rust dlopen C++ 共享库直调——混合体场景闭环）

---

## 6. DoD

1. workspace 全绿 + C++ 测试绿（四语言矩阵扩 C++）
2. `sharding_kill_node` ≤5s / `singleton_takeover` ≤13s / `batch_bandwidth` >60% 全过
3. 04 §13 P4 勾选；C ABI 符号表冻结（semver-checks on cbindgen 输出）

---

## 7. 实现注意事项

1. **ShardRouter 的 facade 注册**：entity 前缀处理器是 facade 三级路由的"本地 registry"特例——注册为 `/user/entity-*` 通配（facade 需支持前缀通配查找，DEV_01 未做——本阶段给 `ParrotActorSystem::register_prefix_handler` 增量，注意与 remote_lookup 的顺序：本地前缀 handler 在远程分支之前）。
2. **租约的多数派定义**：Alive 集合的严格多数（>n/2）——分区时少数派侧续约失败自动降级，无需额外心跳。
3. **BATCH 与 URGENT 交互**：批量帧不设 URGENT（语义互斥，编解码断言拒绝）。
4. **C ABI 的字符串所有权**：`pl_ask` 出参 `out` 由调用方 `pl_free(out)`（补一个 pl_free）——双 free/泄漏是 C 混合体最常见事故。
5. **C++ 端 u64 cid**：`uint64_t` 原生支持（比 TS 简单），但 LE 转换手写（不依赖 Qt/boost）。
