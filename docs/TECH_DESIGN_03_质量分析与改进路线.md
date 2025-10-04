# 03 · 质量分析与改进路线

> 承接 [01 架构与设计原则](./TECH_DESIGN_01_架构与设计原则.md)、[02 核心机制与详细设计](./TECH_DESIGN_02_核心机制与详细设计.md)。本文给出：历史编译错误复盘（已清零）、未完成功能台账、设计债与一致性问题、风险评估与改进路线图。
> 数据基准：commit `00f20c9` + 引擎压测修复轮（2026-10-01）。**当前：workspace 0 编译错误 · 339 测试全绿 · TODO/unimplemented 剩 7 处**。

---

## 1. 历史编译错误复盘（b5f74e0 时代的 38 错 → 已全部清零）

> 本章保留作为重构过程的根因存档。当前 `cargo check --workspace` 0 错误、93 warnings（unused 为主）。

### 1.1 当年错误分布
    10|
| 错误码 | 数量 | 语义 | 集中位置 |
|--------|------|------|----------|
| E0308 | 8 | 类型不匹配 | thread/system.rs、scheduler/* |
| E0609 | 9 | 字段不存在（shared_scheduler/dedicated_scheduler ×4+4，thread_pool_size ×1） | system.rs 引用旧结构体字段 |
| E0560 | 2 | 结构体字面量字段不存在 | 同上（SchedulerGroup 迁移残留） |
| E0277 | 9 | trait 约束不满足（`(): ActorRef` ×3、`Arc<dyn ActorRef>: ActorRef` ×2、`Arc<ThreadActorRef<A>>: ActorRef` ×1、`A: Debug` ×3） | ActorPath.target 占位 `Arc::new(())`；把 Arc 当 ActorRef 直接用 |
| E0599 | 4 | 方法不存在（process_batch / handle_worker_panic / handle_message / create_processor_for_mailbox） | worker 调用系统层尚未实现的方法 |
| E0053/E0382/E0505/E0716 | 各 1 | trait 方法签名不匹配 / 所有权问题 | spawn_root_typed 返回类型、config move、self 借用期间 move、临时值生命周期 |

**根因归类（38 错 → 4 类）**：

1. **R1 · SchedulerGroup 迁移未收尾（约 60%）**：`SchedulerGroup{shared_scheduler, dedicated_scheduler}` 聚合体替代旧的散落字段，但使用点未对齐。
2. **R2 · ActorPath.target 占位符（约 25%）**：`Arc::new(())` 不满足 `WeakActorTarget = Arc<dyn ActorRef>`。
3. **R3 · 泛型 spawn 签名漂移**：trait 返回 `Box<dyn ActorRef>`，实现写成 `Arc`。
4. **R4 · 所有权/生命周期细节（3 处）**。

**清零过程**（对应改进路线阶段 0 + 阶段 1，已全部完成）：
- SchedulerGroup 访问器对齐 + 使用点改走 `scheduler_group.*`；
- spawn 流程重排：先建 `ThreadActorRef`（Arc<dyn ActorRef>）→ 构造 ActorPath.target → 再建邮箱/上下文，删除占位符；
- `spawn_root_typed` 返回类型统一；worker 侧改经 `ProcessorInterface`（`process_batch_erased` 对象安全接口）驱动；
- 所有权问题随 `tokio::sync::Mutex` 化的 processor（`AsyncMutex<ThreadActor>`，guard 可跨 await）一并解决。

### 1.2 修复优先级建议（历史存档，已全部执行完毕）

~~P0/P1/P2 各项~~ → **已完成**。后续维护要点：CI 保持 `cargo check --workspace` + `cargo test --workspace` 全绿门禁；93 条 warning 建议择机 `cargo fix` 清理。

---

## 2. 未完成功能台账（TODO/unimplemented 实测清单）

> 全库 TODO/未实现标记已从 27 处降至 **7 处**。功能缺口按影响面排序（标注 2026-10 修复轮后的最新状态）：

### 2.1 系统级缺口（影响可用性）

| # | 位置 | 现状 | 影响 |
|---|------|------|------|
| ~~F1~~ | ThreadActorSystem::spawn_internal | ✅ **已修复**：调度器 worker → mailbox.pop → processor 链路落地，消息主循环真实运行 | thread 后端 actor 正常消费消息 |
| ~~F2~~ | ThreadActorSystem::new | ✅ **已修复**：调度器经 Weak 引用回指系统 | panic 上报/重启链路可用 |
| F3 | spawn_root_boxed（两后端+聚合层） | 全部返回错误/未实现 | 类型擦除 spawn 通道形同虚设 |
| ~~F4~~ | ThreadContext::ask / ThreadActorRef::send | ✅ **已修复**：`send` 桥接 `ask_with_strategy_and_timeout`（AskEnvelope + oneshot），统一 API 的 ask 在 thread 路径拿到真回复 | 双引擎 ask 语义等价（压测验证） |
| F5 | ActixContext::stop | 空 Ok（注释"由系统实现"但系统也未实现） | 优雅停止不可用（注：`ActorRef::stop()` 经 StopMessage 拦截已可用，缺的是 handler 内 self-stop） |
| F6 | watch/unwatch（actix 侧 Err / thread 侧 Ok 空操作） | 两种语义都不完整；thread 侧 watchers 登记后 `notify_watchers_of_termination` 是 TODO | 死亡通知链断裂 |
| F7 | SystemRef::spawn_actor（thread） | "Not implemented yet" | 经 context spawner 的子 actor 派生不可用 |

### 2.2 能力级缺口（影响完整性）

| # | 功能 | 现状 |
|---|------|------|
| F8 | 监督执行器 | 规范完整（决策/策略/工厂），无运行时调用方；`within` 时间窗未参与熔断 |
| F9 | Stream 处理 | trait 全套就绪；两后端 stream_registry 均 unimplemented!/panic |
| F10 | ActorRuntime | trait + 配置 + 负载均衡策略声明完毕，零实现 |
| F11 | 重试策略 | RetryPolicy/Backoff 在 Message/derive 可声明，**运行时无消费方**（send 路径不读 retry_policy） |
| F12 | 优先级调度 | MessagePriority 全套 + derive 支持，但邮箱/调度队列均 FIFO，优先级不生效 |
| F13 | 超时选项 | MessageOptions.timeout 仅显式参数路径生效；envelope.options.timeout 无人消费 |
| F14 | WeakActorRef::upgrade | 硬编码 None |
| F15 | ActorContextScheduler::schedule/cancel_schedule | trait 方法存在、无实现（ScheduledTask 孤儿） |
| F16 | LifetimeEvent 枚举 | 无生产者/消费者 |
| F17 | 状态指标 | SystemStatus/RuntimeMetrics 两后端均返回占位（0 值/Running 硬编码）；ProcessorStats 是唯一真实计数器 |
| F18 | 观察者死亡通知 | ThreadActor.notify_watchers TODO |
| F19 | ReceiveTimeout | set/get 存储可用，超时触发机制无 |

### 2.3 明确声明的"引擎不适用"（非缺陷，是设计）

- Actix 路径的 `receive_message`（非 test）→ 固定错误"Not use on actix engine"。
- ActixActor 自己的 `receive_message` 同上（嵌套包装场景仍走 engine 通道）。
- **2026-10 修复轮后的新形态**：actix 引擎上 `receive_message` 现在是**异步路径的正式载体**（`use_async_handler() == true` 时启用）；derive 宏尚未生成该开关的属性入口，手写 impl 的 actor 可直接覆写。

### 2.4 2026-10 修复轮新增能力（原报告缺陷的反向清单）

| 能力 | 修复前 | 修复后 |
|---|---|---|
| actix 跨 actor 并行 | 8 actor 串行（0.213s） | Arbiter 池，64 actor 并行（0.144s，吞吐 12×） |
| actix 队头阻塞 | 5s 计算冻结全部 actor（max 5271ms） | max 0.13ms |
| actix 异步 handler | 不可表达 | `use_async_handler()` 启用，1280×10ms IO 任务 0.255s（50× 收敛） |
| actix stop() | 完全失效（StopMessage 被丢弃） | handle 内拦截 → ctx.stop()，`is_alive()=false`、后续 send 报 Mailbox closed |
| actix path 唯一性 | 同名类型覆盖注册表 | uuid 后缀，herd-20000 全部独立存活 |
| thread send 语义 | 伪回复（`Ok(Box::new(()))`） | 桥接真 ask，与 actix 等价 |

---

## 3. 设计债与一致性问题

| # | 问题 | 位置 | 危害 | 建议 | 状态 |
|---|------|------|------|------|------|
| D1 | **同名类型三处冲突**：`MessageEnvelope`（api 与 actix/types.rs 同名不同构）、`StopMessage`（actix/types.rs 与 reference.rs 重复定义）、`SystemError`（api::system 与 thread::error）、`ActorFactory`（actor.rs 泛型版与 context.rs 擦除版）、`CloneableMessage`（api::message trait 与 thread::message trait） | 跨模块 | 使用者 import 错包即语义漂移 | 统一命名或删除重复定义 | 🔴 未变 |
| D2 | **derive 产物反向依赖实现层**：ParrotActor 宏生成代码硬编码 `parrot::actix::*` 符号 | derive/actor.rs | "规范不依赖实现"被击穿；新增引擎需改宏 | 引擎 binding 经用户侧注入 | 🔴 未变（`use_async_handler` 尚无 derive 属性入口，加剧此项） |
| ~~D3~~ | **ask 语义不一致** | thread/address.rs | ~~同一 trait 方法两后端语义不同~~ | — | ✅ **已修复**（send 桥接 AskEnvelope） |
| D4 | **自定义消息克隆不可用**：try_from_boxed 仅支持基元类型 | api::message.rs | broadcast 自定义消息运行期失败 | derive 时自动注册克隆 | 🔴 未变 |
| D5 | **`CloneableMessage` 与 `Message` 语义重叠** | 三处 | 克隆机制三轨并行 | 收敛为单一 trait | 🔴 未变 |
| ~~D6~~ | **actix path 无唯一性** | actix/system.rs | ~~多实例 actor 互相覆盖~~ | — | ✅ **已修复**（uuid 后缀） |
| D7 | **ActixContext::schedule_periodic 是不返回的无限循环 future** | actix/context.rs | await 即永久挂起；无法取消 | 改后台任务 + 句柄 | 🔴 未变 |
| D8 | **Arc/Box 双轨 ActorRef 容器** | types.rs、thread | 装箱转换遍布 | 统一为 Arc | 🔴 未变 |
| ~~D9~~ | **Mutex 跨 await 违规** | thread/processor/core.rs | ~~panic/死锁风险~~ | — | ✅ **已修复**（改 tokio::sync::Mutex，guard 可跨 await） |
| D10 | **`A::Config` 未接线**：传入的 config 参数被系统默认覆盖 | thread/system.rs | actor 自身配置不生效 | 增加转换协议 | 🔴 未变 |
| D11 | **注释与实现漂移** | 多处 | 误导新用户 | 随重构统一修订 | 🟡 部分（压测报告/技术文档已更新） |
| D12 | **测试语义分叉**（cfg(test) 双路径） | derive/actor.rs | 单测路径 ≠ 生产路径 | 长期取消双路径 | 🔴 未变 |
| D13（新） | **双路径 Handler 的 unsafe 边界**：lifetime transmute 依赖三条文档化不变式（地址稳定/wait 门控/不越界存活），非类型系统保证 | actix/actor.rs §3.1a | 未来 actix 升级若改变 ContextFut 布局或 wait 语义，将无声破坏 | 补集成测试固化三不变式（herd + starvation + io-async 场景已有）；关注 actix 版本升级 | 🟡 已文档化+压测覆盖 |
| D14（新） | **Arbiter 池线程生命周期**：池懒构建于首次 spawn，线程随 System 存活；shutdown 不 join arbiter 线程 | actix/system.rs | 粗粒度关闭可能留悬挂线程（进程退出场景无害） | shutdown 时逐 arbiter stop + join | 🟡 已知限制 |
| D15（新） | **panic 隔离粒度差异**：actix 一个 handler panic 击穿整个 arbiter 线程上的 actor；thread 引擎 processor 级捕获后 actor 可继续 | 跨引擎 | 监督语义落地时需拉平 | actix 侧补 actor 级 panic 恢复（Supervisor 机制内做） | 🟡 待监督落地时一并处理 |

---

## 4. 风险评估

| 风险 | 等级 | 说明 |
|------|------|------|
| ~~不可编译主 crate~~ | ~~🔴 高~~ → 🟢 已消除 | workspace 0 错误、339 测试全绿 |
| 类型擦除性能税 | 🟡 中 | 每 ask 两次堆分配+downcast；高吞吐场景可考虑对象池 |
| `NonNull<dyn Any>` 引擎指针 | 🟡 中 | unsafe 语义进公共 trait；actix 侧新增的 lifetime transmute 同属此类（D13，已文档化+压测覆盖） |
| 循环引用治理 | 🟢 低 | Weak 升级失败路径多为报错而非降级 |
| 双 SystemError 混用 | 🟡 中 | thread 内 `?` 自动选择就近类型 |
| 监督缺口 | 🟡 中 | panic 只被记录（ProcessorStats）/actix 侧击穿 arbiter（D15），无自动恢复 |
| 并发原语混用 | 🟢 低-中 | flume/ringbuf/SegQueue/oneshot/mpsc 五种通道并存；行为差异需文档化 |
| 文档腐化 | 🟢 低 | 本轮已全面同步（01/02/03/README/压测报告） |

---

## 5. 改进路线图（建议）

### 阶段 0 · 恢复可编译 —— ✅ 已完成

1. ~~38 错修复~~（§1 复盘）。
2. TODO：`cargo fix` 清理 93 warning。
3. CI 接入 `cargo check --workspace` + `cargo test --workspace`（当前可全绿）。

### 阶段 1 · 打通 thread 后端最小闭环（MVP）—— ✅ 已完成

- ~~消息主循环（F1）~~：调度器 worker → mailbox.pop → processor 链路落地。
- ~~统一 ask 语义（D3）~~：ThreadActorRef::send 内部走 AskEnvelope。
- ~~Mutex 跨 await（D9）~~ 与 ~~调度器系统引用注入（F2）~~。

### 阶段 1.5 · 双引擎能力等价（2026-10 修复轮）—— ✅ 已完成

- actix Arbiter 池（跨 actor 并行 + 队头阻塞消除）。
- actix 双路径 Handler（`AtomicResponse` + `use_async_handler()`，异步 handler 可表达且 actor 串行语义保持）。
- actix stop 拦截 / path 唯一化；压测规模对齐（64 actor CPU 并行 / herd 20000 / IO 1280 任务）。
- 产出：`ENGINE_STRESS_REPORT.md`（13 场景实测，双引擎能力矩阵等价）。

### 阶段 2 · 一致性与安全加固 —— 🔲 进行中（下一步）

- 消除同名类型冲突（D1）；actix `ActixContext::schedule_periodic` 改后台任务（D7）。
- 引擎指针封装安全 API（`actix_addr()/schedule_periodic()/register_stream()` 子集）。
- 把 `A::Config` 真正接入 ThreadActorConfig（D10）。
- **新增**：derive 宏增加 `use_async_handler` 属性入口（消除 D2 加剧项）；ActixContext::stop 真实实现（F5）。

### 阶段 3 · 补齐规范承诺的高阶能力 —— 🔲 未开始

优先级依价值排序：监督执行器（F8，含时间窗熔断、OneForAll 广播、**actix 侧 actor 级 panic 恢复以拉平 D15**）→ watch 死亡通知（F6/F18）→ 优先级邮箱（F12）→ 重试/超时选项接线（F11/F13）→ stream_registry（F9）→ 指标真实化（F17）。

### 阶段 4 · 架构演进（长期）

- derive 宏解耦实现层（D2），支持第二引擎验证抽象完备性。
- ActorRef 容器统一为 Arc（D8）。
- Arbiter 池精细化生命周期管理（D14：shutdown join、动态扩缩容）。
- 若走向分布式：MessageEnvelope 增加 serde 边界 + 远程 path 协议。

---

## 6. 测试资产盘点

| 位置 | 内容 | 现状 |
|------|------|------|
| parrot-api/tests（11 文件） | actor/address/context/errors/message/runtime/system/config/factory/integration | ✅ 可运行 |
| parrot-api-derive-tests/tests | message_tests.rs / actor_test.rs | ✅ 可运行 |
| parrot/tests（13 文件） | thread 引擎（dedicated/multi_thread/config/error）+ actix（system/actor_macro/sync/async message）+ **engine_stress_thread / engine_stress_actix（双引擎压测，13 场景）** | ✅ 全部解冻可运行 |
| examples（9 个） | ping_pong / actor_ring / dedicated_thread / message_cloning / async_message_handling 等 | ✅ 全部可构建 |
| **合计** | **workspace 339 用例** | ✅ 0 失败 |

---

## 附 · 快速事实卡（2026-10-01 修复轮后）

- 代码量：15,276 行（.rs，不含 target；api 3,433 / derive 780 / actix 1,302 / thread 11,709 / system+logging 1,510）
- TODO/未实现标记：**7 处**（原 27 处）
- 编译：**4 ✅ / 0 ❌**（0 errors，93 warnings）
- 测试：**339 通过 / 0 失败**；含双引擎压测套件（CPU 并行 64 actor、herd 20000、IO 1280 任务）
- 双引擎能力矩阵：**完全等价**（ask/tell/超时/stop/async handler/多 actor 并行/海量 actor/洪泛韧性）
- 引擎选型速查（压测实测）：CPU 密集 actix 1.6× 优；IO 密集 actix 4× 优；高并发 ask 争用 thread +19% 优；其余持平（详见 ENGINE_STRESS_REPORT）
- 最大结构风险：thread 模块体量（占实现层 ~81% 行数）+ 设计债 D1/D2 未清
- 最成熟路径：双引擎 ask 链路 + actix Arbiter 池并行 + thread async handler（均有压测佐证）
