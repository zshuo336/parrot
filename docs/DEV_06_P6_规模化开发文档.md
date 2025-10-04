# DEV_06 · P6 规模化开发文档（digest / 路由与目录分片 / 50 集群仿真）

> 状态：**开发文档（实施合同）** · 2026-10-04 · 基准 commit `9e35fc0` + DEV_01-05 交付后
> 设计依据：[07 §14.4](./TECH_DESIGN_07_异构联邦协议设计.md)（百万级节点规模模型——规模对象=百万级**节点**）+ [06](./TECH_DESIGN_06_P2-P4集群与联邦详细设计.md) 附录 B I6（digest 转正）
> 前置：DEV_05 DoD（Directory/RouteGossip 单分片可运行——分片化的地基）
> 上位约束：[07 §14.6 E5](./TECH_DESIGN_07_异构联邦协议设计.md)；规模触发器表（07 §14.4 尾）是本阶段的"何时做"依据

---

## 0. 范围与红线

**做**：SWIM gossip digest 增量 + push/pull 对账（S1）+ 路由表分片/border 聚合 supernet（S2）+ Directory 分片化（一致性哈希，S3）+ 50 集群 compose 仿真 + 百万节点地址空间遍历（S4，G5 门禁）。

**不做（红线）**：
- 百万物理容器（仿真=容器复用进程模拟 + 数字孪生——07 §14.4 结构保证第 5 条）
- 协议帧/状态机任何变更（E1.15：规模轴扩展不改协议，只变配置与部署形态——**本阶段零 wire 变更是验收项**）

---

## 1. 任务分解

```
S1 digest/push-pull ── 改造 DEV_02 SWIM（n>200 触发器）
S2 路由聚合 ── 改造 DEV_05 RouteGossip（集群数>100 触发器）
S3 Directory 分片 ── 改造 DEV_05 DirectoryStore（一致性哈希分片，每分片 Raft 3 副本）
S4 仿真基建 ── 独立（docker compose 生成器 + 数字孪生压测器）
```

估算：S1=5d S2=4d S3=8d S4=8d 缓冲 4d。

---

## 2. S1 · SWIM digest 增量 + push/pull（06 I6 转正）

### 2.1 机制

- gossip 循环改两级：**digest 探测**（每轮发 u64 xor 指纹，字节级增量）+ 指纹不匹配时 **push/pull 对账**（定向全量交换，非广播）
- 触发自适应：成员 ≤200 用 DEV_02 全量事件模式（兼容）；>200 自动切 digest（配置 `swim.mode = "auto|full|digest"`）

### 2.2 测试义务（07 §14.4 规模参数表）

- `digest_bandwidth`：单节点稳态 gossip 带宽 **≤10KB/s**（P6 档；1.0 档 ≤50KB/s——全量模式基线对比表同帧输出）
- `digest_convergence_parity`：digest 模式与全量模式收敛时间等价（±10%）——语义不回退
- `push_pull_reconcile`：人为表漂移（绕过 gossip 直接注入）→ 对账修复 ≤3 轮

---

## 3. S2 · 路由表分片与 border 聚合（07 §14.4 结构保证第 3 条）

- border 对外只通告**集群级 supernet 前缀**（`parrot://prod/eu-1/#`），集群内明细不外泄——路由表条目数=集群数级（百级）而非节点数级（百万级）
- Directory 下发同样按前缀聚合；节点本地缓存条目 = 常去前缀集（工作集假设，LRU 上界 1024 条）

测试：`supernet_aggregation`（500 节点集群对外路由表 ≤ 集群数+常数）；`cache_working_set`（缓存上限 LRU 淘汰，命中率 >99.9% 稳态——07 §14.4 参数表）。

---

## 4. S3 · Directory 分片化

```rust
// 一致性哈希：shard = hash(realm/cluster 前缀) % shard_count
// 每分片 = 独立 Raft 组（3 副本）——DirectoryStore 参数化实例（DEV_05 F5 的横向复制，非新代码路径）
// RESOLVE 路由：border 按目标前缀定位分片组 → 代理查询（客户端无感）
```

测试：`directory_shard_scale`（仿真 50 集群元数据 → 分片组各承载 ~1 集群量级条目，单分片内存 ≤200MB/100 万条目——07 §14.4 参数表）；`shard_rebalance`（加分片组后条目迁移零丢失）。

---

## 5. S4 · 50 集群仿真与百万节点数字孪生（G5 门禁）

### 5.1 仿真基建

```
tools/federation-lab/
├── composegen.rs      # 生成 50 集群 × 200 节点 compose 拓扑（每容器 1 supervisor 进程 × N 逻辑节点模拟）
├── twin.rs            # 数字孪生：百万节点逻辑地址空间的全遍历路由正确性压测器
└── scenarios/         # 收敛/路由/故障注入剧本（复用 DEV_02 混沌矩阵的注入器）
```

- 容器复用进程模拟：每容器跑 4 逻辑节点（50×200×4=40k 物理进程内模拟百万地址的抽样验证 + 孪生全遍历补全）
- **孪生遍历**：不启真实连接——按 Directory/路由表状态静态验证"任意 `parrot://realm/cluster/node/...` 解析端点正确性"全空间断言（百万级 ≤10min 完成）

### 5.2 验收（07 §14.4 参数表逐行 + G5）

| 指标 | 门禁 |
|---|---|
| 跨集群路由收敛 | ≤10s（50 集群仿真实测） |
| 单 Directory 分片 | 100 万条目 / ~200MB |
| 节点缓存命中率 | >99.9% 稳态 |
| 控制面带宽 | ≤10KB/s/节点（digest 档） |
| 端到端跨集群 ask | 额外 ≤1 跳中继 + ≤10ms 目录成本（miss 时） |
| 孪生全遍历 | 百万节点地址空间路由正确性 100% |

---

## 6. DoD（07 §11 P6 出口判据）

1. workspace 全绿 + 仿真基建三剧本（收敛/路由/故障）全过
2. §5.2 参数表逐行达标（报告落 `docs/SCALE_REPORT.md`）
3. **E1.15 断言**：对比 P1 冻结的 golden vectors——wire 零变更（diff 输出空）
4. 07 §11 P6 勾选 + G5 门禁记录归档

---

## 7. 实现注意事项

1. **digest 切换的混跑窗口**：全量/digest 两模式节点并存一个发布周期（滚动升级）——digest 节点对全量节点发 digest 帧，全量节点回退 push/pull（协商位进握手 capabilities 预留位，勿新码点）。
2. **supernet 与 ACL 的交互**：前缀聚合后 ACL 粒度=前缀级——DEV_05 F8 的 ACL 表同样按 supernet 配置（细粒度权限留给 receptionist key 层，两层各司其职）。
3. **孪生遍历不是性能测试**：验证**正确性**（地址→端点映射），延迟性能由 §5.2 的实测行覆盖——别把两件事混在一个断言里。
4. **40k 进程的 compose 资源**：50×200 容器超单机限制——composegen 支持"每容器多 supervisor"折叠模式（默认 4:1），CI 用 10 集群缩减版，全量 50 集群跑专用机器（G5 里程碑一次）。
