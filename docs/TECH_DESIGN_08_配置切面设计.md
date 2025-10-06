# TECH_DESIGN_08 · 配置切面设计（Config Plane / 部署参数外置与三层优先级）

> 状态：**已实现并实证（2026-10-05 交付）** · 2026-10-06 整理成文
> 定位：配置作为独立切面（依赖倒置）——业务逻辑与物理部署解耦。对标 Akka `reference.conf`/`application.conf` 体系，取其分层合并语义、弃 HOCON 自造语法（Rust 生态直用 TOML）。
> 实证基线：`crates/parrot-config`（13 单测+doc 全绿）· `crates/parrot/tests/test_config_aspect.rs` CFG1-6 · transport 心跳旋钮时序对照测试 · ingress `ro2k` 旋钮时序测试 · workspace 全量回归 0 失败（1740+ 用例）· clippy 清零 · golden vectors 字节不变。
> 下游：[TECH_DESIGN_09 §2.2](./TECH_DESIGN_09_应用体系与多引擎统一编排设计.md)（AppManifest `config_overlay` 作为第 0 层并入本模型，裁定 Y3）· `parrot.toml.example`（全键文档样例）。

---

## 1. 问题域与设计目标

**问题**：迁移前全部部署参数（节点身份、监听地址、拓扑角色、心跳/重排/队列等调优常量）散落在三类位置——`RemoteConfig`/`ThreadActorSystemConfig` 的 builder 链、6 个编译期写死常量（心跳 2s/丢失 5 次/出站队列 1024/hop 8/重排 gap 250ms/buffer 1024）、各测试与 lab 的手写副本。改一个心跳参数要改代码重编译——业务逻辑与物理部署耦合。

**目标**：

1. 单一 TOML 文件（`parrot.toml`）控制全部部署域参数；业务代码只依赖配置抽象。
2. **三层优先级（用户契约，最高原则）**：`代码显式设置 > 配置文件 > 编译期默认`。
3. 零迁移成本：不写文件 = 行为与迁移前字节级一致（默认值=原写死常量原值）。
4. 依赖倒置：配置 crate 不依赖任何运行时 crate；两个运行时 crate 各自 `From<&Resolved>` 生成自己的配置（与 RemoteGateway 倒置模式同构，E5.2 分层铁律）。

## 2. 三层优先级模型

```
生效值 = 代码显式设置（builder with_*/直接字段，最高）
        ▷ TOML 文件（PARROT_CONFIG 指定路径，缺省 parrot.toml）
        ▷ 编译期默认（Defaults 常量 = 迁移前写死值）
```

**实现机制——Option 字段即优先级**：`ParrotConfig` 全字段 `Option<T>`；

- 代码层：builder setter 写入 `Some(v)`——一旦写入，文件层永远不覆盖（与调用顺序无关——`merge_toml` 只填 `None` 槽位。契约是"层"不是"调用序"，C4 用例锁定）；
- 文件层：`load_file` 解析 TOML 后逐键填入仍为 `None` 的槽位；未知键 warn 不失败（前向兼容），值类型错配 warn 跳过；
- 默认层：`build()` 时 `unwrap_or_else(Defaults::x)` 折叠，产出 `Resolved`（无 Option——消费侧零样板）。

与 `ThreadActorConfig` 既有 Option 覆盖机制（`merge_with_actor_config`）同构——无新心智模型。

**校验（fail-fast，build 期报错不带入运行时）**：心跳间隔 >0、heartbeat_max_loss >0、hop_limit ∈ 1..=64、bind 必须合法 SocketAddr、seeds 无空串、topology_role ∈ {normal,hub,border,directory}（非法回退 normal + warn）。

## 3. 配置模式全表（键 / 类型 / 默认 / 作用点）

| 键 | 类型 | 默认 | 作用点（最终消费处） |
|---|---|---|---|
| `thread.shared_pool_size` | usize | CPU 核数 | ThreadActorSystem 共享调度池 |
| `thread.shared_burst_workers_max` | usize | CPU 核数 | 弹性突发 worker 上限 |
| `thread.shared_burst_backlog_threshold_ms` | u64 | 100 | 扩突发 worker 的积压阈值 |
| `thread.shared_burst_idle_timeout_ms` | u64 | 5000 | 突发 worker 回收 |
| `thread.shared_queue_capacity` | usize | 10000 | 共享调度队列 |
| `thread.max_dedicated_threads` | usize | 32 | 专用线程上限 |
| `thread.default_mailbox_capacity` | usize | 1024 | actor 默认邮箱 |
| `thread.default_ask_timeout_ms` | u64 | 5000 | ask 默认超时 |
| `thread.shutdown_timeout_ms` | u64 | 10000 | 系统关闭超时 |
| `remote.node.node_id` | string | —（缺省 "parrot-node"/facade 缺省名） | 节点身份 + thread 系统名 |
| `remote.node.bind` | SocketAddr 串 | — | 监听地址 |
| `remote.node.topology_role` | enum 串 | normal | 握手 TopologyRole（星型路由角色） |
| `remote.node.direct_addr` | host:port | — | 方案 A 直拨地址（ROUTE_HINT 源） |
| `remote.node.seeds` | [url] | [] | 种子（`scheme://node_id@host:port`，node_id@ 可省） |
| `remote.node.scheme` | string | tcp | 传输载体 tcp/mem/quic |
| `remote.transport.heartbeat_interval_ms` | u64 | 2000 | 心跳间隔（半开检测粒度） |
| `remote.transport.heartbeat_max_loss` | u32 | 5 | 丢失次数阈值（×间隔≈检出时长） |
| `remote.transport.outbound_queue` | usize | 1024 | 出站帧队列（天然反压） |
| `remote.transport.default_hop_limit` | u8 | 8 | 帧跳数上限（握手体协商） |
| `remote.reorder.gap_timeout_ms` | u64 | 250 | TELL 重排缺口等待 |
| `remote.reorder.buffer_cap` | usize | 1024 | 重排缓冲上限（防打爆） |
| `remote.codec.extra_caps` | u32 | 0 | 附加能力位（pb-only 对端叠加） |

全键带中文说明：`ParrotConfig::documented()`（运维文档数据源）+ 根目录 `parrot.toml.example`（全注释可运行样例——CFG6 用例防腐：示例即文档，默认值展示必须与真实默认一致）。

## 4. 加载管线

```
ParrotConfig::builder()
  .load_file(path)?            # 或 load_default_locations()：PARROT_CONFIG > ./parrot.toml
  ├─ 文件不存在 → debug 日志跳过（特性：零配置启动，非错误）
  ├─ 读文件 → expand_env() → toml 解析（错→ConfigError::Parse 带路径）
  ├─ merge_toml()：只填 None 槽位；未知键 warn；类型错配 warn
  └─ .thread_xxx(...)/.remote_xxx(...)   # 代码层（任意顺序——层优先，非时序）
  .build()?                    # 校验（Invalid 带键名）→ Resolved 折叠
```

**环境变量展开**：`${VAR}` / `${VAR:-default}`（K8s ConfigMap 注入风格）；残缺 `${`（无 `}` 收尾）保持原样提示书写错误，不静默吞。

## 5. 数据模型与 crate 边界（依赖倒置）

```
crates/parrot-config（新，纯数据：serde + toml + thiserror + tracing）
   ↑ 仅依赖 parrot-api（类型协议）
   ├── crates/parrot：ThreadActorSystemConfig::from_resolved(&Resolved)
   │                  ParrotActorSystem::from_config(&Resolved)（facade 一站式）
   └── crates/parrot-remote：RemoteConfig::from_resolved(&Resolved) / with_knobs()
```

- 配置 crate 零运行时依赖——不 import parrot/parrot-remote（倒置方向：运行时依赖配置）。
- 既有 API 全保留（`RemoteConfig::tcp/mem/quic` + builder 链原样可用；knobs=None 走默认）。存量调用点（crawler-lab/parrot-node/probe）零改动编译通过——向后兼容实证。

## 6. 运行时旋钮参数化（6 常量 → RuntimeKnobs）

原写死常量的参数化路径（默认=原值 → golden vectors 与既有性能零扰动）：

```rust
// parrot-remote/src/transport.rs
pub struct RuntimeKnobs {
    pub heartbeat_interval: Duration,     // 原 HEARTBEAT_INTERVAL=2s
    pub heartbeat_max_loss: u32,          // 原 HEARTBEAT_MAX_LOSS=5
    pub outbound_queue: usize,            // 原 OUTBOUND_QUEUE=1024
    pub reorder_gap_timeout_ms: u64,      // 原 ingress 250ms
    pub reorder_buffer_cap: usize,        // 原 ingress 1024
    pub default_hop_limit: u8,            // 原 DEFAULT_HOP_LIMIT=8
}
```

流转图（单一路径，无旁路）：

```
parrot.toml → ParrotConfig::build() → Resolved
  → RemoteConfig::from_resolved()：knobs = Some(RuntimeKnobs{...})
     ├─ Tcp/Quic/MemoryTransport::new(.., knobs) → ConnParams.knobs
     │    → run_connection：出站队列容量 / 心跳间隔 / 半开判定阈值（驱动循环唯一读点）
     ├─ RemoteActorSystem::new：handshake.hop_limit = knobs.default_hop_limit（握手协商）
     └─ ingress.set_reorder_knobs(gap, cap)：reorder_loop 启动时快照（任务生命周期内
        固定——避免运行中改配置引发 expected 语义漂移；下个任务自然用新值）
```

三载体（tcp/quic/mem）全穿透；mem 直连对（`connect_mem_pair`）同样携带。`RemoteConfig` 与传输实现新增 `knobs: Option<RuntimeKnobs>`——None=默认等价迁移前（测试构造点显式 `None`，语义白盒）。

## 7. 与引擎接线（三个生成入口）

| 入口 | 产出 | 覆盖节 |
|---|---|---|
| `ThreadActorSystemConfig::from_resolved(&Resolved)` | thread 引擎系统配置（含系统名=node_id） | `[thread]` |
| `RemoteConfig::from_resolved(&Resolved)` | 远程系统配置（身份/拓扑/seeds/旋钮/caps） | `[remote.*]` |
| `ParrotActorSystem::from_config(&Resolved)` | facade 一站式（本地纯引擎形态；需 spawn 的用户走 from_resolved + `shared()`——文档明示 set_self_weak 边界） | 全部 |

NodeAddr 新增 `FromStr`：`"scheme://node_id@host:port"` / `"host:port"`（缺省 tcp + node_id=hostport）——seeds 声明的解析基础。

## 8. 与应用体系（09）的衔接

- **第 0 层 overlay**（09 裁定 Y3）：`AppManifest.config_overlay > 代码 > parrot.toml > 默认`——应用声明覆盖一切（部署期已知，无运行期翻转）。本 crate 的合并函数复用于 overlay 注入点。
- **组件级配置**（09 §2.2）：`ComponentSpec.config` 经同一 `Resolved` 管道下发各引擎 Executor。
- 预留（本期不实施，架构已留位）：基于 actor 路径模式（`/user/crawler/*`）的部署级配置匹配——`Resolved` 的分节结构与合并函数天然支持多级叠加，无需破坏性变更。

## 9. 测试与验收（已交付实证）

| 层 | 用例 | 结果 |
|---|---|---|
| 单元（parrot-config） | C1 全空=全默认 / C2 文件>默认 / **C3 代码>文件（核心契约）** / C4 顺序无关 / C5 缺文件=特性 / V1-V4 校验与坏 TOML / E1 环境变量 / F1 全字段 / F2 角色解析 / D1 文档完备 | 13/13 绿 |
| doc | builder 最小用例 | 绿 |
| 场景（transport） | `heartbeat_knobs_accelerate_detection`：300ms×2≈0.6s 检出半开（对照默认 10s——旋钮未生效则 3s 超时兜底失败）；下界 ≥2×间隔-容差防计数 bug | 绿 |
| 场景（ingress） | `ro2k_gap_knob_shortens_wait`：50ms 旋钮在 ~150ms 时已放行（默认 250ms 必仍缓冲——时序即证明） | 绿 |
| 集成（CFG1-6） | CFG1 TOML 驱动真实星型拓扑（hub/spoke 全参数外置+真实 ask 回程）/ CFG2 心跳旋钮流入 / CFG3 hop 握手协商 / CFG4 thread 域映射 / CFG5 facade 入口 / CFG6 示例文件防腐 | 6/6 绿 |
| 回归 | parrot-remote 180/180（含既有 `heartbeat_half_open_detected` 默认路径）· workspace 全量 0 失败 · clippy 清零 · golden vectors 字节不变 | 全绿 |

## 10. 边界与诚实声明

1. **运行期不可变**：配置在启动期装配后固定（reorder 旋钮任务级快照是同一原则的微观体现）。动态重载（SIGHUP 热更）不做——热更语义（哪些键可变、变更如何传播到已建连接）复杂度不匹配当前需求；未来如需，走 Orchestrator（09）的组件升级通道而非旁路。
2. **无 include/分层文件**（HOCON 的 `include` 类能力）：单文件即全部。多环境用 `PARROT_CONFIG` 指向不同文件 + 环境变量展开组合，已覆盖 K8s/Docker 场景。
3. **未知键 warn 不拒绝**：前向兼容优先（老配置跑新版本不炸）；拼错键有 warn 日志可查。严格模式（deny）如需可加 feature 开关——默认宽松。
4. **文件缺失是特性**：零配置启动=迁移前行为（设计目标 3 的直接体现），仅 debug 日志。
