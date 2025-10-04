# Parrot 核心升级技术方案与详细设计（六项）——附 core-lab POC 验证

> 版本：v1.0（2026-10-03）
> 配套 POC：`Mx-poc/core-lab/`（独立 workspace，6 crate，13 测试全绿）
> 定位：本文是对评测报告六项关键债务/机会的完整技术方案 + 详细设计 + POC 验证结论。

---

## 0. 总览

| # | 需求 | 方案核心 | POC crate | 验证结果 |
|---|------|---------|-----------|---------|
| 1 | derive 宏反向依赖解耦 | 注册点反转：宏只引用 `::parrot_api::` 规范符号 | `derive-decouple` | ✅ 1 测试：双引擎中立 spawn |
| 2 | 监督执行器 F8 + Watch F6/F18 | 策略状态机 + 共享工厂 + panic 捕获 + 死亡广播 | `supervision` | ✅ 3 测试：重启/通知/升级 |
| 3 | parrot-api "框架的框架"定位 | behaviour 层与 runtime 层彻底分离的中立 API 面 | `api-neutral`（经 #1 验证） | ✅ 双形态引擎同构粘合 |
| 4 | 类型擦除单块内存 | Header+payload 连续分配，downcast=指针运算 | `erased-alloc` | ✅ 3 测试：往返/类型错配/Drop |
| 5 | 静态类型编程模式 | `TypedActorRef<M,R>` 双轨 + 显式桥接边界 | `typed-static` | ✅ 3 测试：零Any/桥接/基线 |
| 6 | BEAM 调度机制借鉴 | reduction 预算 + 高优插队 + per-process heap | `beam-sched` | ✅ 3 测试：公平/插队/堆释放 |

POC 运行方式：

```bash
cd Mx-poc/core-lab && cargo test
# TOTAL passed: 13  failed: 0
```

POC 质量定位：验证核心语义与算法可行性，非正式实现质量量级（无 UnsafeCode 审核、无 miri、无压测）。

---

## 1. derive 宏反向依赖解耦（D2 债务）

### 1.1 现状问题

`parrot-api-derive` 的 `ParrotActor` 宏生成代码直接引用 `parrot::actix::*` 符号：

- 规范层（api）在**符号级**依赖实现层（engine），依赖倒置被击穿；
- 第三方引擎（thread / 未来引擎）无法享受宏胶水；
- 编译依赖图出现环：api-derive → parrot(engine) → parrot-api。

### 1.2 方案：注册点反转（三层架构）

```
┌─────────────────────────────────────────────────┐
│ 宏层 parrot-api-derive                           │
│  生成代码只含 ::parrot_api::* 符号（规范层）        │
│  ——零引擎符号，编译期可静态断言                     │
├─────────────────────────────────────────────────┤
│ 规范层 parrot-api                                 │
│  ErasedActor / EngineRuntime / ActorBehaviour    │
│  BoxedMessage / BoxedActorRef                    │
│  （不知道任何引擎的名字）                           │
├─────────────────────────────────────────────────┤
│ 实现层 parrot::actix / parrot::thread / 第三方     │
│  impl EngineRuntime for ActixRuntime { .. }      │
│  （依赖规范层，可插拔注册）                         │
└─────────────────────────────────────────────────┘
```

关键机制（POC 已验证）：

1. **宏生成物**（`derive-decouple/src/lib.rs`）：
   - `impl ::parrot_api::ErasedActor for Self`：把 `ActorBehaviour::receive` 桥到擦除接口；
   - `Self::spawn_on<E: EngineRuntime>(engine, path)`：经 trait 泛型 spawn，任何引擎可承载。
2. **符号绑定**：用户侧 `extern crate api_neutral as parrot_api;`（正式版中 `parrot-api` 本就是独立 crate，无需别名——POC 用别名模拟）。
3. **防回归断言**（正式版落地项）：宏测试里对 `cargo expand` 输出做 lint，出现 `parrot::actix` 即 CI 失败。

### 1.3 POC 验证（`derive-decouple/tests/poc1_test.rs`）

- `DerivedCounter` 经宏获得 `spawn_on`，**同一类型**跑在 `MockNativeEngine`（线程邮箱型）与 `MockFiberEngine`（路径重写型，模拟不同形状）上；
- 断言两引擎独立计数、第二引擎保持自身路径形状（`fiber:/d2`）；
- 全程业务代码零 `use` 引擎符号。

### 1.4 正式版迁移要点

- 现有 `ParrotActor` 宏生成物逐条改为规范符号；
- `parrot::actix` 提供 `impl EngineRuntime`；`parrot::thread` 同理；
- 宏展开 lint 进 CI（`cargo expand + grep -L 'parrot::actix'`）。

---

## 2. 监督执行器（F8）+ Watch 死亡通知（F6/F18）

### 2.1 语义模型（对齐 Akka/Erlang Supervisor）

```
spawn_supervised(factory, path, strategy)
        │
        ▼
┌─ ChildEntry ────────────────────────────┐
│ factory: 共享工厂(Arc<Mutex<FnMut>>)      │  ← 重启的实体来源
│ strategy: OneForOne{max,window}/Stop/…   │
│ restarts: Vec<Instant>  ← 窗口限频历史    │
└─────────────────────────────────────────┘
        │ 消息循环（catch_unwind 包裹）
        ▼ panic / 通道关闭
   on_child_death(path, reason)
        │
        ├─► broadcast WatchEvent::Terminated（DeathWatch：任何死法都广播）
        │
        └─► 策略决策:
              OneForOne: 窗口内 restarts < max → Restart（factory 重调）
                         窗口超限           → Escalate
              Stop:      不重启
              Escalate:  上抛父级（POC 中= 广播 Escalated 事件）
```

### 2.2 关键算法与实现细节（POC `supervision/src/lib.rs`）

1. **panic 捕获**：actor 闭包执行包 `catch_unwind(AssertUnwindSafe(...))`；panic 值规范化为 `DeathReason::Panic(String)`；ask 挂起者收到 `Err("panic: ...")` 而非永久阻塞。
2. **共享工厂**：`ChildFactory = Arc<Mutex<Box<dyn FnMut() -> ActorClosure>>>`。
   - **踩坑记录（写进设计以防复犯）**：POC 第一版用 `std::ptr::read` 做 Box 浅克隆 → 同一 Box 双 drop → SIGBUS。结论：**重启语义要求工厂可重入，Arc<Mutex> 是最低成本正解**；正式版进一步用"工厂注册表（path → 构造器）"让监督器与用户闭包解耦。
3. **窗口限频**：`restarts.retain(|t| now - t < window)` 后检查 `len() >= max_restarts` → Escalate。对齐 Akka `maxNrOfRetries / withinTimeRange`。
4. **DeathWatch**：`watch(watcher, target) → Receiver<WatchEvent>`；monitor 与被 monitor 完全分离（任意方可 watch 任意路径，非父子也可）；`unwatch` 移除订阅。
5. **正常退出不重启**：通道关闭（所有 sender drop）= `DeathReason::Normal`，只广播不重启——与 Erlang `normal` 退出语义一致。

### 2.3 POC 验证（3 测试全绿）

| 测试 | 验证点 |
|------|-------|
| `supervisor_restart_on_panic` | panic(999) 后自动重启，消息服务恢复 |
| `death_watch_notification` | watcher 收到 `Terminated{Panic}` 事件 |
| `restart_budget_escalation` | 窗口内连环击杀 → Escalate 事件 |

### 2.4 正式版增量（POC 未覆盖、设计已预留）

- **AllForOne / RestForOne**：决策作用域从单 child 扩到兄弟集合（children 表已按 path 索引，扩展点在 `Decision` 分支）；
- **指数退避**：`Restart` 分支加 `min(backoff_base * 2^n, cap)` 延迟（BEAM `supervisor:restart_intensity` + Akka Backoff 语义）；
- **监督树层级**：Escalate 沿 parent 指针上抛而非直接广播；`Terminated` 附带 exit signal 传递（POC 的 broadcast 是扁平化简化）;
- **system message 通道**：kill/monitor 走独立高优队列（与 #6 的 Priority::High 复用）。

---

## 3. parrot-api 中立化（"框架的框架"回归）

### 3.1 现状问题

现 API 照着 actix 设计：`Context` 携带生命周期钩子、地址模型是 `Addr` 形状、`Started/Stopping/Stopped` 状态机是 actix 约定。后果：thread 引擎被迫长成 actix 形状；非 actix 系引擎（Erlang/Ray/Akka 粘合）适配别扭。

### 3.2 设计原则：behaviour / runtime 二分（Erlang 哲学）

Erlang 的成功抽象：**callback module 定义"做什么"，runtime 决定"怎么跑"**。parrot-api 应该是 behaviour 层，且只定义最小完备面：

```rust
// 规范层全部 API 面（POC api-neutral/src/engine.rs，共 5 个符号）
pub type BoxedMessage = Box<dyn Any + Send>;
pub type BoxedResult = Result<BoxedMessage, String>;
pub type BoxedActorRef = Arc<dyn ActorRefErased + Send + Sync>;

pub trait ErasedActor: Send + 'static {
    fn receive(&mut self, msg: BoxedMessage) -> BoxedResult;
}
pub trait ActorRefErased: Send + Sync {
    fn tell(&self, msg: BoxedMessage);
    fn ask(&self, msg: BoxedMessage) -> BoxedResult;
    fn path(&self) -> String;
}
pub trait EngineRuntime: Send + Sync {
    fn name(&self) -> &'static str;
    fn spawn_erased(&self, actor: Box<dyn ErasedActor>, path: &str)
        -> Result<BoxedActorRef, String>;
}
```

设计约束：

1. **无生命周期钩子强加**：`pre_start/post_stop` 是可选 trait（`Lifecycle`，默认空实现），不是 `Actor::started(ctx)` 硬钩子；
2. **无 Context 强加**：behaviour 收 `(state, msg)`，引擎相关能力（定时器/订阅/spawn 子）由引擎各自的 ExtendedContext 提供，不进规范面；
3. **消息模型最小化**：规范面只有 `BoxedMessage`；静态类型轨（#5）是**扩展 trait**而非替代，规范面不变；
4. **ref 三方法面**：`tell/ask/path` 是所有已知引擎（Actix Addr、Erlang pid、Ray handle、Akka tell/ask）的公共子集，验证过无表达力损失。

### 3.3 多引擎粘合模式（POC 双形状验证）

- 引擎 A（线程邮箱型）：`mpsc` + 独立线程，`tell` fire-and-forget；
- 引擎 B（fiber 型）：路径重写 + 委托，对外同一 `EngineRuntime` 面；
- 同一 `DerivedCounter` 在两引擎行为一致（#1 测试 `poc1_derive_is_engine_neutral`）。

### 3.4 正式版迁移路径

1. 新面以 `parrot_api::engine` 模块落库（不动现有面，双面过渡）；
2. `parrot::actix` / `parrot::thread` 各实现 `EngineRuntime`；
3. 远程层（Mx-poc 的 L0-L4 wire）天然对齐：`spawn_erased` 的远端版= 网关 spawn；
4. 现有 actix 形 API 标记 deprecated 于 2.0。

---

## 4. 类型擦除的单块内存优化（ADR-1 成本削减）

### 4.1 现状成本

边缘类型安全、中心类型擦除：每消息 2 次堆分配（消息 Box + 信封/Any 擦除 Box）+ 2 次 downcast（发送侧、接收侧各一）。

### 4.2 方案：SingleAllocEnvelope（Header+payload 连续单分配）

内存布局（POC `erased-alloc/src/lib.rs`）：

```
┌─────────────── 一次 alloc(total, align=max(Header,M)) ───────────────┐
│ Header { type_id: TypeId,          // 16B，downcast 判据             │
│          drop_fn: unsafe fn(*mut u8), // 特化 drop（类型擦除析构）     │
│          payload_off: usize,        // M 对齐后的 payload 偏移        │
│          layout: Layout }           // dealloc 依据                   │
│ [padding 至 align_of::<M>()]                                          │
│ payload: M（move 写入）                                               │
└──────────────────────────────────────────────────────────────────────┘
```

关键算法：

1. **布局计算**：`align = max(align_of Header, align_of M)`；`payload_off = round_up(size_of Header, align_of M)`；`total = payload_off + size_of M`。一次 `alloc`。
2. **类型擦除析构**：`make_drop::<M>()` 生成特化裸函数 `unsafe fn(*mut u8)` 存入 header——这是"在无类型内存上恢复类型化 drop"的核心，对标 Erlang boxed header 与 `Box<dyn Any>` 的 vtable 面，但**无多级指针跳转**。
3. **downcast = O(1)**：`TypeId` 比较 + `ptr + payload_off` 读出，零分配零哈希。
4. **所有权安全**（POC 踩坑后固化）：
   - `downcast(mut self)` 成功路径必须 `mem::forget(self)` 后手动 `dealloc`——否则 `Drop::drop` 二次释放（POC 第一版 SIGSEGV 的根因）；
   - 失败路径原样返回 `Err(self)`，信封可继续尝试其他类型。

### 4.3 效果与边界

- 分配次数：2 → **1**；downcast 语义不变（仍 2 次，但每次变为纯指针运算，无 vtable 双跳）；
- 代价：header 约 48B（TypeId16+fn8+off8+Layout16）+ padding（≤ align-1）。小消息（≤8B）相对开销大 → 正式版分层：
  - **SSO 档**：≤16B 消息 inline 进 header 尾部（SmallBox 思路），零独立分配；
  - **单块档**：本 POC 布局；
  - **池化档**：单块进一步进 slab 池（复用不释放），配合 #6 per-actor heap。
- Send 推导：`unsafe impl Send`（payload 构造时 move 独占，M: Send 已约束）。

### 4.4 POC 验证（3 测试）

往返（256B 大消息）/ 类型错配保信封 / payload Drop 精确执行一次。

### 4.5 正式版集成点

mailbox 队列元素从 `Box<Box<dyn Any>>` 换 `SingleAllocEnvelope`；`ask` 的响应槽同机制；miri + 泄漏审计进 CI。

---

## 5. 静态类型编程模式（双轨引擎）——可行性结论：**可行，已 POC**

### 5.1 设计（用户构想 + POC 落地形态）

**动态轨（现状）保留**：`Box<dyn Any + Send>`，适用于异构消息、远程、动态路由。

**静态轨新增三个符号**（POC `typed-static/src/lib.rs`）：

```rust
// ActorRefExt 的静态版
pub trait TypedAskRef<M: Send + 'static>: Send + Sync {
    type Reply: Send + 'static;
    fn ask(&self, msg: M) -> Result<Self::Reply, String>;
    fn tell(&self, msg: M);
}
// Actor trait 的静态 receive 版
pub trait TypedReceive<M>: Send + 'static {
    type Reply: Send + 'static;
    fn receive_typed(&mut self, msg: M) -> Result<Self::Reply, String>;
}
// 静态 ref：持有类型化通道 mpsc<(M, ReplyTx)>
pub struct TypedActorRef<M, R> { tx: Sender<(M, Option<Sender<Result<R,String>>>)>, .. }
```

核心性质：

1. **零 Any 全链路**：`spawn_typed` 建立 `mpsc::<M>` 类型化通道；`ask`/`tell` 消息**从不装箱**，接收侧**从不 downcast**——静态轨全程 0 装箱 0 downcast（对比动态轨 2+2）；
2. **`fn(M) -> R` 不变型 PhantomData**：防 M 协变导致的类型混用；
3. **双轨显式桥接**：`TypedActorRef::into_dyn(encode, decode)`，装箱只发生在边界各一次，且由用户显式声明编解码（无隐式 Any 转换点）；
4. **一个 actor 多消息类型**：`impl TypedReceive<Add> + TypedReceive<Get>` 多实现；每消息类型一个 ref 视图。POC 用双 spawn 演示独立实例；**正式版**由 `#[parrot_actor]` 宏生成"单一 spawn + 按 M 分派的枚举信封"（宏读出全部 `TypedReceive` impl 生成 `enum Msg { Add(Add), Get(Get) }` + match 分派，仍零 Any）。

### 5.2 与后端（actix 等）直连

宏生成的枚举信封天然是 actix `Message`/Handler 的形状：`Msg` 枚举即 actix 消息类型，`receive_typed` 即 Handler 逻辑——**静态轨消息可直接喂 actix mailbox，无 Any 中转**。这是"把这个类型直接发送到 actix 等后端"的落地路径，POC 以线程引擎验证了通道结构（actix 侧为同构映射）。

### 5.3 POC 验证（3 测试）

| 测试 | 验证点 |
|------|-------|
| `static_track_zero_any` | 静态 ask/tell 全程类型化，累积语义正确 |
| `dual_track_bridge` | into_dyn 桥接，装箱仅边界 |
| `dyn_track_baseline` | 动态轨现状不受影响，双轨并存 |

### 5.4 正式版注意

- ask 的 `Reply` 关联类型使 ref 类型面变宽（`TypedActorRef<M,R>`），宏须承担类型推断（`engine.actor(calc, path)` 已验证推断可省略标注）；
- 远程边界强制回落动态轨（wire 层本就序列化）——静态轨是**进程内高速档**，定位要写进 ADR。

---

## 6. Erlang/OTP 调度机制借鉴（重点 thread 后端）

### 6.1 BEAM 机制 → Parrot 可借鉴性分析

| BEAM 机制 | BEAM 做法 | Parrot(thread) 现状 | 可借鉴性 | POC |
|-----------|----------|--------------------|---------|-----|
| reduction 抢占 | 每 process 4000 reductions 强制 yield（真抢占：栈扫描/信号驱动） | 分片 scheduler 有 yield，但**长消息处理不可打断**，且 yield 不能让急件插队（M2/M3 实测） | ✅ 高：预算让出 + 让出后重扫高优队列 | `reduction_preemption_and_fairness` |
| 优先级消息 | signal 有 priority，抢占点重排 | 无优先级区分 | ✅ 高：队列扫描 O(n) 挑 High | `priority_jump_beats_fifo` |
| per-process heap | process 私有堆，GC 独立，死亡即整堆丢 | actor 状态在闭包/结构体，drop 语义零散 | ✅ 中高：Heap 槽位化，死亡整块释放 | `process_death_releases_heap` |
| 分布式 | location transparent pid | Mx-poc L0-L4 已验证 | ➖ 已覆盖（另一分支） | — |
| 抢占实现成本 | BEAM 有自己的调度器/栈格式 | Rust 线程栈不可安全扫描 | ⚠️ 采用**合作式检查点**（诚实的权衡，见 6.3） | — |

### 6.2 POC 设计（`beam-sched/src/lib.rs`）

```
ReductionScheduler
├─ 全局队列 VecDeque<QueuedMsg{actor, msg, priority}> + Condvar
├─ N worker：
│   1. 取消息：先线性扫 High（插队语义）→ 否则 FIFO 头
│   2. reduction 检查：p.reductions >= p.budget → 记抢占 + 清零（预算恢复）
│   3. 执行 behaviour(&mut heap, msg)，返回 false → p.alive=false
│      （Process 整体 drop = per-process heap 整块释放）
└─ 统计：preemptions / priority_jumps（可观测性）
```

POC 取 `budget=4`（极小值放大可观测性；BEAM 为 4000，正式版按消息平均成本校准，建议默认 1024 起步 + per-actor 可调）。

### 6.3 关键权衡：真抢占 vs 检查点

BEAM 能 4000-reduction 硬抢占是因为它 owns everything（调度器、栈格式、代码计数）。Rust 上：

- **信号栈扫描**（类似 tokio 都不做）：需要 sanitizer 级别的栈元数据，成本/风险不成比例；
- **检查点插入**：behaviour 签名天然是**逐消息**为单位——`FnMut(&mut Heap, msg)` 每条消息之间就是让出点。POC 证明：**逐消息预算 + 高优重扫**已解决 M2/M3 的两大痛点（长任务饿死他人、yield 不让急件插队）；
- **残余缺口**：单条消息内部超长计算仍不可打断。正式版提供 ` cooperative_yield` 显式 API（长循环内自查预算），文档明示这是与 BEAM 的语义差距，不做虚假承诺。

### 6.4 POC 验证（3 测试）

| 测试 | 验证点 |
|------|-------|
| `reduction_preemption_and_fairness` | 慢 actor（2ms/条×40）不饿死快 actor（10 条全清）；preemptions>0 |
| `priority_jump_beats_fifo` | 单 worker 下 High 消息越过 20 条 Low 积压先执行 |
| `process_death_releases_heap` | 进程退出后从存活表移除，heap 随 Process drop 整块释放 |

### 6.5 正式版落地（thread 后端改造清单）

1. 现有分片 scheduler 的每 actor 消息循环加 `reductions` 计数与预算检查（POC 逻辑直移）；
2. 队列结构从纯 FIFO 改双队列（High/Normal，扫描改 pop，O(1)）；
3. `Process{behaviour, heap, reductions, budget}` 结构化 actor 状态，死亡路径统一 `drop`；
4. budget 暴露为 spawn 参数 + 系统默认值；
5. `Terminated`/kill 等系统消息走 High 队列（与 #2 监督联动：重启必须先于用户消息到达）。

---

## 7. 六项方案的整合视图

```
                        ┌────────────────────────────┐
                        │  用户代码（边缘类型安全）      │
                        │  #[derive(ParrotActorNeutral)]  ← #1 宏（零引擎符号）
                        │  TypedActorRef<M,R>  ←→  DynRef  ← #5 双轨（显式桥接）
                        └─────────────┬──────────────┘
                                      │ spawn_on / into_dyn
                        ┌─────────────▼──────────────┐
                        │ parrot-api（规范层，#3）      │
                        │ ErasedActor/EngineRuntime   │
                        │ BoxedMessage=#4 单块信封      │
                        └─────────────┬──────────────┘
              ┌───────────────────────┼─────────────────────┐
        ┌─────▼─────┐          ┌─────▼─────┐         ┌─────▼─────┐
        │ actix 引擎 │          │ thread 引擎│         │ 远程网关    │
        │(静态轨直连) │          │#6 调度升级 │         │(L0-L4)    │
        │           │          │#2 监督/watch│        │           │
        └───────────┘          └───────────┘         └───────────┘
```

- #1/#3 是**符号级地基**：先立规范面，其余全部长在上面；
- #4 是**数据平面**优化：单块信封同时服务动态轨与远程序列化缓冲；
- #5 是**编程模型**扩展：静态轨进程内高速档，远程/异构回落动态轨；
- #2/#6 是**运行时语义**补全：监督树 + 抢占调度让 thread 引擎具备 production 身份性功能。

## 8. 风险与迁移顺序

1. **先 #1+#3**（纯结构性，无行为变化，CI 加宏 lint）；
2. **再 #2**（监督是行为新增，无破坏；工厂注册表先行避免 POC 妥协）；
3. **再 #6**（scheduler 改造，需基准回归防性能倒退）；
4. **再 #4**（unsafe 密集，miri + fuzzing 门禁，mailbox 灰度切换）；
5. **最后 #5**（面最广，宏生成量大，依赖 #3 规范面稳定）。

各步独立可发布，均不破坏现有 API（#3 双面过渡）。
