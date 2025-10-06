# TECH_DESIGN_09 · 应用体系与多引擎统一编排设计（App / Extension / Job / Debug）

> 状态：**设计定稿（评审输入）** · 2026-10-06
> 上游：[01 架构原则](./TECH_DESIGN_01_架构与设计原则.md)（双引擎+facade）· [04 远程集群](./TECH_DESIGN_04_远程与集群架构.md)（六层模型）· [07 联邦协议 1.0](./TECH_DESIGN_07_异构联邦协议设计.md)（Wire/拓扑/Directory/X12 自举裁定）· 配置切面（`parrot-config`，2026-10-05 交付）
> 实证基线：crawler-lab 四运行时集成（Rust hub + Erlang/Ray/JVM 网关全互通）· workspace 1740+ 测试全绿 · 五语言 Wire 1.0 实现齐备
> 本文回答的三个问题 + 两项要求（已与需求方逐项确认，见 §10 决策记录）：
> 1. 引擎扩展（二开/插件/扩展库的加载、载入、卸载）
> 2. 框架与应用分离（Spring 与 Spring 应用式关系）
> 3. 引擎上的应用/任务加载（Spark/Flink 式）——**且为多引擎并存统一编排**（parrot/akka/ray/erlang 预部署，一个 App 跨引擎，统一提交/部署/管理/升级，对引擎差异无感透明）
> 4. 调试与测试（大型跨引擎任务的调试）
> 5. 未来形态预演（library / 编程语言 VM / runtime——预研可行性，不在现架构留坑）

---

## 0. 术语与总览

| 术语 | 定义 | 对应既有概念 |
|---|---|---|
| **AppManifest** | 一个应用的完整描述：跨引擎组件图 + 连接 + 升级策略（纯数据） | Spring 的 `@Configuration`；Spark 的作业 jar+描述 |
| **Component** | App 的一个组件：归属某引擎、有部署产物、有实例策略 | actor 拓扑的一个子图；Flink 的算子链 |
| **EngineAdapter** | 统一部署协议在各引擎的方言实现（每网关一个） | 现有 `AdminCommand::SpawnLocal` 的推广 |
| **Orchestrator** | 控制面：desired ↔ observed 调和、升级状态机（parrot actor 自举） | k8s controller；Spark Master；**不引入外部服务** |
| **ArtifactRef** | 组件部署产物的引用：Props / Wasm / Dylib / Jvm / PyModule / Beam | — |

三问一链（依赖关系决定实施顺序）：

```
问题3（多引擎统一加载/升级） ← 顶层
   └─ 问题2（应用=Manifest 描述符，宿主=Runner/Orchestrator） ← 中层
        └─ 问题1（引擎扩展三形态：编译期/wasm/dylib/进程外） ← 底层
调试体系横切全部（§8）          未来预演约束全部（§9）
```

---

## 1. 现状资产盘点（本设计的事实基础——全部已在 master 实证）

| 资产 | 位置 | 现状 | 本设计如何复用 |
|---|---|---|---|
| 双引擎统一 facade | `crates/parrot/src/system.rs` | thread + actix 并存，`BoxedActorRef` 位置透明（X1-X8 实证） | 多引擎统一在**进程内**的既有范本；本设计将其推广到**跨进程跨语言** |
| Wire 1.0 + 五语言网关 | `crates/parrot-remote/src/frame.rs`；`interop/{jvm,python,erlang,cpp-lite,typescript-lite}` | akka(Scala/Netty)/ray(Python)/erlang(OTP)/TS/C++ 全部实现握手/编解码/ask/tell | **联邦数据面已通**。本设计补的是**控制面**（部署/升级/监管），不新造连接 |
| K0 admin 远程 spawn | `crates/parrot-remote/src/admin.rs`（`AdminCommand::SpawnLocal` + `PropsFactory` inventory + `AdminPending` 回执表） | 编译期工厂按名 spawn，带回执 | **统一部署协议的 parrot 方言雏形**；§5 扩展为 admin-v2 |
| parrot-node 标准进程 | `crates/parrot-node/` | env 配置、`PARROT_NODE_READY` 就绪协议、TCP healthcheck、内置 actor | **部署单元（Executor）现成形态**；§6 增强 artifact 接收能力 |
| 集群基建 | `swim.rs`/`receptionist.rs`/`sharding.rs`/`singleton.rs`/`raft/`/`topology.rs`/`durable.rs`（WAL） | SWIM gossip、分片、单例、Raft、拓扑三模式、durable tell、心跳半开检测 | **Orchestrator 的一致性/容灾全部自产**：AppSupervisor 用 Singleton+Raft，升级 drain 用 durable WAL |
| federation-lab 孪生 | `tools/federation-lab/`（composegen + twin） | 50 集群 compose 生成 + 百万节点数字孪生 | 调试体系（§8）的混沌门禁底座 |
| 帧级全链追踪 | `frame.rs trace_line`（TX/RX）+ cid 关联 + 重排/迟到/丢弃计数 | debug 开关输出全帧轨迹 | trace span 树与 record/replay 的数据源 |
| 配置切面 | `crates/parrot-config/` | 三层合并（代码>文件>默认）+ `parrot.toml` | AppManifest 的配置绑定层 |

**核心判断**：需求方要的"一个 App 跨 parrot/akka/ray/erlang 统一提交部署管理升级" = **把 crawler-lab 里手写在 `main.rs` 的跨引擎编排，升级为声明式 Manifest + 常驻控制面 Orchestrator + 各引擎 EngineAdapter**。数据面已通，本设计交付控制面。

---

## 2. 问题2 · 框架与应用分离：AppManifest + 宿主

### 2.1 概念对齐（Spring ↔ Parrot）

| Spring | Parrot | 落点 |
|---|---|---|
| `SpringApplication.run()` | `parrot app run`（本地）/ `parrot app deploy`（集群） | 宿主引导 |
| `@Bean`/`@Component` | `ComponentSpec` | 声明式组件 |
| `application.properties` | `parrot.toml`（配置切面已交付） | 三层优先级 |
| `@ConfigurationProperties` | `ComponentSpec::config` → `parrot-config::Resolved` | 强类型绑定 |
| `ApplicationListener` | `ComponentHook`（on_start/on_stop/drain） | 生命周期 |
| IoC 容器 | `AssemblingContext`（装配器，按依赖拓扑排序注入） | 依赖注入 |
| Actuator | Orchestrator 的 observed state + admin 查询 | 运维面 |

### 2.2 Manifest 数据模型（新 crate `parrot-app`，只依赖 parrot-api + serde + parrot-config）

```rust
// crates/parrot-app/src/manifest.rs
pub struct AppManifest {
    pub name: String,                      // "crawler"
    pub version: semver::Version,          // 应用级版本（升级单位）
    pub components: Vec<ComponentSpec>,
    pub wiring: Vec<WireSpec>,             // 静态路径绑定（启动期解析校验）
    pub config_overlay: Option<toml::value::Table>, // 并入 parrot-config 文件层之前
}

pub struct ComponentSpec {
    pub name: String,                      // "frontier"
    pub engine: EngineKind,                // Parrot | Akka | Ray | Erlang | LiteTs | LiteCpp
    pub artifact: ArtifactRef,             // §4 三形态 + 各引擎原生
    pub instances: InstancePolicy,         // Singleton | Sharded{n} | Pool{n} | Ephemeral
    pub placement: PlacementConstraint,    // 拓扑角色/标签/Directory 选域（复用 topology.rs）
    pub upgrade: UpgradePolicy,            // §7 三策略
    pub deps: Vec<String>,                 // 启动依赖（DAG，Planner 拓扑排序）
    pub config: Option<toml::value::Table>,// 组件级配置 → parrot-config 合并
    pub hooks: ComponentHooks,             // on_start/on_stop/drain（健康与优雅升级的钩子面）
}

pub enum ArtifactRef {
    Props { factory: String },                          // parrot：编译期工厂名（inventory）
    Wasm { digest: String, uri: String },               // parrot：wasm component（wasmtime）
    Dylib { digest: String, uri: String, abi: u32 },    // parrot：动态库（§4.3 高质量卸载协议）
    Jvm { main_class: String, coords: Option<String> }, // akka 网关
    PyModule { module: String, runtime_env: Option<toml::Value> }, // ray 网关
    Beam { app: String },                                // erlang 网关
}

pub struct WireSpec {                    // 组件间静态连接
    pub from: ActorPathPattern,          // "frontier:/user/next"（组件内路径）
    pub to: ActorPathPattern,            // "index:/user/build"
    pub qos: QoS,                        // 复用 topology.rs::QoS
}
```

**分层铁律**（与 07 §14.6 E5.2 一致）：

```
用户应用包    → 只依赖 parrot-api + parrot-app（Manifest 是纯数据，可单测/diff/静态扫描）
宿主（Runner/Orchestrator）→ 依赖引擎 + 配置切面 + admin-v2 + 扩展加载器
引擎          → 不知道任何具体应用
```

Manifest 双形态：Rust 类型（程序化构造/测试断言）+ TOML 文件（`app.toml`，运维可读可 diff）。两者 serde 互转，单一事实源是 Rust 类型。

### 2.3 本地宿主 `parrot app run`（同构调试的基石）

```
parrot app run --manifest app.toml [--profile local]
  ① parrot-config 装配（overlay > parrot.toml > 默认）
  ② Planner：组件依赖 DAG 拓扑排序 → 逐组件装配
  ③ 本地模式下各引擎形态：
     - Parrot 组件 → 内嵌 ThreadActorSystem spawn（Props/Wasm/Dylib 三形态全可本地加载）
     - Akka/Ray/Erlang 组件 → 内嵌网关（JVM: jvm crate / Python: 嵌入 or 子进程 / Erlang: 子进程）
       全部走 memory transport 对（跨语言边界仍走真实 Wire 编解码——**序列化路径与生产一致**）
  ④ wiring 解析 → 校验所有 ActorPathPattern 可达 → 起消息流
  ⑤ Ctrl-C/SIGTERM → hooks.on_stop 逆依赖序 → drain → 引擎 shutdown
```

**同构性保证**：本地/集群的装配链路是同一份代码（`AssemblingContext`）；差别只在 `LocalDeployer` vs `ClusterDeployer`（§6）。与 Spark `local[*]` ↔ yarn 同构。调试断点可打在 Rust/JVM/Python/Erlang 任何一侧（单进程或本机子进程组）。

---

## 3. 问题3 · 多引擎统一编排：Orchestrator（内嵌控制面，X12 裁定）

### 3.1 拓扑位置

```
┌────────────────────────── parrot 集群（预部署）──────────────────────────┐
│  Orchestrator（常驻 actor 群，Singleton+Raft 保护——复用 singleton.rs/raft/）│
│   ├── AppSupervisor（per-app 一个；desired=Manifest ↔ observed=健康表）      │
│   ├── Planner（依赖 DAG + placement——读 Directory/SWIM 拓扑）               │
│   ├── HealthWatch（心跳半开 + SWIM 故障 → 组件失联事件——复用现有检测）      │
│   └── RolloutTracker（升级状态机实例——§7）                                  │
│  EngineAdapter（每网关一个 admin 通道，§5）                                  │
└──────────────────────────────────────────────────────────────────────────┘
     ▲ admin-v2（SYSTEM_EVENT 帧）        ▲ admin-v2              ▲ admin-v2
 ┌───┴────────┐  ┌──────────┴──────┐  ┌──┴───────────┐  ┌────────┴────────┐
 │ parrot-node │  │ akka 网关(JVM)  │  │ ray 网关(py) │  │ erlang 网关(OTP)│
 │ (Executor)  │  │ (Executor)     │  │ (Executor)   │  │ (Executor)     │
└──────────────┘  └────────────────┘  └──────────────┘  └─────────────────┘
```

### 3.2 提交与调和（k8s 式，零外部依赖）

```
$ parrot app deploy --manifest app.toml --target parrot://cluster-gw:9800

AppSupervisor 调和循环（每 app）：
  desired（Manifest） ──diff──> observed（组件实例表：Running/Draining/Failed/…）
    ├── 组件缺失 → Planner 选址 → admin-v2 DeployComponent → 目标 Executor
    ├── 组件多余（版本旧）→ §7 升级状态机
    ├── 组件失联（HealthWatch 事件）→ 按实例策略重启/重调度（复用 Sharding rebalance）
    └── 幂等：所有 admin-v2 命令带 req_id + 组件版本，Executor 侧去重
```

**desired state 持久化**：Manifest 存 Raft 日志（复用自家 raft）；Executor 重启/网关重连后 AppSupervisor 重新对账（调和天然收敛，无需两阶段提交）。

### 3.3 Executor（部署单元）

- **parrot**：`parrot-node` 增强——启动时向集群声明 `artifacts: [props, wasm, dylib]` 能力位（握手 TLV 新增 capability）；接收 `DeployComponent` → 按 artifact 形态加载（§4）→ spawn → 回执。
- **akka/ray/erlang**：各自网关进程 = Executor。网关重启语义见 §5.2 `reload_semantics`。

### 3.4 "自动、无感、透明"的兑现清单（需求原话逐项）

| 需求原话 | 机制 | 章节 |
|---|---|---|
| 自动根据引擎类型提交 | Manifest 声明 `engine` 字段 → Planner 路由到对应 Executor 的 adapter | §5 |
| 无感 | 应用侧只见 ActorRef（facade 三级路由 + Wire 寻址）；组件间通信与引擎无关 | §2.2 wiring |
| 透明 | `parrot app status/trace/topology` 输出跨引擎统一视图（cid 贯穿） | §8 |
| 引擎重启自动处理 | adapter `reload_semantics` 声明 → Orchestrator 编排 drain→restart→reconcile | §5.2 |
| 任务更新 | 三级升级策略 + 状态机 + 回滚 | §7 |
| 统一维护（不分散管理） | App = 一等公民：所有 CLI/admin 操作以 app 为单位，跨引擎组件聚合展示 | §3.2/§8 |

---

## 4. 问题1 · 引擎扩展三形态（决策 D1：全要，高质量+充分测试）

### 4.1 形态矩阵与分流规则

| 形态 | 机制 | 加载 | 卸载 | 隔离 | 延迟 | 定位 |
|---|---|---|---|---|---|---|
| **L1 Props** | inventory 编译期注册（**现有** PropsFactory） | 编译期 | 无（随进程） | 无 | 零 | 引擎内置组件、热路径 |
| **L2 Wasm** | wasmtime + WIT Component Model | 运行期 ms 级 | **真卸载**（instance drop） | **沙箱**：fuel/epoch 限额、内存隔离、能力制 | ~1-5µs/调用 | 用户可部署产物、第三方扩展、**未来语言前端**（§9） |
| **L3 Dylib** | `libloading` + 自定稳定 ABI 边界 | 运行期 | **协议化安全卸载**（§4.3） | 进程内（panic guard + ABI 校验） | 近零（间接调用） | 高性能运行期扩展（可信来源） |
| **L4 进程外** | Wire 网关（现有五语言） | 连接注册 | 硬卸载（进程退出） | 进程级 | ~百µs | 异构引擎、存量系统 |

分流：`[extensions] mode` 配置节 + Manifest artifact 类型即分流声明。默认信任策略：L1/L3 需运维显式启用（digest 校验强制），L2 对第三方开放，L4 对一切开放。

### 4.2 L2 Wasm：WIT 世界与宿主集成

```wit
package parrot:component@0.1.0;

interface ctx {                    // 宿主提供给组件的能力（能力制——最小权限）
  use parrot:types@0.1.0.{actor-path};
  self-ref: func() -> actor-path;
  log: func(level: u8, msg: string);
  config-get: func(key: string) -> option<string>;   // 只读 parrot-config 投影视图
  clock-now-ms: func() -> u64;
}

interface handler {
  record msg { type-key: string, payload: list<u8> }
  handle: func(m: msg) -> result<list<u8>, string>;  // ask
  tell: func(m: msg);                                // 单向（无返回）
  on-start: func();
  on-drain: func();                                   // 升级 drain 钩子（§7）
}

world parrot-component {
  import ctx;
  export handler;
}
```

宿主集成（`parrot-wasm` crate，依赖 wasmtime + wasmtime-component-bindgen）：

- **编解码边界**：组件消息即 Wire 的 type_key + bytes——与现有 codec_registry 完全同构，组件内部用什么序列化自由（serde_json/cbor 均可）
- **调度映射**：wasm 调用发生在 actor 的消息处理内 → 天然串行语义；epoch deadline 设为 thread 引擎调度片预算（fuel 上限可配，超额 → `ActorError::OverQuota` → 监督策略接管——与 native actor 同一套监督）
- **限制（诚实声明）**：组件不能直接持有 ActorRef 跨调用（句柄表制：u32 id ↔ 宿主 WeakActorTarget）；不能自起线程；网络/文件经由 ctx 能力（1.0 只给 log/config/clock，网络能力 P2 再议）

### 4.3 L3 Dylib：高质量卸载协议（难点全解）

Rust 无稳定 ABI → 边界纪律 + 生命周期协议，两者都是硬约束入 CI：

**① ABI 边界（`parrot-abi` crate，双方共同依赖）**

```rust
// 唯一导出形态：extern "C" + 显式布局——禁止跨边界 Rust 类型
#[repr(C)]
pub struct AbiMeta {
    pub abi_version: u32,           // PARROT_ABI_VERSION，加载时强校验
    pub parrot_min: u32,            // 兼容的最低宿主版本
    pub name: *const u8, pub name_len: u32,
    pub hooks: AbiHooks,
}

#[repr(C)]
pub struct AbiHooks {
    pub construct: extern "C" fn(cfg: AbiStr, out: *mut *mut DynComponentVt) -> AbiResult,
    pub destroy: extern "C" fn(*mut DynComponentVt),
}

#[repr(C)]
pub struct DynComponentVt {
    pub handle_msg: extern "C" fn(self_: *mut (), msg: AbiMsg, out: *mut AbiReply) -> AbiResult,
    pub on_drain: extern "C" fn(self_: *mut ()) -> AbiResult,
}
```

- 消息边界也是 bytes（`AbiMsg{type_key, payload}`）——与 wasm/网络路径同构，**三种形态共用一套消息契约测试**
- panic 边界：dylib 侧 `catch_unwind` 到错误码；宿主侧对 `handle_msg` 再包一层防御（双保险，文档明示边界两侧责任）
- 禁止清单（CI 扫描 dylib 产物）：不注册 TLS 析构、不 spawn 自有线程持有组件内函数指针、不 install signal handler。violation = 加载拒绝。

**② 安全卸载四步协议（`UnloadProtocol`）**

```
1. Quarantine：从路由表摘除（新消息不再进）→ 组件实例进入 draining
2. Drain：等 in-flight 消息归零（per-instance 计数，上限 = drain_timeout，
   超时按监督策略 Restart 语义处理未完成消息——记入 DRAIN_ABORTED 计数器）
3. Destroy：逐实例 destroy()（dylib 侧清引用）→ 等宿主侧组件句柄表清空
   → 等 tokio 任务栅栏（所有曾进入该库代码的任务已退出/迁移）
4. Dlclose：引用计数归零后 dlclose。验证：卸载后立刻 dlopen 同名新版本
   （升级主路径）+ 全局状态无污染断言（测试 §11 T-Ext 套件）
```

- **诚实边界**：若 dylib 违反禁止清单（如泄漏代码指针），第 3 步栅栏可能不归零 → `drain_timeout` 后强制 dlclose 并告警（接受潜在 UB 风险，运维侧 digest 审计兜底）。此风险在文档与加载时警告中双重明示；规范产物（走 parrot-abi 模板编译）不会触犯。
- 重复符号/版本共存：同一 dylib 升级用 **soname 版本化**（`libcomp-frontier-2.so`），新旧短暂共存支持 Rolling（§7）。

### 4.4 L4 进程外（现有网关，一段收束）

进程外扩展 = 网关进程注册的 actor 前缀。加载/卸载 = 连接建立/断开（心跳半开检测 3s 级摘除路由）；失败半径 = 进程边界。无新增机制，纳入 `extensions` 配置统一声明即可。

---

## 5. EngineAdapter：admin-v2 统一部署协议（决策 D3：四引擎一步到位）

### 5.1 协议（AdminCommand v2 扩展，仍走 SYSTEM_EVENT 帧 + AdminPending 回执）

```rust
pub enum AdminCommandV2 {
    DeployComponent { req_id, component: ComponentDeploy },  // 部署/替换
    DrainComponent  { req_id, path_prefix, timeout_ms },     // 升级前置
    StopComponent   { req_id, path_prefix },
    ComponentStatus { req_id, path_prefix },                 // observed state 上报
}
pub struct ComponentDeploy {
    pub name: String, pub version: semver::Version,
    pub artifact: ArtifactRef,           // 各引擎方言解释
    pub instances: InstancePolicy,
    pub config: Option<bytes::Bytes>,    // toml 片段
}
```

### 5.2 四方言实现与 reload 语义（诚实声明表——编入 Executor 能力位）

| 引擎 | adapter 实现（落点） | 部署原语 | `reload_semantics` | 升级/卸载边界（诚实） |
|---|---|---|---|---|
| **parrot** | `admin.rs` 扩展 + `parrot-node` artifact 通道 | Props spawn / wasmtime instantiate / libloading load | **HotSwap**（三形态皆可原地换实例） | wasm 真卸载；dylib 走 §4.3 协议 |
| **akka** | `ParrotGatewayMain` 加 AdminPort（Netty 同端口） | child-first URLClassLoader 加载 app jar → 网关 ActorSystem 内 spawn | **HotSwap（classloader 级）** / 可选 GatewayRestart | JVM classloader 卸载受 GC 限制（与 Spark executor 同边界）——软卸载 + drain 后强制重启网关选项 |
| **ray** | `ray_gw.py` admin handler → **ray job API**（`ray.job_submission`，ray 原生分发 working_dir/runtime_env） | named actor 起 `ParrotDispatcher` | **JobUpdate**（ray 原生） | ray kill actors 即时；runtime_env 变更 = 作业重提（Orchestrator 编排） |
| **erlang** | `parrot_gw.erl` admin → OTP `code:load_abs/1` + supervisor child 重启 | **真热替换**（四引擎唯一：`code:purge/1` + `load_mod`） | **HotSwap（模块级，原生）** | OTP 语义，gen_server 状态延续（upgrade 注解态） |

**"引擎重启才能加载"统一编排**：adapter 能力位声明 `reload_semantics` → Orchestrator 对 GatewayRestart/NodeRestart 级变更执行：`drain（durable tell WAL 边缘兜底）→ restart（就绪协议 `PARROT_NODE_READY` / 网关 stdout port 行）→ reconcile（admin-v2 ComponentStatus 对账）`。应用与用户无感——desired state 在 Orchestrator 手里，重启只是 observed 抖动。

### 5.3 Placement

复用 `topology.rs` 拓扑三模式 + Directory：`PlacementConstraint{ role: Option<TopologyRole>, realm: Option<String>, label: Option<String>, anti_affinity: bool }`。Planner 生成候选 → Executor 能力/负载过滤（SWIM 元数据）。

---

## 6. 升级（问题3 的"任务更新怎么做"）：三级策略 + 状态机 + 回滚

### 6.1 策略（per-component `UpgradePolicy`）

1. **HotSwap**：drain 旧实例（hooks.on_drain + durable WAL 兜底断连不丢）→ 新实例起 → **路由原子切**（复用 ROUTE_HINT/重拨/重排 seq 机制——切换瞬间保序已有实现）→ 旧实例停。适用：erlang 模块 / parrot wasm / ray job update / akka classloader。
2. **Rolling**（Pool/Sharded 组件）：按分片逐个 HotSwap；新旧共存期路由表带版本向量（Wire version 字段防跨版本消息错配——网关方言已留位）。
3. **Recreate**（破坏性变更）：全停 → 状态迁移（版本化快照，格式复用 durable WAL record）→ 全起。回滚 = 加载上一代快照。

### 6.2 RolloutTracker 状态机（Raft 日志驱动，可观测）

```
Pending → Planning → Draining → Deploying → Verifying(健康探针+回声 ask)
       → Switching(路由原子切) → Running
任何阶段失败 → Rollback（状态快照 N 代保留，drain 中的消息由 WAL 重放）
```

`parrot app rollout <app>` 输出状态机轨迹（per-component 时间线）。

---

## 7. 调试与测试体系（决策 D4：五能力全要）

| # | 能力 | 机制 | 复用底座 |
|---|---|---|---|
| D1 | **本地同构调试** | `parrot app run`：四引擎组件进程内/本机组装，断点直打任何一侧；序列化路径与生产一致（Wire 编解码不因本地跳过） | §2.3 AssemblingContext + mem transport |
| D2 | **全链 trace** | cid 贯穿扩展为 span 树：Rust tracing ↔ JVM/Python/Erlang 网关统一 `trace_id`（admin-v2 通道透传 OTLP 语义）；`parrot app trace <app> --follow` 实时 per-message 时间线 | frame trace_line + OpenTelemetry 兼容导出 |
| D3 | **镜像调试** | `debug=mirror(frontier)`：Orchestrator 向目标组件插镜像策略 → 消息流双写本地镜像 actor（只读副本），断点/慢放/状态检查零生产干扰 | facade 路由层拦截 + Wire 双发 |
| D4 | **孪生混沌门禁** | AppManifest 输入 federation-lab 孪生 → 应用图整体在孪生跑升级状态机全部分支 + 6 故障注入器 → CI 门禁 | DEV_00 三轴仿真 + composegen/twin |
| D5 | **Record/Replay** | 全帧 trace 落盘（cid 重组消息序列）→ 回放器按序重放至组件（含跨引擎段）；线上 bug → 确定性回归用例 | golden vectors 思路升级（帧级→应用级）+ durable WAL 格式 |

测试义务（§11 汇总）:每能力至少 单元+场景+集成 三层，全量回归并入 `make test-full`。

---

## 8. 未来预演（问题4：library / 语言 VM——不实施，架构不留坑）

### 8.1 Library 形态（已基本成立，补一条嵌入纪律）

- 现状：workspace 核心 crates 无进程假设；`NODE` singleton 仅在 parrot-node 进程壳。
- 嵌入纪律（入 CI 检查）：核心 crate（parrot-api/parrot/parrot-remote/parrot-config/parrot-app）**禁止**：全局可变 static、signal handler 注册、main 语义、自起后台线程（调度线程由显式 API 起）。检查方式：`cargo-llvm-lines` 无异常 + rg 扫描清单 + 文档声明。
- `parrot-node` 定位 = 官方服务器打包；嵌入者直接组 crates（crawler-lab 已是此形态的活例）。

### 8.2 语言 VM 形态（预研结论：可行，wasm 是路径）

"Parrot 语言运行时" = **WIT 系统接口（actor/mailbox/寻址暴露为 host functions）+ wasmtime 实例池 + fuel 抢占映射调度器 + Wire 寻址**。任何可编译 wasm component 的语言（Rust/Go/JS/Python-GC/…）即获得 parrot 运行时。

预研验证点（后续专项，不影响当前架构）：
1. wasmtime instance pooling 延迟上界（目标 <10µs 实例化）
2. wasm-gc 语言组件成熟度（Kotlin/GraalWasm/Dart 跟踪）
3. epoch deadline 与 tokio runtime 交互（协作抢占边界）
4. WIT 类型演进的兼容策略（resource 类型版本化）

**不留坑的架构保证（本期落实）**：§4.2 WIT 世界一旦发布即视为公共 API 冻结管理（semver）；组件消息契约（type_key+bytes）与网络契约同构——未来 VM 上跑的组件与今天的 wasm 组件是同一产物，零迁移。

---

## 9. 与既有体系的冲突消解

| # | 潜在冲突 | 裁定 |
|---|---|---|
| Y1 | admin-v2 vs K0 admin 协议 | 超集扩展：v1 命令码保留，v2 新增命令独立码点；老节点对 v2 命令回 `Unsupported`（能力位预判，不发即不问） |
| Y2 | Orchestrator vs federation-lab composegen | composegen 继续管**基础设施**（容器/进程/网络）；Orchestrator 管**应用**（Manifest 组件）。实验室场景两者叠加：compose 起集群，Orchestrator 部署 app |
| Y3 | AppManifest 配置 vs parrot-config 三层 | 优先级第 0 层：`app config_overlay > 代码 > parrot.toml > 默认`（应用声明覆盖一切——部署期已知，无运行期翻转） |
| Y4 | wiring 静态绑定 vs Receptionist 动态发现 | 互补：wiring = 编排期拓扑（可校验/可 diff）；Receptionist = 运行期弹性发现（服务内多实例）。Manifest 内组件间用 wiring，组件内部实现自由 |
| Y5 | 三形态产物 vs 性能铁律（热路径零损） | L1 永远是热路径默认；L2/L3 仅经显式 artifact 声明进入；L4 不进数据面热路径（网关跳已在预算内）。性能门禁含三形态分别的基准（§11） |
| Y6 | dylib 卸载 UB 风险 vs 工业级要求 | 风险白盒化：禁止清单 CI 扫描 + 加载时审计告警 + drain_timeout 强制兜底；规范产物路径（parrot-abi 模板）风险为零。文档双明示 |

---

## 10. 决策记录（2026-10-06 与需求方确认）

| # | 决策 | 结论 | 理由摘要 |
|---|---|---|---|
| D1 | 运行期产物形态 | **Props + Wasm + Dylib 全部支持**（进程外网关已有）；高质量实现+充分测试 | 热路径零损（Props）、安全可卸载与语言 VM 支点（Wasm）、高性能运行期扩展（Dylib，走 §4.3 协议化卸载） |
| D2 | Orchestrator 形态 | **集群内嵌**（parrot actor 自举：Singleton+Raft+durable） | X12 裁定一致；零外部依赖；自产容灾基建齐备 |
| D3 | 首期多引擎范围 | **四引擎一步到位**（parrot/akka/ray/erlang）；crawler-lab 改造为首个多引擎 App 验收场景 | 数据面已通（crawler-lab 实证），控制面四方言工作量可控且互为校验 |
| D4 | 调试套件 | **五能力全要**（app run/trace/镜像/孪生/record-replay），各配三层测试 | 大型跨引擎任务调试是刚需；底座（cid/trace_line/孪生/WAL）全部现成 |

---

## 11. 落地路线图与测试矩阵

> §11.2 是全量测试计划。**三份上位文档共同约束**：[DEV_00 §1](./DEV_00_总体测试与验收计划.md)（L0-L4 执行矩阵/频次/门禁）· [DEV_00 §4](./DEV_00_总体测试与验收计划.md)（验收五步流程）· [03 §质量分析](./TECH_DESIGN_03_质量分析与改进路线.md)（覆盖率方法与防退化）。本节把三者具体化到应用体系，并升级两条门禁：**代码覆盖率 100%（可测性设计达到，不可达项白名单报批）**；**多引擎测试标准对齐既有五语言矩阵（L2 vectors + L3 混沌 + L4 门禁值）**。

### 11.1 阶段切分（每阶段：DEV 文档先行 → 实现 → 四层测试 → 全量回归）

| 阶段 | 内容 | 交付物 | 依赖 |
|---|---|---|---|
| **A. parrot-app** | Manifest 模型 + Planner + AssemblingContext + `parrot app run`（本地四引擎组装）+ TOML 双形态 + 校验测试 | crate `parrot-app` + CLI | 无 |
| **B. admin-v2** | 协议扩展 + parrot Executor（artifact 通道）+ ray job API 方言 + erlang 热加载方言 + akka child-loader 方言 | 四网关 admin 通道 + `parrot-node` 增强 | A |
| **C. wasm** | `parrot-wasm`（wasmtime+WIT 绑定）+ fuel/epoch 映射 + 沙箱能力制 | `DeployComponent{Wasm}` 全链 | B |
| **D. dylib** | `parrot-abi` + 加载器 + 卸载四步协议 + 禁止清单 CI 扫描 | `DeployComponent{Dylib}` 全链 | B（与 C 并行） |
| **E. orchestrator** | AppSupervisor/HealthWatch/RolloutTracker + 调和循环 + desired 持久化(Raft) | `parrot app deploy/status/rollout` | B |
| **F. debug 套件** | trace span 树 / 镜像 actor / record-replay / 孪生门禁接入 | 五能力 CLI + CI 门禁 | E（D3/D5 部分可提前） |
| **G. 验收** | crawler-lab 改造为 App（跨四引擎）+ 全量回归 + 性能门禁 | DEV_09 DoD 核销 | 全部 |

### 11.2 测试计划（全量——按既有五引擎矩阵标准制定；DEV_09 展开为逐函数测试义务）

#### 11.2.0 覆盖率铁律（100% 承诺的实现机制）

- **门禁**：新增 crate（`parrot-app`/`parrot-wasm`/`parrot-abi`/orchestrator 模块）行覆盖 **100%**，分支覆盖 ≥95%；workspace 总量不低于现状（91.1%+）且只升不降。
- **可达性方法论**（继承 remote 层 91.1% 实战经验）：① 一切外部副作用（wasmtime/libloading/网关进程/ray API）走 trait 边界注入，测试替身可达全分支；② 状态机穷举（升级/卸载/调和的每个转移弧显式用例）；③ 错误注入点显式建模（`DeployError`/`UnloadError` 枚举驱动）；④ 生产代码不允许 `#[cfg(test)]` 分支（可测性靠设计不靠宏）。
- **白名单制度**：真不可测项（dlclose 后 UB 探测、JVM GC 触发 classloader 回收等）逐项列入 `docs/coverage-waiver.md`，写明不可测原因 + 替代验证手段（孪生/混沌/文档断言），**需求方签批**后计入豁免。白名单条目上限 10，超限视为设计缺陷返工。

#### 11.2.1 L0/L1 单元测试（每提交，CI ≤10min，阻塞合并）

| 模块 | 用例族 | 数量级 |
|---|---|---|
| Manifest | 校验全分支：DAG 环拒绝/wiring 不可达/semver 非法/组件重名/engine 未知/artifact 与 engine 不匹配/空组件表/TOML↔Rust roundtrip 字节一致 | 25+ |
| Planner | 拓扑排序稳定性（同输入同序）/依赖缺失报错定位精确/placement 过滤全匹配全不匹配/并发 planner 幂等 | 15+ |
| AssemblingContext | 依赖序装配/装配失败回滚（已起组件逆序停）/config_overlay 合并优先级（Y3 裁定）/hooks 时序断言 | 20+ |
| admin-v2 协议 | 编解码 roundtrip/req_id 去重/超时回执/旧节点 Unsupported 能力位预判（Y1）/四方言同一 ComponentDeploy 向量输出一致（golden 化） | 20+ |
| parrot-wasm | WIT 绑定 roundtrip/fuel 耗尽→OverQuota→监督接管/epoch 抢占/句柄表越界拒绝/组件 panic→错误码/沙箱能力制（无权限 ctx 调用被拒）/实例 drop 内存回收断言 | 30+ |
| parrot-abi/dylib | ABI 版本不匹配拒绝/parrot_min 校验/panic 双边界（dylib 侧 catch + 宿主侧防御）/四步卸载协议每步状态断言/drain 超时强制路径/重载后全局状态无污染/soname 新旧共存 | 35+ |
| Orchestrator | 调和 diff 全形态（缺/多/版本旧/失联/重启）/状态机全转移弧（含 Rollback 每个入口）/desired 持久化重放/幂等（同命令 N 次=1 次效果） | 40+ |
| Executor | artifact 三形态分发校验（digest 不符拒绝）/能力位上报/就绪协议兼容 | 15+ |

#### 11.2.2 L2 场景 + 跨引擎集成（每 PR，CI docker ≤25min，阻塞合并）

**单引擎内场景（三形态产物各一套，共 9 套）**：加载→ask/tell→drain→卸载→重载→内存无泄漏（进程 RSS 断言）→异常注入（加载中 kill/卸载中来消息）。

**多引擎集成（对齐 crawler-lab / 五语言 vectors 标准）**：

| 用例族 | 内容 | 门禁 |
|---|---|---|
| MG1-4 | 单 App 跨 parrot+akka+ray+erlang：deploy→组件全 Running→跨引擎 ask/tell 全链→status 聚合视图 | 全通 |
| MG5-8 | 升级三策略各跨引擎一场：HotSwap（erlang 模块热换+路由原子切+seq 保序断言）/Rolling（分片逐换+新旧共存期消息版本校验）/Recreate（状态迁移+回滚） | 切换零丢帧（durable WAL 断言） |
| MG9-10 | 引擎重启无感：akka 网关 GatewayRestart（drain→restart→reconcile 全自动，应用侧零错误）/parrot-node 重启同构 | 重连窗口内 ask 失败率=0（排队重试） |
| MG11 | admin-v2 四方言契约：同一 Manifest 依次部署到四引擎，ComponentStatus 输出结构一致（golden） | 字节级一致 |
| MG12 | 本地/集群同构：`app run` 与 `app deploy` 同一 Manifest，消息轨迹（cid 序列+payload hash）一致 | 序列一致 |
| Vectors | admin-v2 帧 + WIT 编解码 golden vectors 四语言各自跑（rust/jvm/py/erl） | 冻结不变 |

#### 11.2.3 L3 混沌（每夜，独立 runner ≤60min，报警+阻塞 release）

复用 DEV_00 六注入器，注入对象升级为 App 级：

| 场景 | 断言 |
|---|---|
| 升级中 kill -9 目标 Executor | RolloutTracker 自动 Rollback，App 恢复 Running，WAL 重放零丢 |
| Orchestrator 多数派分区 | 少数派不下发任何部署（Raft 安全性）；愈合后调和收敛 |
| 网关双杀（akka+ray 同时） | App 降级清单正确；恢复后自动 reconcile |
| drain 半程消息风暴 | drain_timeout 兜底路径触发，DRAIN_ABORTED 计数=预期 |
| dylib 卸载后立刻 dlopen 新版 | 全局状态无污染（禁清单扫描 + 运行断言双验证） |
| wasm fuel 风暴组件 | OverQuota→监督 Restart 限频，不影响同 Executor 其它组件 |

#### 11.2.4 L4 验收基准（release + 每夜缩减，独占裸机）

| 基准 | 门禁 |
|---|---|
| 三形态 ask 开销 | Props=现状基线（字节级不变）/ Dylib 增量 <1µs / Wasm 增量 <10µs |
| 编排面 | deploy 回执 P99 <1s；调和周期 <5s（百组件 App）；HotSwap 升级端到端 <10s（含 drain） |
| 孪生门禁 | App 图 ≤100 组件全分支混沌 <10min（CI 档）；50 集群档 <6h |
| 回放 | record→replay 确定性（同输入 N 次回放轨迹 hash 一致） |

#### 11.2.5 回归义务（每阶段出口）

- `make test-full` + `MODE=polyglot`（五语言）+ `MODE=stress` + lint 全绿
- golden vectors（帧/WIT/admin-v2 三套）字节不变
- 现有全部用例（1740+）零失败；存量引擎迁移场景（§11.3）作为**重构后的活体回归基准**

### 11.3 存量引擎迁移：全量重构为应用体系的实例（需求 2 裁定，2026-10-06 确认）

**定位声明**：现有全部引擎侧资产——crawler-lab（四运行时集成实验室）、federation-lab（50 集群孪生）、parrot-node 内置 actor 族、interop 四网关示例——**不是遗留物，而是应用体系完成后的首批迁移对象与活体验收例子**。迁移完成前，应用体系不算交付（G 阶段 DoD 的组成部分）。

| # | 存量资产 | 迁移后形态 | 迁移收益（同时是验收断言） |
|---|---|---|---|
| M1 | crawler-lab `main.rs` 手写编排（~700 行：网关地址解析/消息注册/拓扑装配/数据流驱动） | `crawler.app.toml` Manifest（frontier=erlang / index=ray / search=akka / crawl+hub=parrot）+ `parrot app run` | 编排代码 ≤50 行；四引擎组件声明式可 diff；**G 阶段主验收场景** |
| M2 | federation-lab composegen | 基础设施层不动（Y2 裁定）+ 孪生输入从裸拓扑升级为 **AppManifest 驱动**（§7 D4 孪生门禁的输入源） | 同一 Manifest 既跑真实集群又跑孪生 |
| M3 | parrot-node 内置 actor（echo/counter/kv/slow + deploy.* PropsFactory） | 内置 actor 改为**默认 App**（`builtin.app.toml`），parrot-node 启动即 `app run` | parrot-node 自身成为应用体系首个自举用户（吃自己狗粮） |
| M4 | interop 四网关手写 main/示例 handler | 各网关 admin-v2 Executor 化后的回归用例库（MG11 契约测试的 fixture） | 网关示例与契约测试单一事实源 |
| M5 | crawler-lab/RH/CFG 等既有集成测试 | 语义不变，装配入口改为 Manifest（断言逻辑零改动） | 回归基线连续性：迁移前后消息轨迹（cid 序列）一致 |

迁移原则：**行为等价优先**——M1/M3/M5 均以"迁移前后可观测行为（消息轨迹/性能门禁/输出）不变"为验收线；迁移中暴露的手写编排隐式依赖，回流为 Manifest 模型能力（如 crawler-lab 的批量参数 → `ComponentSpec::config`）。

### 11.4 性能预算（对齐 07 §14）

- 编排面（非热路径）：deploy 回执 P99 < 1s（单跳网关）；调和周期 5s 级
- 数据面零损：L1 组件路径与现状字节级一致（回归门禁）；L2/L3 组件按各自基准门禁约束
- 孪生：app 图（≤100 组件）全分支混沌 < 10min CI 档

---

## 12. 风险登记

| 风险 | 等级 | 缓解 |
|---|---|---|
| wasmtime 编译/体积拖累 parrot-node | 中 | feature gate：`wasm` 默认关，启用才进二进制；Executor 能力位如实上报 |
| dylib 卸载 UB（违规产物） | 中 | §4.3 协议 + CI 扫描 + 白盒文档；强制 digest 审计 |
| JVM child-loader 泄漏（升级频繁） | 中 | 升级次数阈值 → 网关计划性重启（Orchestrator 编排，低峰执行） |
| 四网关 admin 方言漂移 | 中 | 协议契约测试：同一 ComponentDeploy 向量四方言结果一致（golden 化） |
| Orchestrator 自身脑裂 | 低 | 自家 Raft 已过验收；AppSupervisor 幂等调和天然收敛 |
| WIT 接口过早冻结限制演进 | 低 | semver + world 版本并存期（wasmtime 多版本实例支持） |

---

## 附：与需求方确认的原始问题对照

| 原始问题 | 本文章节 |
|---|---|
| 1. 引擎扩展（加载/载入/卸载） | §4（三形态+进程外四层） |
| 2. 框架与应用分离（Spring 式） | §2 |
| 3. 多引擎统一提交/部署/管理/升级（自动无感透明） | §3 + §5 + §6 |
| 4. 大型任务调试测试 | §7 |
| 5. library / 语言 VM 预演 | §8 |
| 6. 充分测试计划（100% 覆盖率） | §11.2（全量五层测试计划 + 覆盖率铁律与白名单制度） |
| 7. 存量引擎全部重构为新体系的实例 | §11.3（M1-M5 迁移裁定：迁移完成前应用体系不算交付） |
