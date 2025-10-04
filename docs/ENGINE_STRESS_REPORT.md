# Parrot 双引擎（Thread vs Actix）极限压测与能力等价性验证报告

> 测试日期：2026-10-01（第二轮）/ 2026-10-02（第三轮 debug / 第四轮 release + 双修复） · 环境：macOS (Apple Silicon, 15 逻辑核) · Rust stable · **release test profile**
> 压测代码：`parrot/tests/engine_stress_thread.rs` / `engine_stress_actix.rs`（共用 `engine_stress_common/`）
> 第三轮新增 8 个场景（M1–M3 混合负载 + E1–E4 极端场景 + BatchEcho 微基准），负载参数双引擎逐字节对齐。
> 业务逻辑逐字节对齐：两个引擎跑完全相同的 actor 代码，仅引擎绑定层不同。

---

## 一、本轮修复摘要（针对第一轮三大缺陷）

| 缺陷 | 根因 | 修复 | 效果 |
|---|---|---|---|
| **无跨 actor 并行**（8 actor 串行 0.213s） | 适配层只用 System 主 arbiter，所有 actor 挤单线程 | `ActixActorSystem` 内置 **Arbiter 池**（默认 CPU 数个 `Arbiter::new()` 线程），spawn 时 round-robin 经 `start_in_arbiter` 分配 | 64 actor 并行 wall=0.144s，吞吐 11096/s（**+12×**） |
| **队头阻塞传染**（一个 5s 计算冻结全部 actor，max 5271ms） | 单 arbiter 上同步 handler 独占线程 | 多 arbiter 物理隔离 + 异步路径 await 期间释放线程 | 饥饿测试 max **0.13ms**（原 5271ms，**40000×改善**） |
| **无法表达异步 handler**（同步签名，IO 场景不可用） | `Handler::Result` 为同步 `Option` | Handler 返回 `AtomicResponse`：同步快路径（`receive_message_with_engine`，默认）+ 异步路径（`receive_message` async fn，经 `use_async_handler()` 开关启用），future 挂到 `ctx.wait` 保持 actor 串行语义 | io-async-64actors 实测通过：1280×10ms 任务 wall=0.255s（**50× 收敛**） |
| （附带）同类型 actor 注册表互相覆盖 | 路径只用类型名 | spawn 路径追加 uuid 后缀 | herd-20000 全部独立存活 |

**兼容性**：默认行为不变（同步快路径优先，`None` 丢弃语义保留）；workspace 全部测试 0 失败。

---

## 二、测试矩阵总览

| # | 场景 | 验证目标 |
|---|------|---------|
| 1 | seq-ask-echo-1k | 串行 ask 往返延迟基线 |
| 2 | conc-ask-echo-c8 / c64 | 中/高并发 ask 吞吐与延迟 |
| 3 | tell-echo-100k | fire-and-forget 吞吐 |
| 4 | cpu-serial / cpu-parallel-**64actors** | CPU 密集：串行 vs 并行扩展性 |
| 5 | flood-500k-tell | 50 万消息洪泛（邮箱容量、drain 能力） |
| 6 | ask-timeout-short | 1ms 超时在 actor 繁忙时是否正确触发 |
| 7 | send-after-stop | 停止后的错误语义 |
| 8 | herd-**20000**-actors | 海量 actor 创建成本（akka 级规模） |
| 9 | longrun-2G-iters | 长时程任务（~10s 单任务） |
| 10 | starve-echo-during-longrun | **饥饿交叉验证**：长任务期间其他 actor 响应力 |
| 11 | io-async-**64actors** | IO 密集（handler 内真实 await）并发收益 |

---

## 三、核心数据对比（修复后）

| 场景 | Thread 引擎 | Actix 引擎（修复后） | 优势方 |
|---|---|---|---|
| 串行 ask 1k | 48k/s，p99=0.05ms | **70k/s**，p99=0.03ms | actix +46% |
| 并发 ask c8 | 302k/s | **289k/s** | 持平（thread +4%） |
| 并发 ask c64 | **357k/s**，p99=0.26ms | 301k/s，p99=0.34ms | thread +19% |
| tell 100k | 63k/s | **100k/s** | actix +58% |
| CPU 串行（200×200k 迭代） | 959/s，p50=1.04ms | 968/s，p50=1.03ms | 持平（单 actor 语义一致） |
| **CPU 并行（64 actor × 25 msg）** | 7098/s，wall=0.225s，p50=6.7ms | **11096/s，wall=0.144s，p50=4.6ms** | **actix 1.6×** |
| 洪泛 500k | 343k/s 全 drain | **388k/s** 全 drain | **actix +13%** |
| 超时边界（1ms） | ✅ 正确 Timeout | ✅ 正确（独立线程探测验证） | 等价 |
| stop 后发送 | ✅ InternalError | ✅ Mailbox closed | 等价 |
| **20000 actor 创建** | 0.099s（202k/s） | **0.094s**（213k/s） | 持平（actix 略优） |
| 长时程 2G 迭代 | 10.3s 正常完成 | 10.3s 正常完成 | 持平 |
| **饥饿测试：计算期间 echo** | max=0.06ms | **max=0.13ms** | **等价（均无冻结）** |
| **IO 密集（handler 内 await，64 actor × 20 × 10ms）** | 1.06s（12× 收敛） | **0.255s（50× 收敛）** | **actix 4×** |

> 注：第一轮报告中 actix 的 cpu-parallel（8 actor 串行 0.213s）、starve（max 5271ms）、io-async（不可表达）三项缺陷已全部消除。

---

## 四、能力等价性矩阵（修复后）

| 能力 | Thread | Actix | 结论 |
|---|---|---|---|
| ask（请求-响应） | ✅ | ✅ | 等价 |
| tell（fire-and-forget） | ✅ `send_msg` | ✅ `do_send`/`tell` | 等价 |
| 超时控制 | ✅ oneshot+timeout | ✅ actix timeout | 等价 |
| stop 语义 | ✅ | ✅ | 等价 |
| **async handler（IO 型）** | ✅ handler 是 async fn | ✅ `use_async_handler() = true` 启用异步路径 | **等价** |
| **多 actor 真并行** | ✅ 池化 worker 并发处理 | ✅ Arbiter 池（默认 CPU 数线程） | **等价（actix 略优）** |
| **海量 actor（20000）** | ✅ | ✅ | 等价 |
| 消息洪泛韧性 | ✅ 500k 无丢失 | ✅ 500k 无丢失 | 等价（actix 快 13%） |
| 同类型多实例注册 | ✅ 路径唯一 | ✅ uuid 后缀路径 | 等价 |

---

## 五、性能画像与适用场景结论（修复后）

### Thread 引擎（自研，池化 worker + 唤醒钩子调度）
- **架构特征**：中央 `SchedulingQueue` + N worker 抢占邮箱批次；`ScheduleState` 保证单 actor 互斥；async handler 原生支持。
- **强项**：
  - 高并发争用 ask（c64 下 357k/s，+19%）
  - 长时程任务隔离（max 0.06ms）
- **观察**：64 actor 规模 CPU 并行吞吐 7098/s（batch=10 的调度粒度带来轻微队头延迟，p50 6.7ms）；IO 密集大规模并发下收敛比 actix 低（worker 批处理重排队开销）。
  - **后续实验（ADR-4 复审）**：`worker.rs::batch_size_hol_experiment` 证实单 actor 场景 batch_size **不放大**队头延迟——batch=1 因每消息重入队开销反而慢 ~2×（7.8ms vs 4.4ms，backlog=200×20µs）；batch 的真实杠杆是**跨 actor** 公平性（批间让出调度队列）。上句"轻微队头延迟"应理解为批处理粒度下的调度间隙，而非 batch 本身的代价。

### Actix 引擎（适配层，修复后）
- **架构特征**：成熟 actix 生态 + **Arbiter 池**（每 arbiter 独立 OS 线程/单线程 runtime）+ **双路径 handler**（同步快路径 / `ctx.wait` 异步路径）。
- **强项**：
  - 串行 ask 延迟最低（70k/s，p99 0.03ms）
  - CPU 并行扩展性最佳（64 actor 11096/s，p50 4.6ms——专职 arbiter 线程无调度队列开销）
  - 洪泛吞吐最高（388k/s）
  - IO 密集收敛比最佳（async handler 释放线程，50× 收敛）
  - 海量 actor 创建与 thread 持平（213k/s）
- **语义保证**：
  - actor 内消息严格串行（`ctx.wait` 门控邮箱，与 thread 引擎一致）
  - ask 回复在 handler 完成后才发送（超时/关闭语义不变）

---

## 六、最终推荐（修复后）

| 场景 | 推荐 | 依据 |
|---|---|---|
| CPU 密集（计算、编解码、加密） | **Actix**（反超） | 64 actor 11096/s vs 7098/s；专职 arbiter 线程 |
| IO 密集（DB、HTTP、等待型） | **Actix** | 50× 收敛 vs 12×；async handler 释放线程更彻底 |
| 高并发 ask 争用（>32 并发） | Thread | c64 下 357k/s vs 301k/s |
| 低并发串行 ask、延迟敏感 | **Actix** | 70k/s，p99 0.03ms |
| 纯吞吐管道（fire-and-forget） | **Actix** | 100k/s tell、388k/s 洪泛 |
| 长时程任务 + 系统响应 | 两者皆可 | 隔离性等价（max 0.13ms / 0.06ms） |
| 海量轻量 actor | 两者皆可 | 20000 actor 均通过，速度持平 |

**总体结论**：修复后 **Actix 适配层补齐了全部三项能力短板（并行、隔离、异步 handler），且在 CPU 并行、IO 密集、洪泛吞吐、串行延迟四个维度反超 Thread 引擎**；Thread 引擎仅在高并发 ask 争用场景保持优势。两引擎能力矩阵完全等价，可按场景自由选型。

---

## 七、实现说明（适配层关键机制）

### 7.1 Arbiter 池（`parrot/src/actix/system.rs`）
```rust
// spawn 时 round-robin 分配 arbiter
let addr = actix::Actor::start_in_arbiter(&pool.next_arbiter(), |_ctx| actor_base);
```
- 每个 arbiter 是独立 OS 线程（`enable_all` 单线程 tokio runtime，timer 可用）
- 池懒构建、进程内共享、默认 `num_cpus` 个
- `ActixActorSystem::with_arbiter_count(n)` 可显式指定

### 7.2 双路径 Handler（`parrot/src/actix/actor.rs`）
```rust
impl<A> Handler<ActixMessageWrapper> for ActixActor<A> {
    type Result = AtomicResponse<Self, Option<ActorResult<BoxedMessage>>>;
    // 1) 默认：receive_message_with_engine 同步快路径（零开销）
    // 2) use_async_handler() == true：receive_message（async fn）经
    //    AsyncDispatchFuture 挂到 ctx.wait——await 期间释放 arbiter 线程，
    //    actor 串行语义由 actix 的 waiting() 门控保证
}
```
- ask 回复在 future 完成后经 actix oneshot 发送，`Addr::send`/timeout/do_send 语义全部不变
- lifetime 扩展安全性：`ActixActor` 位于堆上 boxed `ContextFut`（地址稳定）；`ctx.wait` 门控期间邮箱不被 poll（无别名访问）；future 生命周期不超过 ContextFut

### 7.3 actor 侧开关（`parrot-api/src/actor.rs`）
```rust
fn use_async_handler(&self) -> bool { true }  // 默认 false，完全兼容存量
```
- `false`（默认）：走同步 `receive_message_with_engine`，行为与旧版逐字节一致
- `true`：全部消息走 `receive_message`，与 thread 引擎行为完全一致（同一份 async handler 代码双引擎通用）


---

## 第三轮扩展（2026-10-02）：分钟级 CPU 混合负载与极端场景

### 场景设计动机

用户核心诉求：**一个 actor 内执行分钟级 CPU 密集任务的同时，新的长时程任务与短小任务持续加入**，对比双引擎的整体吞吐、执行效率、任务耗时。为此新增：

| # | 场景 | 验证目标 |
|---|------|---------|
| M1 | mixed-minute-cpu-plus-incoming | 8 个专职 actor 各跑一个 ~11.6s 重 CPU 任务（分钟级语义样本，debug profile 标定）+ 4 个后备 actor 持续接 40 个 1.6s 中等任务并发涌入 + 独立探测器每 200ms 测短任务延迟 |
| M2 | mixed-same-actor-fifo | **同一 actor** 邮箱 FIFO：1×9.3s 长任务 + 6×1.6s 中任务 + 4000 个微秒级短任务排队，尾部哨兵 ask 测排队代价 |
| M3 | chunked-vs-solid-longrun | 同一 9.3s 任务两种执行形态：连续执行 vs 20 片分片 + 片间 yield（协作式让出）；对比总耗时（让出开销）与同 actor 排队 tiny 任务的等待 |
| E1 | pingpong-rtt-10k | 双 actor 乒乓 RTT（10k 次 2 跳 ask） |
| E2 | self-chain-ask-tell-20k | 同一 actor 背靠背 ask→tell（退化链式，测自传递开销） |
| E3 | slow-consumer-8prod-40k | 8 快生产者 × 5000 × ~200µs 消息灌 1 个慢消费者（邮箱积压极限） |
| E4 | spawn-stop-storm-5k | 5 波 × 1000 actor 创建→ask→停止（生命周期风暴） |

### 核心数据（第三轮）

| 场景 | Thread 引擎 | Actix 引擎 | 优势方 |
|---|---|---|---|
| **M1 混合分钟级负载**（48 任务，标称 155.5 CPU·s） | wall=25.0s，util=41%，probe p99=0.2ms max=0.2ms | **wall=17.0s，util=61%**，probe p99=1.4ms max=1.4ms | **actix（效率 +48%）**；thread（延迟微优） |
| M2 同 actor FIFO 混合 | wall=19.3s，tail-probe 7ms | wall=19.5s，tail-probe ≈0ms | 等价（actix 尾部略优） |
| M3 分片 vs 连续 | solid=9.5s / chunked=9.5s，yield 开销 −0.2%，tiny 等待 9526ms | solid=9.7s / chunked=9.7s，yield 开销 0.5%，tiny 等待 9747ms | **等价**：两引擎均保持 actor 串行语义（分片不插队），让出开销 <1% |
| E1 乒乓 RTT | 64,593 RTT/s（p99 0.1ms） | **102,126 RTT/s**（p99 0.0ms） | **actix +58%** |
| E2 背靠背自传递 | 68,788/s | **99,629/s** | **actix +45%** |
| E3 慢消费者 | 36.7s 全 drain（1090/s） | 37.0s 全 drain（1082/s） | 等价（受单 actor 串行上限约束） |
| E4 生命周期风暴 | 5000 spawn+stop 0.200s | 5000 spawn+stop 0.213s | 等价 |

### 第三轮关键发现

1. **饱和状态下的短任务保护（M1）**：两引擎在 12 个并发 CPU 任务饱和全部执行线程时，短探测任务延迟均保持亚毫秒级（thread 0.2ms / actix 1.4ms）——无饥饿。actix wall 快 48%（17.0s vs 25.0s，util 61% vs 41%），原因是 arbiter 池专职线程无共享调度队列的批次让出开销。**注意**：首轮未缩规模的探索性运行中，thread 引擎在 48 任务×4 倍膨胀 + 与编译进程 CPU 竞争下曾观测到 probe max 56s；纯净环境下不复现。结论：两引擎都应避免"长任务占用全部 worker/arbiter + 短任务共享调度队列"的部署形态，thread 引擎建议为长任务配置 DedicatedThread。
2. **actor 串行语义的代价（M2/M3）**：无论分片让出与否，同一 actor 的后续消息必须排队（两引擎一致，thread 9526ms / actix 9747ms）。**分片 yield 不能让消息插队**——这修正了第二轮报告的一个隐含假设（chunked 让出可改善响应性）；它只对跨 actor 调度公平性有意义。若需响应性，必须拆分 actor 或使用分片自 ask（拆到子任务消息中）。
3. **基准语义陷阱（实现层发现）**：thread 引擎的 `ActorRef::send` 是 ask 语义（5s 默认超时），actix 侧 `send` 实现为 do_send。M2 场景迫使压测代码显式区分"纯投递（None 超时）"与"ask 往返"——这是两引擎 API 语义的一个已知不对齐点（记录在案，详见 §九）。
4. **结构化吞吐优势（E1/E2）**：actix 在成对交互（乒乓 +58%）与自传递（+45%）上保持显著吞吐优势，与其在单次投递路径上的零拷贝通道优势一致。
5. **慢消费者等价（E3）**：单 actor 串行处理是物理上限（~1090 msg/s @ 200µs/条），双引擎差距 <1%。邮箱无界积压均正常（40k 消息无丢失）。
6. **生命周期成本可忽略（E4）**：5000 次完整 spawn→ask→stop 循环 0.2s（~25k/s），两引擎等价。

### 第三轮方法论说明

- **标定**：debug test profile 下 `burn_cpu` 实测 ~0.194G iters/s（2G iters=10.3s），BURN_RATE 常量记入 `engine_stress_common`。release profile 约 4×，换环境需重标定。
- **分钟级语义**：M1 的"分钟级任务"以 ~11.6s×8 并发任务表达（总 155 CPU·s > 2 分钟）；真 60s 单任务在首轮探索中已验证可行（46.6G iters 完成，仅 wall 膨胀）。
  - 首轮探索运行（未缩规模）：M1 8×60s 长 + 40×3.1s 中并发完成 wall=281.9s（thread，correct=false 系 ask 默认 5s 超时所致，修复后 correct=true）／286.1s（actix）。两者均完成全部计算，差异 <2%。
- **测量修正记录**：M3 首版探测发送目标错误（发给了空 actor），修正后双引擎结果均为"分片不插队"——旧数据（2.7s 插队收益）作废。
- **环境噪声控制**：thread 版曾与前台编译进程并行运行导致 util/M1 probe 严重失真（56s max），最终数据均为无竞争纯净运行。


---

## 第四轮（2026-10-02 下午）：release 重测 + 两项引擎级修复

### 修复 1：消息原语语义统一（ADR-10，原 §九遗留问题）

**问题**：`ActorRef::send` 在 thread 引擎是"带 5s 隐式默认超时的 ask"，在 actix 引擎是"无界 ask"；`send_with_timeout(None)` 在 thread 是纯投递、在 actix 是无界 ask——跨引擎语义漂移（本报告 §九记录）。

**修复**（`parrot-api/src/address.rs` + 双引擎实现）：

| API | 统一后语义 |
|---|---|
| `send` / `send_with_timeout(None)` | **无界 ask**：等到 actor 处理完返回结果，无隐式超时 |
| `send_with_timeout(Some(d))` | 有界 ask：超时后调用方放弃（消息仍留在队列） |
| `deliver`（新增） | **纯投递**：mailbox 接受即返回，显式 tell 路径 |
| `ActorRefExt::tell` | 内部改走 `deliver`（不再产生无用 ask 等待） |

**回归适配**：依赖"send 默认超时报错"的 3 个单测改为显式 `send_with_timeout(Some(d))`；新增 `test_send_unbounded_receives_reply_when_answered` 验证无界路径。

### 修复 2：thread 引擎弹性扩容（ADR-11）+ 双层饥饿根因治理

**M1 release 复测暴露的真相**（8×65s 真分钟级长任务 + 40×2.7s 中任务 + 200ms 间隔短探测）：

| 版本 | M1 probe max | M1 wall / util | 根因 |
|---|---|---|---|
| 修复前（release） | **67492ms**（等长任务结束） | 105.1s / 40% | 双层饥饿（见下） |
| 修复后（release） | **0.9ms** | 70.9s / 59% | — |
| actix 对照 | 9.0ms | 70.1s / 60% | 专职 arbiter 天然免疫 |

**双层饥饿根因与治理**：

1. **第一层（调度器自饿）**：CPU 密集 handler future 经 `tokio::spawn` 跑在 8 个 runtime worker 线程上；12 个 CPU 任务全部占线后，弹性巡检/邮箱唤醒等**引擎自身的 tokio task 全部饿死**——调度器失去自救能力。debug 版"通过"纯属 E 核调度巧合。
   → **修复**：`worker.rs` 中 handler future 改经 `spawn_blocking`（专用阻塞线程池，默认上限 512），runtime 线程永不被 CPU handler 占据。
2. **第二层（池满饿死）**：全部 core worker 被长任务占满时，短任务在调度队列中无人处理。
   → **修复**：`ElasticController` 弹性 burst worker：
   - 触发：队列积压持续 ≥`burst_backlog_threshold`（默认 100ms）且无 idle core worker；**wake_hook + 周期巡检双探测**（巡检周期 = threshold/2，解决"等回复的 caller 不再发消息 → probe 断流"的鸡生蛋死锁）
   - 扩容：每次 probe 最多 +1（`compare_exchange` 保证 backlog 首时刻不被覆盖——首版 `swap` bug 已修）
   - 收缩：burst worker 空闲 ≥`burst_idle_timeout`（默认 5s）由 reaper 置专属 shutdown flag 优雅退出（不 mid-batch abort）
   - **全局上限**：`pool_size + burst_workers_max`（默认各 num_cpus），线程永不爆炸
   - 配置：`ThreadActorSystemConfig::shared_burst_{workers_max,backlog_threshold_ms,idle_timeout_ms}`
   - 可观测：`SchedulerMetrics.burst_workers_alive`（经 `ThreadActorSystem::scheduler_metrics()`）

**专项测试**（`parrot/tests/test_elastic_scaling.rs`，3 用例全过）：
- 饱和保护：2 core worker 被 8s CPU 任务打满时 200 个短任务 max 延迟 **149ms**（修复前 7055ms，47×）
- 收缩：压力解除后 burst 被收割，二次打满仍可救援（128ms）
- 上限：20 长任务风暴下调度线程峰值 = 上限值，无爆炸

### 第四轮 release 核心数据（21 场景全过，thread 206.2s / actix 201.5s）

| 场景 | Thread | Actix | 结论 |
|---|---|---|---|
| **M1 混合分钟级** | wall=70.9s util=59% probe p99=0.3ms **max=0.9ms** | wall=70.1s util=60% probe max=9.0ms | **等价且 thread 反超**（弹性+阻塞池修复后） |
| M2 同 actor FIFO | 51.4s | 49.5s | 等价（单 actor 串行物理上限） |
| M3 分片 vs 连续 | solid 33.4s / chunked 33.4s，tiny 等 33418ms | solid 33.7s / chunked 33.7s，tiny 等 33650ms | 等价：分片不插队（两引擎一致的串行语义） |
| E1 乒乓 | 82.9k/s | **169.7k/s** | actix +105% |
| E2 自传递 | 89.7k/s | **165.6k/s** | actix +85% |
| E3 慢消费者 | 4853/s（8.2s drain） | 5164/s（7.7s） | 等价（actix +6%） |
| E4 生命周期 | 26.7k/s | 25.7k/s | 等价 |

> release 标定：`burn_cpu` 0.88 G iters/s（debug 的 4.5×）。BURN_RATE 常量已更新为 release 值。
> **回归方法学**：压测套件已标 `#[ignore]`——日常 `cargo test` 跳过（避免 7 分钟拖累），压测显式跑：`cargo test -p parrot --release -- --ignored --nocapture`。

### 第四轮复测（同日第三次独立运行，验证稳定性）

| 场景 | Thread | Actix | 稳定性结论 |
|---|---|---|---|
| M1 混合分钟级 | wall=70.5s util=59% probe p99=0.6ms **max=3.2ms**（350 probes） | wall=70.7s util=59% probe p99=0.1ms max=3.2ms（350 probes） | **三次运行 thread M1 max：0.9 → 3.2ms，actix：9.0 → 3.2ms——两引擎收敛到同一亚毫秒档，结果可复现** |
| M2 同 actor FIFO | 51.1s | 50.3s | 波动 <2% |
| M3 分片 vs 连续 | 68.3s（tiny 等 34080ms） | 68.0s（tiny 等 33962ms） | 波动 <1% |

### 最终选型结论（release，修复后）

| 场景 | 推荐 | 依据 |
|---|---|---|
| 分钟级 CPU + 混合负载（响应敏感） | **两者等价** | thread 修复后 probe max 0.9ms vs actix 9.0ms，wall/util 持平（59%/60%） |
| 成对交互/自传递吞吐 | **Actix** | 乒乓 +105%、自传递 +85%（专职 arbiter 零调度队列开销） |
| 慢消费者积压 | 两者等价 | 单 actor 串行物理上限主导，差距 <7% |
| 长任务治理 | **Thread** 更可控 | 弹性 burst + DedicatedThread + spawn_blocking 三层机制化隔离；actix 需手工 arbiter 规划 |

---

## 八、测试资产

- 压测套件（已 `#[ignore]`，显式运行）：`cargo test -p parrot --release -- --ignored --nocapture`
- 弹性扩容专项：`cargo test -p parrot --release --test test_elastic_scaling -- --nocapture`（3 用例）
- **21 个场景**覆盖：延迟/吞吐/并行/饥饿/边界/混合负载/生命周期（超时、stop、洪泛、20000 海量 actor、分钟级混合、慢消费者、spawn 风暴）
- 规模对齐 akka 级：CPU 并行 64 actor、herd 20000、IO 64 actor × 1280 任务、慢消费者 40k 积压
- 全部修复已回归：workspace 测试 0 失败

## 九、API 语义不对齐（第三轮发现，**第四轮已解决 → 见"修复 1"与 ADR-10**）

> 下表为**修复前**的历史记录，仅作根因存档。当前语义：`send`/`send_with_timeout(None)` = 无界 ask（两引擎一致）；`send_with_timeout(Some(d))` = 有界 ask；`deliver`（新增）= 纯投递；`tell` 内部走 `deliver`。

| API（修复前） | Thread 引擎 | Actix 引擎 | 影响 |
|---|---|---|---|
| `ActorRef::send` | **ask 语义**（内部走 ask 通道，默认 5s 超时，返回 handler 结果） | **do_send 语义**（投递即返回，不等待处理） | 跨引擎代码若依赖 `send` 的返回值/超时行为，语义漂移。M2 场景被迫双路径适配 |
| `ActorRefExt::tell` | tokio::spawn 内走 send（即 ask），消息入队但等待方 5s 放弃 | 同左（spawn 内 do_send） | tell 语义两引擎一致（fire-and-forget），但 thread 版会创建无用等待 |

~~建议：统一 `send` 为"纯投递返回回执"或"ask"其一，并在 trait 文档显式声明。~~ ✅ 已按"ask"统一并新增 `deliver`，trait 文档已显式声明（`parrot-api/src/address.rs`）。

---

## 第五轮（2026-10-02 晚）：Thread 引擎结构性优势专项实证

> 触发问题："thread 模式的所有长处是什么？在哪些场景、领域能超过 actix？"——本轮不再比较"相同场景谁快"，而是**识别 thread 引擎具备而 actix 架构上不具备的能力**，并用 `parrot/tests/test_thread_advantages.rs`（A1–A5）逐项实证。

### 5.1 修复 3：actor 级 backpressure_strategy 配置链路断裂（本轮发现并修复）

**现象**：A2/A3 首跑 0 拒收——邮箱容量与策略配置看似无效，200k 条消息全部入队。
**根因**：`system.rs` 中两处 `ThreadActorRef::new` 用的是**系统级默认策略**（`Block`），`ThreadActorConfig.backpressure_strategy` / `mailbox_capacity` 虽然正确构造了有界 flume 邮箱，但 push 策略从未从 actor 配置注入：
- 返回给用户的 typed ref（spawn_at 返回值）
- 注册表 BoxedActorRef（resolve 拿到的引用）

**修复**：两处均改为 `thread_config.backpressure_strategy.clone().unwrap_or_else(|| system_default)`（含 `ask_timeout` 同样处理）。

**修复后 A3 数据**：bounded(256)+Error，200k pushes → **accepted=256 rejected=199744**，push 阶段 0.03s 完成，RSS 增长 44.3MB。修复前同场景：200k 全收、push 耗时 23.33s（被 Block 策略拖住等消费者）、无界积压。

### 5.2 Thread 引擎结构性优势矩阵（分析 + 实证结论）

| # | 优势 | Thread 引擎机制 | Actix 架构限制 | 实证（release） |
|---|---|---|---|---|
| A1 | **DedicatedThread 独占线程隔离** | 1 actor = 1 专属 OS 线程 + SPSC ringbuf 私有邮箱，任务永不跨 actor 抢线程 | Actor 绑死单个 Arbiter；N 个重 actor 可能哈希到同核互相拖慢，无独占线程概念 | ✅ 4×4.5s 重任务并行 wall=4.95s（串行需 18s）；共享池 100 探针 max=0~1ms 完全不受扰 |
| A2 | **细粒度背压策略矩阵** | 邮箱级 `Block/Error/DropOldest/DropNewest` 四策略，per-actor 可配 | 邮箱无界（`mbox` 无界链表）；溢出只能靠外部限流，无原生丢老/丢新 | ✅ Error：12/20 拒收（mailbox=8）；DropOldest：mailbox=4 灌 10 条，最终处理最新消息（processed=6 events） |
| A3 | **内存防护（OOM 免疫）** | 有界邮箱 + Error 拒收 = 生产者压不垮消费者 | 无界邮箱 + 慢消费者 = 无限积压，生产端失控即 OOM | ✅ 200k 灌入 0.03s 完成，199,744 条拒收，RSS +44MB 稳定（对比修复前无界 Block 23s / 无界积压） |
| A4 | **宿主 runtime 零侵入嵌入** | 无 System/Arbiter 设施，`ThreadActorSystem::shared()` 直接寄生宿主 tokio runtime | 必须初始化 `actix::System`（或其 runtime 内）才能 spawn/通信 | ✅ 无任何 System 直接在宿主 4-worker runtime 上 ask 往返成功 |
| A5 | **海量 actor 创建吞吐** | spawn 只创建数据结构 + 注册（无线程/任务启动成本） | 每个 actor spawn 需要 Arbiter 分配 + enqueue + context 构建 | ✅ 20,000 actors / 0.034s = **592,844 actors/s**（actix 同机 ~324k/s，+83%） |
| B1 | 工作窃取式负载均衡 | 中央 SegQueue + 弹性 burst workers，非均匀负载自然摊平 | Arbiter 轮转分配是静态的，重 actor 扎堆同 Arbiter 无法迁移 | 第四轮 M1 已证：12 长任务 + 短任务混跑，probe max 3.2ms，利用率 59% 持平 |
| B2 | 弹性扩缩容 | `ElasticController`：积压超阈值自动 spawn burst workers，闲置自动回收，全局线程预算封顶 | Arbiter 数固定（=CPU 核数），饱和即排队 | 第四轮 `test_elastic_scaling.rs` 已证 |
| B3 | 阻塞隔离 | 重 CPU handler 走 `spawn_blocking`，不占 Tokio worker 线程 | 同一 Arbiter 上长任务会拖住全部同居 actor（除非手工 offload） | 第三/四轮 starve 系列已证 thread 不再饿死旁路短任务 |

### 5.3 结论

1. **五大结构性优势全部实证成立**（A1–A5），其中 A2/A3 的实证过程直接挖出并修复了配置链路缺陷（修复 3）。
2. Actix 在"纯异步 IO 密集、均匀负载、无背压诉求"的通用场景仍是吞吐冠军（arbiter 专职线程少一层调度）；Thread 引擎在**重 CPU 混合负载、延迟敏感隔离、内存可控性、海量实体、嵌入宿主**五类领域结构性占优。
3. 修复 3 后全量回归 457 passed / 0 failed（含新增 5 项优势验证）。

---

## 第六轮（2026-10-02 晚）：三引擎对等横评 —— Thread vs Actix vs Akka Typed

> 用户问题："与 Akka 的单机性能比较"。用 **Akka 2.6.20 Typed（javadsl，最后一个 Apache-2.0 版本）** 实现与 Rust 基准**逐场景对等**的逻辑：相同消息语义（ask/tell）、相同 actor 数/并发度/每条消息 CPU 迭代数、相同 burn_cpu 内核（LCG 常数一致）、相同分位数算法。代码位于 `bench/akka-bench/`（`./run.sh` 一键编译运行；2026-10-02 起与 `bench/actix-bench/` 同居 `bench/` 基准目录）。

### 6.1 环境与口径

| 项 | thread | actix | akka |
|---|---|---|---|
| 运行时 | tokio 8-worker（共享池 15 核+burst） | 15 arbiter 专职线程 | JVM Temurin 21.0.9, ZGC, fork-join parallelism=15 |
| 编译 | rust release | rust release | javac 默认 + JIT（warmup 3000 ask 后才计时） |
| 逻辑 | `engine_stress_thread.rs` | `engine_stress_actix.rs` | `AkkaBench.java`（20 场景全对等） |

三轮同机连续运行（M1/M2/M3 等分钟级场景各跑一遍保证可比）。

### 6.2 全量对比（release / JIT 后）

| 场景 | thread | actix | akka | 最优 |
|---|---|---|---|---|
| seq-ask-echo-1k | 76k/s p99=0.00ms | 95k/s | 97k/s p99=0.03ms | **akka** |
| conc-ask-echo-c8-m1000 | **418k/s** p99=0.00ms | 364k/s | 234k/s p99=0.08ms | **thread** |
| conc-ask-echo-c64-m200 | 439k/s p99=0.20ms | **474k/s** p99=0.20ms | 272k/s p99=0.33ms | **actix** |
| tell-echo-100k | 88k/s | 165k/s | **1.3M/s** | **akka** |
| cpu-serial-200x200k | 4k/s p99=0.30ms | 4k/s p99=0.30ms | 4k/s p99=0.52ms | actix（微差） |
| cpu-parallel-8actors-200k | **51k/s** p99=1.50ms | 48k/s p99=2.40ms | 46k/s p99=1.05ms | **thread** |
| flood-500k-tell | 319k/s | 370k/s | **1.9M/s** | **akka** |
| ask-timeout-short | ✓ | ✓ | ✓ | 语义一致 |
| send-after-stop | ✓ | ✓ | ✓ | 语义一致 |
| herd-20000-actors | **789k/s** | 322k/s | 117k/s | **thread** |
| longrun-2G-iters | 2.37s | **2.25s** | 2.25s | actix/akka 并列 |
| starve-echo-during-longrun | 1.50s (p99 0.1ms) | **1.50s** (p99 0.1ms) | 1.52s (p99 0.10ms) | 三方持平 |
| io-async-64actors-10ms | 0.85s | **0.25s** | 0.28s | **actix** |
| mixed-minute-cpu-plus-incoming | 70.7s util=59% probe max 3.2ms | 70.7s util=59% probe max 3.2ms | **68.8s** probe max 7.3ms | **akka** |
| mixed-same-actor-fifo | **50.25s** | 50.27s | 50.49s | 三方持平（FIFO 语义强制） |
| chunked-vs-solid-longrun | 67.70s | 67.83s | 67.92s | 三方持平（CPU 主导） |
| pingpong-rtt-10k | 83k/s | **152k/s** | 129k/s p99=0.03ms | **actix** |
| self-chain-ask-tell-20k | 88k/s | 165k/s | **229k/s** | **akka** |
| slow-consumer-8prod-40k | 8.23s | 8.05s | **7.72s** | **akka**（微差） |
| spawn-stop-storm-5k | **26k/s** | 25k/s | 23k/s | 三方持平 |

数值最优场次：thread 6 / actix 6 / akka 6。

### 6.3 分域解读

1. **Tell/纯投递吞吐：Akka 碾压（1.3M~1.9M/s，8~15 倍）**。Akka 的 `tell` 是无界邮箱上一次 CAS 入队，无 envelope 分配、无 future、无 waker；thread 引擎的 `send` 是 ask 语义（建 oneshot + 等待），actix 的 do_send 走 wrapper 装箱。**这是语义代价，不是调度缺陷**——parrot 的 `deliver`（纯投递）也在第五轮测过，但仍含 BoxedMessage 装箱开销。
2. **并发 ask（中低竞争 c8）：thread 最快（418k/s）**——中央队列+oneshot 派发在高并发等待者下摊薄；Akka 的 ask 要过 `Scheduler`+`CompletableFuture` 两层，c8 下垫底（234k/s）。**极高竞争（c64）actix 反超**（专职 arbiter 线程无池化调度开销）。
3. **海量 actor：thread 碾压（789k/s vs akka 117k/s，6.7 倍）**。Akka spawn 要走 guardian→cell→mailbox 链路且受 dispatcher 限流；thread 引擎 spawn 仅构造数据结构+注册。**海量短命实体场景（游戏单位、IoT 会话）thread 是三者最优**。
4. **CPU 混合长负载（M1）：Akka 总时长最优（68.8s）但探针尾延迟最差（max 7.3ms vs 3.2ms）**——JIT 后 burn 循环略快 + fork-join 抢占式线程调度摊平队列；代价是 GC/调度抖动落在尾部分布。thread/actix 的 probe max 更稳。
5. **IO 密集：actix 最优（0.25s ≈ 理论下限 0.2s）**，thread 0.85s——thread 引擎的 handler 经 spawn_blocking 执行，10ms sleep 场景下多一层阻塞池跳转；akka 0.28s（stash 模拟独占窗口）。**纯异步 IO 高并发仍是 actix 主场**。
6. **FIFO/长任务类（M2/M3/longrun/starve）三方持平**——这些场景由 CPU 总量与邮箱 FIFO 语义主导，引擎差异被淹没（<1%）。

### 6.4 结论

- **三引擎各有一个"统治域"**：Akka=纯投递吞吐与稳态 JIT 算力；actix=高竞争 ask 与异步 IO；thread=海量实体创建与中竞争 ask。
- **parrot 双引擎组合覆盖了 Akka 的多数优势域**（tell 高吞吐→actix 引擎；海量实体→thread 引擎），且在 Akka 最弱的**尾延迟稳定性**（M1 probe max 3.2ms vs 7.3ms）与**背压/隔离能力**（Akka 无界邮箱，无 DedicatedThread）上结构性领先。
- Akka 基准资产保留在 `bench/akka-bench/`（`run.sh`），可复现。


---

## 第七轮（2026-10-02 晚）：亲和性调度器 + 分配优化 + 新优势域

### 7.1 Sharded（亲和性）调度器（ADR-14）—— 回应"强绑定固定线程池"

**架构**：`ShardedScheduler`（`scheduler/sharded.rs`）= N 个固定 OS 线程 × 每线程独占一条 MPSC 分片队列。actor 以 `SchedulingMode::Sharded { affinity_key }` 指定亲和域，按 FNV-1a+splitmix64 哈希到分片后**永不迁移**；消息到达经 wake-hook 直推分片队列，**完全绕过中央 SegQueue**。

**优势分析（对照"少一层等待"的预期）**：
1. **消除全局队列争用**：SharedPool 每消息一次全局 SegQueue CAS（所有 worker 争抢同一 cache line）+ Notify 唤醒；Sharded 只有一次线程私有 MPSC push——稳态下目标线程已在自旋等待，**无唤醒延迟**。
2. **缓存局部性**：同亲和域消息恒在同一线程处理，actor 状态驻留 L1/L2；跨 actor 同域消息也共享线程栈热度。
3. **尾延迟隔离**：分片间零干扰（实测 S2：4×9s 重 CPU 打满一域，另一域探针 max=**48µs**）。
4. **代价**：无 work-stealing，分片负载不均时无法互救（适合已知亲和域且大致均衡的场景：分片状态、分区 topic、会话粘滞）。

**实测**（`tests/test_sharded_scheduler.rs`，release）：

| 场景 | Sharded | SharedPool | 提升 |
|---|---|---|---|
| S1 8 actor×20k ask（4 亲和域） | **380k/s** | 216k/s | **1.76×** |
| S2 邻域重载下的探针 | p99=45µs max=48µs | —（共享池会被拖入同队列） | 隔离性 |

### 7.2 消息双分配确认与优化（ADR-13）

**确认**：优化前一次 ask 恰好 **3 次堆分配**：
1. 调用方 `Box::new(payload)`（`BoxedMessage` 装箱）
2. `AskEnvelope::new` 内 `Box::new(ThreadReplyChannel(tx))`（trait 对象装箱）
3. 调用方 `Box::new(envelope)`（envelope 二次装箱进 `BoxedMessage`）

**优化**：`AskEnvelope.reply` 改为**内联** `oneshot::Sender`（消除 #2）；新增 `new_typed` 构造器把未装箱消息直接移入 envelope（调用方可避免 #1 与 #3 重复）；回复路径改同步 `send`（消除 async trait 虚调用）。优化后一次 ask = **1 次分配**（+payload 本身）。

**收益实测**：c64 并发 ask 439k→**494k/s**（+12%）、herd 789k→**825k/s**、pingpong/self-chain 持平（这些路径瓶颈在驱动侧而非分配）；allocator 压力（每消息 malloc/free 次数）降 2/3，长期运行碎片更少。actix 侧的 `MessageEnvelope` 每条消息还含 `uuid::new_v4()`（v4 需读 CSPRNG），是另一处可优化点（thread 引擎无此开销）。

### 7.3 新优势域盘点（任务 4：业界/编程场景扫描）

| 领域 | 场景 | 为什么是 thread 引擎优势 |
|---|---|---|
| **游戏服务端** | 大量单位/会话实体、状态机 actor | herd spawn 825k/s（akka 117k/s 的 7 倍）；DedicatedThread 隔离逻辑帧线程 |
| **IoT/边缘网关** | 千级设备连接、每设备状态 actor | Sharded 亲和域=设备分区，消息直达无全局队列；有界邮箱防背压雪崩 |
| **金融风控/撮合** | 订单簿分片、关键路径微秒级延迟 | Sharded p99=45µs + 分片隔离；DedicatedThread 给撮合核心独占线程 |
| **电信/网络功能** | SIP/DPI 会话粘滞处理 | Sharded 会话亲和（同用户同线程）；S2 证明邻域重载零干扰 |
| **嵌入式/资源受限** | 无 JVM、无 actix System 设施 | A4 宿主零侵入嵌入；spawn 无线程成本 |
| **批处理/科学计算** | 分钟级 CPU 段落 + 短任务混合 | M1 probe 3.2ms（akka 7.3ms）尾延迟稳定；弹性 burst 池 |
| **流处理拓扑** | 分区 topic → 分片 worker | Sharded = Kafka partition 语义的 actor 化 |
| **可作为库嵌入** | 中间件/数据库内核内建 actor 子系统 | 无 System 依赖（akka 需 ActorSystem、actix 需 System 上下文） |

**共同主题**：thread 引擎的护城河是**结构可控性**——线程拓扑（shared/dedicated/sharded 三模式）、邮箱（有界+四背压策略）、内存（分配次数/上限）全部显式可配；akka/actix 在各自默认路径上快，但用户无法重塑这些结构。


---

## 第八轮（2026-10-02 晚）：消息池化论证 + Work-Stealing 现状审计

### 8.1 Q1：消息消费后缓存复用（ADR-15）—— 已实现 `message_pool.rs` 并实测

**论证（先例 + 约束）**：LMAX Disruptor（槽位复用 100M ops/s）、JVM TLAB（Akka tell 1.9M/s 的本质）、Netty/Kafka buffer pool 均验证该思路。但 `Box<dyn Any>` 架构下消费点在用户 `receive_message` 内按值 drop，引擎无法拦截——因此设计为**可选 `Pooled<T>` wrapper**（Drop 归还 thread-local per-TypeId free-list）+ 上限控制（每类型 128 / 最多 256 类型 / 大对象不入池）+ Disruptor 式"内容为脏"契约。

**实测**（`tests/test_message_pool.rs`，black_box 防优化）：

| 场景 | 全局 allocator | 池化 | 结论 |
|---|---|---|---|
| P1 串行 1M alloc+drop | 26ns/op | 19ns/op | **1.37×** |
| P2 4 线程并发 2M | 20.1ms | **7.1ms** | **2.83×** |

**过程中的重要发现（记入 ADR-15）**：池的首版用全局 atomic 计数器，P2 并发反而**慢 3.4×**（比 malloc 还差）——计数器争用完全吞掉池收益。改 thread-local 计数后反超。**教训：池化组件上任何跨线程原子操作都是毒药**。使用建议：高频小消息（tick/传感器/行情）值得池化；ask 路径 p50 30µs 下单次分配节省 ~25ns 占 0.08%，延迟无感，收益在**分配速率压低**（碎片、page 驻留）与并发场景。

### 8.2 Q2：Work-Stealing 现状审计 + 基础设施（ADR-16）

**现状确认**：源码审计（`queue.rs` / `shared/mod.rs` 注释宣称 "work-stealing"）——**名不副实**。实际是全局单 `SegQueue` 集中 push/pop：所有 worker 争抢同一队列（每 pop 一次跨核 cache line 争用），空闲 worker 无法从忙 worker 手里取活，只能等中央队列投喂。这是 tokio 早期（0.1）之前就淘汰的形态。

**已建基础设施**：`scheduler/steal.rs`——chase-lev 式无锁 `StealDeque<T>`（ArrayQueue 槽位 + owner/steal 双端语义）+ `StealRing` 拓扑（本地优先 → 随机两跳窃取）。并发正确性测试（4 线程 5 万条无丢失无重复）通过。

**微基准对比**（`tests/test_steal_bench.rs`，8 生产者+8 worker×3 秒，三轮稳定）：

| 方案 | 吞吐 | worker 均衡度 |
|---|---|---|
| Q-A 集中 SegQueue（现状） | 1060k/s | max/min = **1.05×（优秀）** |
| Q-B 本地队列+窃取 | 1050k/s | max/min = **7.7×（本基准下劣化）** |

**反直觉结论（诚实记录）**：本引擎的调度粒度是 **mailbox（批次）而非细粒度任务**——中央队列在"粗粒度均衡投喂"下天然公平（1.05×），而本地队列+窃取在 producer 亲和投递模拟中反而失衡 7.7×（窃取的两跳随机探测大部分落空）。**吞吐持平（1060k vs 1050k）**。因此：`steal.rs` 作为基础设施保留（供未来细粒度调度与 burst worker 就近取活），但**不默认替换中央队列**——本轮实证否决了"集中队列是瓶颈"的假设。真正的调度收益已由 Sharded（1.76×）实现，其思路（消除中央队列）与 work-stealing（优化中央队列）是两条路线，前者在本架构的粒度模型下胜出。


---

## 第九轮（2026-10-02 晚）：正确性专项验证 + 三引擎终测

### 9.1 正确性专项套件（`tests/test_correctness_suite.rs`，新增）

**审计结论**：此前压测的 `correct=true` 主要证明"完成度"（消息数对/未超时），仅 4 处相等断言。本轮补齐八类语义不变量，全部通过：

| # | 不变式 | 断言内容 | 结果 |
|---|---|---|---|
| C1 | **恰好一次** | 8 发送者×2000 条=16000 事件，HashSet 去重后仍 16000（零丢失/零重复） | ✓ |
| C2 | **每发送者 FIFO** | actor 逐条处理 ⇒ 每个发送者子序列 seq 严格 +1 递增 | ✓ |
| C3 | **计算完整性** | burn_cpu 返回值与参考实现逐条一致（ask 回复端到端 + 日志重放双路校验） | ✓ |
| C4 | **回复路由无串扰** | 256 并发 asker×3 轮，每个 token 原值返回 | ✓ |
| C5 | **跨模式等价** | SharedPool/DedicatedThread/Sharded 三模式同 500 条输入 → 输出序列全等且=参考重放 | ✓ |
| C6 | **背压精确性** | Error：cap=4 恰好收 4 拒 16（Full 错误）；DropOldest：8 条进 cap-4 保留最新 [4,5,6,7] | ✓ |
| C7 | **停止语义** | stop 后 ask 失败 + 事件数不再增长（无幽灵处理） | ✓ |
| C8 | **超时语义** | 空闲 50ms 超时成功即返；阻塞时 5ms 超时 7ms 内返回 Timeout 错误 | ✓ |

（C6 首跑暴露测试时序缺陷——DropOldest 断言早于堵门任务完成，修正等待条件后通过；引擎行为本身正确。）

### 9.2 终测总表（全部 release，同机连续运行；全量回归 467/467）

20 场景 × 3 引擎全部 `correct=true`。数值最优：thread 6 / actix 5 / akka 7。

| 场景 | thread | actix | akka | 最优 |
|---|---|---|---|---|
| seq-ask-echo-1k | 50k/s | 78k/s | 101k/s | akka |
| conc-ask-echo-c8-m1000 | **366k/s** | 365k/s | 233k/s | thread |
| conc-ask-echo-c64-m200 | **509k/s** | 470k/s | 284k/s | thread |
| tell-echo-100k | 89k/s | 158k/s | **1.26M/s** | akka |
| flood-500k-tell | 326k/s | 366k/s | **1.92M/s** | akka |
| cpu-serial-200x200k | 4k/s | 4k/s | 4k/s | 持平 |
| cpu-parallel-8actors-200k | **52k/s** | 50k/s | 45k/s | thread |
| herd-20000-actors | **847k/s** | 313k/s | 116k/s | thread（7.3×） |
| longrun-2G-iters | 2.29s | 2.26s | 2.28s | 持平 |
| starve-echo-during-longrun | 1.50s | 1.50s | 1.51s | 持平（probe p99 均 0.1ms） |
| io-async-64actors-10ms | 0.86s | **0.24s** | 0.25s | actix |
| mixed-minute-cpu-plus-incoming | 70.96s（probe max 3.2ms） | 71.71s | **68.61s**（probe max 7.3ms） | akka 时长 / thread 尾延迟 |
| mixed-same-actor-fifo | **49.98s** | 50.59s | 50.46s | thread |
| chunked-vs-solid-longrun | **67.63s** | 68.38s | 68.40s | thread |
| pingpong-rtt-10k | 83k/s | **153k/s** | 125k/s | actix |
| self-chain-ask-tell-20k | 89k/s | 153k/s | **230k/s** | akka |
| slow-consumer-8prod-40k | 8.19s | 7.88s | **7.80s** | akka（微差） |
| spawn-stop-storm-5k | 27k/s | 25k/s | 27k/s | 持平 |
| ask-timeout-short / send-after-stop | ✓ | ✓ | ✓ | 语义一致 |

### 9.3 多轮优化净收益（thread 引擎，vs 第六轮基线）

| 优化 | 场景 | 前→后 |
|---|---|---|
| ADR-13 分配消除 | conc-ask-c64 | 439k→**509k/s（+16%）** |
| ADR-13 + spawn 路径 | herd-20000 | 789k→**847k/s（+7%）** |
| ADR-14 Sharded | 亲和域 ask（S1） | 216k→**380k/s（+76%）** |
| ADR-14 Sharded | 邻域重载隔离（S2） | probe p99 **45µs**（共享池会被拖入同队列） |
| ADR-15 池化 | 并发 alloc/drop（P2） | 20.1ms→**7.1ms（2.83×）** |

### 9.4 终局结论

1. **正确性**：8 类不变量 + 3 引擎 20 场景 correct + 474 全量回归（含第十轮 B 系列业务逻辑 7 项），三层验证全绿。
2. **性能格局**：thread 统治"结构可控域"（herd 7.3×、c64 ask、亲和隔离、混合 FIFO）；akka 统治纯投递（1.9M/s，无装箱语义代价）与 JIT 稳态算力；actix 统治高竞争 ask 微差与异步 IO。
3. **parrot 双引擎组合**在各场景选对引擎后，除纯投递吞吐（受 ask 语义装箱限制，设计取舍）外全面覆盖 akka/actix 优势域，并在尾延迟稳定性、背压控制、线程拓扑可控性上结构性领先。

---

## 第十轮（2026-10-02 晚）：业务逻辑正确性专项验证（B 系列）

第九轮 C1–C8 验证的是**引擎语义层**（消息恰好一次、FIFO、回复路由），第十轮补齐用户指出的**业务逻辑层**：actor 收到消息后，内部业务状态变迁、计算结果、副作用是否**处理正确** —— 而不只是"收到了"。

### 10.1 方法论：模型对照 + 守恒不变量

- **模型对照（model-based testing）**：测试侧用独立实现的业务期望模型重放同样规则，与 actor 实际状态/回复精确比对（B2/B5/B7 逐位一致；actor 内外两种独立写法语义等价，非复制粘贴）。
- **守恒不变量（conservation invariants）**：并发交错下业务结果组合非确定，但守恒律不变 —— 资金守恒、库存守恒（补偿零泄漏）、计数守恒。这是并发业务正确性的强验证方式（B1/B4）。
- **错误契约**：业务失败必须是**类型化业务回复**（可 downcast），区别于协议错误的引擎异常；失败后状态零污染（B3/B6）。

### 10.2 B1–B7 场景与结果（`parrot/tests/test_business_logic.rs`，全绿）

| # | 业务场景 | 验证的业务逻辑正确性 | 结果 |
|---|---------|---------------------|------|
| B1 | 银行账户（8 客户端 × 400 笔并发存取） | 余额守恒：`balance = initial + Σ存款 − Σ成功取款` 精确成立；200 次透支全拒且状态不污染 | ✓ 1000+79600−43200=37400 |
| B2 | 聚合统计（10,000 并发值提交） | sum/min/max/n 与数学期望**逐位一致**；恰好一次计数 | ✓ n=10000 sum=−4990 min=−1000 max=1000 |
| B3 | 订单状态机 | 合法链 Created→Paid→Shipped→Delivered 全生效；非法跳变（Created→Ship、Delivered→Cancel）拒绝且状态保持不变 | ✓ |
| B4 | 三 actor Saga 流水线（下单→库存→支付，200 并发订单） | 编排 actor 内嵌套 ask 的多 actor 业务事务：30 确认 / 70 缺货 / 100 资金不足全部**补偿回滚**；资金守恒 900→{0,5,10} 与台账逐笔吻合；库存零泄漏（widget 60/60，gadget 0/0）；每单恰好一个回执 | ✓ 5 连跑全绿（每轮组合不同、守恒恒成立） |
| B5 | 文本规范化服务 | 10 组刁钻输入（大小写/重复/Unicode/空串/标点）结果与参考实现逐条一致 | ✓ |
| B6 | 错误契约 | 透支→`InsufficientFunds` 类型化业务回复（非引擎异常）；拒后精确余额取款仍成功；未知消息→引擎级错误 | ✓ |
| B7 | 副作用审计流 | 500 事件：每个业务决策被审计 actor **恰好一次、按序**记录，审计日志与业务结果序列逐一对应 | ✓ |

### 10.3 关键发现：并发业务测试的正确姿势

B4 开发过程中抓到一个**测试方法论缺陷**（非引擎缺陷）：最初按"发起顺序"重放模型，因 orchestrator 邮箱并发到达顺序非确定而误报。修正为：

1. 每单在 orchestrator 内获得单调处理序号（actor 单线程 = 天然串行化点）；
2. 守恒不变量对交错不敏感，作为主断言；
3. 结果组合（哪 30 单成功）随调度变化是**合法的**，守恒断言每轮都精确成立。

这本身就是 actor 模型业务正确性验证的范式：**结果依赖顺序，守恒不依赖顺序**。

### 10.4 全量回归

`cargo test --workspace --release`：**474 passed / 0 failed**（467 既有 + 7 新增业务逻辑测试）。

### 10.5 结论

业务逻辑层与引擎语义层双层正确性验证齐备：
- **引擎层**（C1–C8）：消息恰好一次送达、FIFO、回复路由、背压精确拒绝。
- **业务层**（B1–B7）：actor 拿到消息后业务状态变迁正确、计算精确、Saga 补偿零泄漏、错误契约清晰、副作用恰好一次有序审计。

---

## 第十一轮（2026-10-02 晚）：纯 Actix 基准 —— parrot 包装税量化

### 11.1 动机与设置

用户要求：akka 有独立压测目录，actix 也应有**平级的纯 actix 压测目录**，以验证 parrot 包装一层之后 actix 的性能是否有损失。（后续整理：两目录统一收纳进 `bench/` 基准目录。）

新增纯 actix 基准（独立 workspace、零 parrot 依赖；现位于 `bench/actix-bench/`，与 `bench/akka-bench/` 平级同居 `bench/`）：

- **场景逐字节对等**：22 个场景与 `engine_stress_actix.rs` 一一对应——相同消息语义/数量/CPU 迭代数（burn_cpu 同 LCG 常数）、相同分位数算法（ceil 索引）、相同 arbiter 拓扑（复刻 `ArbiterPool` round-robin，否则"包装税"会混入"拓扑差异"）、相同调用语义（parrot `send()`= `addr.send().await`）。
- **唯一差异**：不经 parrot——原生 `Handler<M>` 静态分发，无 `BoxedMessage` 装箱、无 `MessageEnvelope`+`Uuid::new_v4()`、无 `AtomicResponse` 回复装箱、无 downcast 链、无注册表写锁。
- 额外附加 `*-dosend` 变体：原生 `do_send` fire-and-forget 上限参考（parrot 压测口径的 tell 走 `send().await` 往返语义，二者不可直接比）。

运行：`cd bench/actix-bench && cargo run --release`（或 `./run.sh`）；报告落 `/tmp/parrot_bench_actix_raw.md`。

### 11.2 同机背靠背实测（parrot 包装 vs actix-raw）

| 场景 | parrot 包装 (/s) | actix-raw (/s) | 包装税 | 判定 |
|---|---|---|---|---|
| conc-ask-echo-c64-m200 | 592,262 | 1,077,264 | **-45%** | 高并发 ask 包装税最大 |
| conc-ask-echo-c8-m1000 | 546,529 | 598,328 | -9% | 中等并发税降 |
| seq-ask-echo-1k | 113,914 | 100,483 | ~0（噪声） | 样本小，无显著差 |
| tell-echo-100k (send.await) | 171,159 | 183,350 | -7% | 往返语义下税小 |
| tell-echo-100k-dosend（参考） | —（无对等口径） | 3,672,313 | — | 原生 F&F 上限 |
| flood-500k-tell (send.await) | 351,931 | 507,652 | **-31%** | 洪泛往返税明显 |
| flood-500k-dosend（参考） | — | 4,412,810 | — | 原生 F&F 上限 |
| pingpong-rtt-10k | 156,746 | 179,680 | -13% | 两跳 RTT 每跳累积税 |
| self-chain-ask-tell-20k | 163,374 | 193,063 | -15% | 背靠背串行累积税 |
| cpu-serial-200x200k | 3,590 | 4,259 | -16%（含噪声） | CPU 任务被税放大占比小 |
| cpu-parallel-8actors-200k | 47,781 | 49,619 | -4% | 并行摊薄 |
| longrun-2G-iters | 2.380s | 2.228s | -6.8%（疑机器噪声） | 单消息税应≈0 |
| herd-20000-actors（spawn） | 0.071s | 0.047s | **spawn 慢 51%** | uuid+注册表写锁+Box |
| spawn-stop-storm-5k | 25,546 | 27,111 | -6% | 同上但分摊 |
| starve / io-async / mixed-minute / mixed-fifo / chunked / slow-consumer | 持平 | 持平 | ~0 | 业务计算淹没包装开销 |

（完整 22+2 行见 `/tmp/parrot_bench_actix.md` 与 `/tmp/parrot_bench_actix_raw.md`。）

### 11.3 包装税构成定位（代码级）

每消息路径上的 parrot 额外动作（`ActixActorRef::create_envelope` + `ActixActor::handle`）：

1. `uuid::Uuid::new_v4()`（每消息一次，含随机数生成）
2. `MessageEnvelope { id, payload, sender, options, message_type }` 构造 + `ActixMessageWrapper` 包装
3. `Box::new(msg)` 装箱（类型擦除）+ 接收侧 `downcast_ref` 链分发（8+ 分支）
4. 回复路径 `AtomicResponse::new(Box::pin(ready(...).into_actor(self)))` —— ready-future + ActorFuture 包装的双层装箱
5. spawn 路径：`format!("actix://{}/{}", name, uuid)` + 注册表 `RwLock` 写锁

税的形态与场景的关系完全符合预测：**纯消息密度场景（c64 ask 45%、flood 31%、RTT 链 13-15%）显著；计算密度场景（CPU/IO/分钟级混合）税被淹没（≈0）**。

### 11.4 结论与行动建议

1. **parrot 包装确实有损**，且损耗集中在"消息每跳"路径：高并发 ask 最重（-45%），洪泛 tell 次之（-31%），串行 RTT 链 -13~15%，CPU/IO 密集场景无损。
2. 常见热点可优化项（按预期收益排序）：① 去掉每消息 `Uuid::new_v4()`；② 同步路径返回 `MessageResult` 直通而非 `AtomicResponse` 装箱 ready-future；③ `message_type` 硬编码 `"unknown"` 的字符串每消息传递。**⚠ 本节初版曾预估"落地后税可收窄到 <15%"——该预估经 11.5 影响评估后判定过于乐观，实施前必须先 profile 归因，见 11.5。**
3. 该基准目录沉淀为常驻资产：`bench/actix-bench/` 与 `bench/akka-bench/` 平级收纳于 `bench/`，任何包装层改动后可一键复测（`cargo run --release`）量化回归。搬迁后已实测复跑验证（actix-raw c64 ask 1.13M/s、akka flood 1.94M/s，与搬迁前一致）。

### 11.5 查漏补缺：两条优化建议的负面影响评估（对 11.4 的修正）

用户追问"这些改动有什么负面影响？"后逐条核实代码事实，结论：**两项优化都不应在 profile 归因之前动手**。

#### 11.5.1 优化①（去掉每消息 Uuid::new_v4()）的负面影响

| 负面影响 | 严重度 | 事实依据 |
|---|---|---|
| 唯一性语义弱化 | 中 | v4 UUID 是 122-bit 随机，天然跨进程/跨重启/跨系统唯一；原子自增是 64-bit 顺序值，两进程各从 0 计数必然碰撞。`MessageEnvelope` 是 `parrot-api` 公共结构体且 `id` 为公开字段——未来做分布式追踪/消息 journal/跨节点去重时顺序 ID 埋雷 |
| 可预测性 | 低~中 | 顺序 ID 可猜测；若外部将消息 id 用作幂等 token 或防重放凭据，属安全降级 |
| 公共 API 破坏 | 高（若改类型） | `id: uuid::Uuid`（16 字节）→ `u64` 是 breaking change 波及全部下游；若保类型改惰性生成，读侧需处理分支且高并发读反而引入竞争 |
| 测试资产破坏 | 低 | `parrot-api/tests/message_test.rs:238` 断言 `id != Uuid::nil()`，nil 占位方案会挂 |
| **收益可能远低于直觉** | — | **关键**：`Uuid::new_v4()` 用线程本地 PRNG（getrandom 仅首次初始化，此后纯内存 xorshift），不是系统调用；高争用下换全局 `AtomicU64::fetch_add` 同样有 cache-line 弹跳。-45% 税中 uuid 占比**未经 profile 确认**，不能拍脑袋归因 |

**保守替代方案**：保持 `Uuid` 类型不变，若 profile 证显著才改 **UUIDv7 风格**（时间戳高位 + 线程本地计数器低位：无全局原子、保跨系统唯一、类型兼容、测试不破坏）。

#### 11.5.2 优化②（同步路径直通 MessageResult）的负面影响——四重障碍

**障碍一：关联类型不允许"一个 impl 两种 Result"。** `Handler<M>::Result` 每个 `(Actor, Message)` 组合只能有一个值。同步路径要 `MessageResult`（零装箱立即返回），异步路径必须 `AtomicResponse`（等 future）——二者无法共存于同一个 `Handler<ActixMessageWrapper>` impl。出路只有：拆两个 wrapper 类型（但发送侧 `Addr<ActixActor<A>>` 类型上无从选择）、或 `ActixActor<A, Mode>` 泛型化（spawn API/注册表/所有 Addr 签名全链路 breaking change）。

**障碍二：`use_async_handler()` 是运行时开关。** 签名 `&self` 语义上允许 actor 随内部状态逐消息切换路径；改编译期决定是行为变更，依赖动态切换（如降级模式）的 actor 被静默破坏。

**障碍三：不能退化为 `ResponseActFuture`——这不是随意选择。** actix 默认 handler 返回后立即继续拉 mailbox；只有 `AtomicResponse`（内部 `ctx.wait` 门闩）在 async future 完成前阻塞后续消息，这是 **per-actor 串行语义**的承重墙（`parrot/src/actix/actor.rs` 注释明确记载）。换 `ResponseActFuture` 后 async handler 飞行中下一条消息插入执行，B4 Saga 守恒、B7 审计序、C2 FIFO 等一切依赖 per-actor 串行的测试都会挂。

**障碍四：unsafe 边界需全部重审。** `EngineContextHandle::from_raw` 与 `AsyncDispatchFuture` 的 lifetime transmute 两处 unsafe 的 SAFETY 论证均锚定当前分发结构；拆路径 = 重新论证 soundness（此为近期刚修复的高危区域）。

**收益上限的冷静估计**：`ready().into_actor()` + `AtomicResponse` ≈ 2 次堆分配 + 一次 no-op poll，相对 ask 全链路（oneshot + channel + arbiter 唤醒 + `BoxedMessage` 装箱 + downcast 链 + envelope）**可能只占税的 20~30%**。即使完美落地，c64 ask 的 -45% 或许仅收窄到 -30%；剩余是"引擎无关抽象"（`BoxedMessage` 统一分发）的结构性代价，消除它须改公共 API。

#### 11.5.3 修正后的行动顺序

1. **先 profile 归因**（Instruments time-profile 跑 `bench/actix-bench` 的 c64 ask），把 -45% 拆解到 uuid / envelope 构造 / Box+downcast / AtomicResponse / oneshot 各占比；
2. 优化①仅在 profile 证显著（>10%）后按 UUIDv7 保守版实施；
3. 优化②降级为"有条件可行"：须 profile 证装箱占比 >15% 才值得 breaking change，实施走 `ActixActor<A, Mode>` 泛型化 + spawn API 显式声明 + `use_async_handler` 保留一个版本周期做 deprecation；
4. 任何落地以全量回归兜底：474 测试 + B 系列守恒断言（能抓串行性破坏）+ `bench/actix-bench` 复测量化。

**一句话结论：优化①是"低风险但可能低收益"，优化②是"真实收益但架构代价高且踩在 unsafe 边界上"——两者都不该在 profile 归因之前动手。**

---

## 第十二轮（2026-10-04）：消息维度综合矩阵 —— 轨道 × 大小 × 分配路径 × 通信模式

> 触发诉求：把此前散落各轮的"动态轨 vs 静态轨"、"消息大小与分配次数（1 次分配 vs 2 次分配）"及各种组合场景系统化为一**张矩阵**，用同一份代码、同机连续运行一次跑全。
> 资产：`parrot/tests/bench_msg_matrix.rs`（6 组 A–F 系列，`#[ignore]` 日常跳过；运行：`cargo test -p parrot --release --test bench_msg_matrix -- --ignored --nocapture --test-threads=1`，全套实测 **2.0s**）。
> 环境与口径：同机 M5 Pro（15 核），thread 引擎默认共享池；动态轨 actor 为 `DynEcho`（downcast 链回显），静态轨为单协议 `EchoS/M/L` 与混合协议 `EchoMixed{PingS, PingL}`（derive 枚举信封）。大消息用定长数组（`[u8;64]`/`[u8;1024]`/`Box<[u8;65536]>`）避免 Vec 双分配噪声。

### 12.1 矩阵设计

| 维度 | 取值 |
|---|---|
| 轨道 | 动态轨（`BoxedMessage` 擦除 + downcast 链）/ 静态轨（M4 typed 枚举信封，零 Any） |
| 消息大小 | u64 inline（≤16B SSO，asker 侧 1 分配）/ u64 boxed（2 分配）/ 64B / 1KB / 64KB |
| 通信模式 | 串行 ask（N=10k）/ c8 并发 ask（80k）/ tell 纯投递（100k）/ 大小混合流（95% 小 + 5% 64KB） |
| 组合 | 混合协议枚举槽位膨胀（小消息付大槽代价）/ 动态轨大小混合洪泛 |

### 12.2 全量数据（release，单次连续运行，6 组全过）

**A/B/C 系列：串行阶梯（ask N=10k，tell N=100k）**

| 行 | 场景 | 吞吐 | 相对基线 | 分配画像 |
|---|---|---|---|---|
| A1 | 动态轨 ask inline u64 | 82,716/s | 1.00x | asker 1 分配（oneshot）；消费侧还原仍需 Box |
| A2 | 动态轨 ask boxed u64 | 87,672/s | 1.06x | asker 2 分配（payload Box + oneshot） |
| A3 | 动态轨 ask 64B | 86,089/s | 1.04x | 同 A2，payload 64B |
| A4 | 动态轨 ask 1KB | 70,469/s | 0.85x | 同 A2，payload 1KB（sender 侧 clone 1KB 开始可测） |
| A5 | 动态轨 ask 64KB | 72,488/s | 0.88x | 同 A2，payload 64KB（Box 指针传递，大小不敏感） |
| B1 | 动态轨 tell u64 | 669,101/s | 8.1x（vs A1） | 1 分配（payload Box） |
| B2 | 动态轨 tell 64B | 784,865/s | 9.5x | 1 分配 |
| B3 | 动态轨 tell 1KB | 783,900/s | 9.5x | 1 分配——**tell 对 payload 大小完全不敏感**（Box 指针入队） |
| C1 | 静态轨 ask u64 | **115,659/s** | 1.40x（vs A1） | 0 分配，枚举槽 ~24B |
| C2 | 静态轨 ask 64B | **114,723/s** | 1.38x | 0 分配，槽 ~72B（按值 copy） |
| C3 | 静态轨 ask 1KB | **110,675/s** | 1.34x | 0 分配，槽 ~1KB（每条 copy 1KB 进 flume 槽） |

**D 系列：c8 并发 ask（80k 消息）**

| 行 | 场景 | 吞吐 | 结论 |
|---|---|---|---|
| D1 | 动态轨 inline c8 | 543,993/s | 并发下 inline 档最快 |
| D2 | 动态轨 boxed 1KB c8 | 492,183/s | 1KB clone 进入驱动侧成本 |
| D3 | 静态轨 1KB c8 | **512,001/s** | 槽 copy 与 Box 分配在并发下互相抵消 |

**E 系列：计数分配器全链路差分（N=2k，含消费侧与回复侧）**

| 行 | 场景 | allocs/msg（全链路） | 理论画像 |
|---|---|---|---|
| E1 | 动态轨 ask inline | **16.05** | oneshot 1 + 消费侧还原 Box 1 + 回复 Box 1 + 调度/唤醒路径 ~13 |
| E2 | 动态轨 ask boxed | **16.05** | payload Box 1 + oneshot 1 + 回复 1 + 调度路径 ~13 |
| E3 | 静态轨 ask | **8.00** | reply channel + 摊销 ≈ 动态轨的一半 |
| E4 | 动态轨 tell | **6.54** | payload Box 1 + 入队路径 ~5.5 |
| E5 | 静态轨 tell | **1.77** | flume 槽预分配按值入队，接近零分配 |

**F 系列：组合场景**

| 行 | 场景 | 吞吐 | 对照 |
|---|---|---|---|
| F1 | 静态轨小消息经混合协议 {S,L} 枚举 | 105,712/s | vs C1 单协议 115,659/s：**槽位膨胀税 -8.6%** |
| F1b | 静态轨大消息经混合协议 | 112,542/s | 与 C3 持平（本来就付 1KB 槽） |
| F2 | 动态轨 95% u64 + 5% 64KB 混洪泛 | 843,803/s | 与 B 系列纯 tell 持平——异构大小无退化 |

### 12.3 关键发现

1. **E1 = E2（16.05）：inline SSO 在动态轨"全链路不省分配"**。`AskEnvelope::new_inline` 省掉了 asker 侧 payload Box，但消费侧 `AskPayload::into_boxed` 在 `receive_message` 边界**还原为 BoxedMessage**（既定设计，见 `envelope.rs` 注释）——分配只是从发送侧搬到消费侧。吞吐上 A1（82.7k）与 A2（87.7k）无显著差（单次 malloc ~25ns 占 p50 ~12µs 的 0.2%）也印证。**inline 档的真实价值在"发送侧最小化"（嵌入式/实时发送线程）而非全链路总量**。
2. **静态轨优势量化：串行 ask +34~40%、全链路分配减半（16→8）**。C1–C3 对 A1–A4 的稳定 1.34–1.40x 来自零 Any 装箱 + 零 downcast 链 + 零 payload 分配三重消除。与 PERF_BASELINE 的 M4 数据（tell 10.2x、ask 持平）合并解读：**串行 ask 在本轮更快是因为本轮动态轨对照走 `ask`/`ask_inline` 完整路径而非基线的 BoxedMessage 直接 ask**。
3. **动态轨 tell 对 payload 大小完全不敏感（B1/B2/B3 = 669k/785k/785k）**：Box 指针入队，64B→1KB 零退化。静态轨 ask 同样不敏感（C2/C3 差 3.5%）：1KB 槽 copy（~30ns）被调度开销淹没。**"消息大小"在双轨下都不是吞吐杠杆；"分配次数"才是**（E 系列一半差距）。
4. **混合协议枚举槽位膨胀税 -8.6%（F1 vs C1）**：协议集 {PingS(24B), PingL(1KB+)} 的枚举槽 = 最大变体，小消息也付 1KB copy。**选型指导：高价值小消息协议应独立成单协议 actor（或把大消息变体拆到子 actor），避免槽位被最大变体拖累**。F1b 反向印证：大消息在混合枚举中零额外代价。
5. **并发（c8）下三档趋同（492k~544k）**：D 系列中 inline/boxed/静态轨差距收窄到 ±5%——并发瓶颈转移到 actor 单点串行处理与唤醒路径，发送侧分配差异被摊薄。**发送侧优化（inline/静态轨）的价值集中在串行低并发场景**。
6. **异构大小混合流无退化（F2 = 843k/s ≈ 纯 tell 档）**：95%+5% 混合与纯小消息洪泛持平，邮箱与调度器对异构消息无亲和性损失。

### 12.4 与既有轮次的口径衔接

| 本轮行 | 对应既有数据 | 关系 |
|---|---|---|
| A2（dyn ask 87.7k/s） | 第九轮 seq-ask-echo-1k thread 50k/s | 本轮 N=10k 更长 + 空 handler（无 burn）+ 新系统实例，量级一致 |
| B1（dyn tell 669k/s） | M4 基线动态轨 tell 837k/s | 同量级（本轮走 `deliver` 纯投递路径） |
| C1（static ask 115.7k/s） | M4 基线静态轨 ask 持平动态轨 | 本轮对照更严：动态轨走完整 `ask` 路径后静态轨领先 40% |
| E5（static tell 1.77 allocs） | M5 计数分配器（inline vs boxed 差分） | 本轮补上"全链路"口径（含消费/回复/调度路径） |

### 12.5 行动建议

1. **高价值小消息协议**（行情 tick、传感器、信号）：首选静态轨单协议 actor（C1 115k/s + 8 allocs 全链路）；避免与 >256B 变体混入同一枚举（F1 税）。
2. **动态轨大消息**：直接 Box 传递（A4/A5 0.85–0.88x 轻微税来自 sender 侧 clone，业务侧可 `std::mem::take` 消除）；无需池化（tell 档 785k/s 已无分配瓶颈征兆）。
3. **inline 档定位修正**：文档与 API 注释应明确 `ask_inline` 的收益边界是"asker 侧零 payload 分配"，全链路分配不变（E1=E2 实证）；对消费侧分配敏感的场景应改用静态轨而非 inline。
4. **并发 >8 的发送侧**：无需在 inline/boxed/静态轨之间做性能选型（±5%），按类型安全与代码人体工学选静态轨即可。

---

## 第十三轮（2026-10-04）：六方终局横评 —— thread / actix / 纯 actix / Akka / Erlang / Ray

> 触发诉求：以**各方最优配置**做一次全面对等压测，输出能力等价性矩阵 + 性能画像 + 场景推荐（对齐第四轮报告的呈现格式）。
> 六方资产：thread=`engine_stress_thread.rs`（弹性 burst + 静态轨消息）· actix=`engine_stress_actix.rs`（Arbiter 池 15 线程）· 纯 actix=`bench/actix-bench`（零 parrot 包装）· akka=`bench/akka-bench`（JVM 21 ZGC, fork-join=15, JIT 预热 3k）· erlang=`bench/erlang-bench`（OTP 29, 15 schedulers, **SCALE 重标定 1/24**——旧值 2200 使 CPU 场景过度缩放，本轮修正）· ray=`bench/ray-bench`（降级版：15 核/10GB/5min 预算，actor=OS 进程）。
> 同机连续运行（2026-10-04，M5 Pro 15 核，release/JIT 后）。全部 20 场景对等：同 burn_cpu LCG 内核、同分位数算法、同并发度；跨语言按"计算时长对等"缩放迭代数（Erlang 1/24、Ray 1/86，note 标注）。

### 13.1 全量数据（20 场景 × 6 方，吞吐 /s；语义场景为 ✓）

| 场景 | thread | actix | 纯actix | akka | erlang | ray | 最优 |
|---|---|---|---|---|---|---|---|
| seq-ask-echo-1k | 81,125 | 128,272 | **165,396** | 97,473 | 1,610,306* | 273 | erlang*（见注） |
| conc-ask-c8-m1000 | 505,849 | 551,512 | **1,122,131** | 223,579 | 890,670 | 301 | 纯actix |
| conc-ask-c64-m200 | 569,151 | 600,098 | **1,734,085** | 333,160 | 1,001,330 | 283 | 纯actix |
| tell-echo-100k | 82,444 | 162,336 | 190,501 | 1,378,655 | **9,000,900** | 287 | erlang |
| tell-100k-dosend（F&F 参考口径） | — | — | 3,121,265 | ≈tell | ≈tell | — | erlang（9M 含投递） |
| cpu-serial-200x200k | 3,929 | 4,264 | **4,344** | 4,077 | 4,690 | 266 | 持平（计算主导） |
| cpu-parallel-8actors | 49,552 | 49,807 | **48,601**† | 41,018 | 47,231 | 776 | rust 三方（持平） |
| flood-500k-tell | 330,514 | 369,000 | 510,821 | 1,913,673 | **4,509,542** | 267 | erlang |
| ask-timeout-short | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | 六方语义一致 |
| send-after-stop | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | 六方语义一致 |
| herd-20000-actors | **518,131/s** | 305,323 | 453,490 | 148,686 | 1,339,226‡ | 10,384（150 上限） | 见注‡ |
| longrun-2G-iters | 2.30s | 2.26s | 2.25s | 2.24s | **2.09s** | 1.92s（缩放后） | 持平（CPU 主导） |
| starve-echo（重载期探针 p99/max） | 0.1/0.1ms | 0.1/0.1ms | 0.1/0.3ms | 0.1/0.2ms | 0.01/0.5ms | 3.5/3.5ms | 六方亚毫秒 |
| io-async-64actors-10ms | 0.87s（1.5×） | 0.24s（53×） | 0.25s（51×） | 0.25s（52×） | **0.22s（58×）** | 0.59s | erlang/akka/actix 并列 |
| mixed-minute（wall / probe max） | 71.6s / 7.8ms | 70.7s / 4.8ms | 70.5s / 9.2ms | **68.4s** / 0.3ms | 86.4s / 9.6ms | 4.3s（缩放） | akka 时长 / akka 尾延迟 |
| mixed-same-actor-fifo | 51.7s | 50.9s | 50.0s | **49.9s** | 77.2s | 4.6s（缩放） | 六方 FIFO 语义强制 |
| chunked-vs-solid | 69.7s | 68.4s | 68.3s | 67.4s | 64.4s | 11.8s（缩放） | 持平（让出开销 <2%） |
| pingpong-rtt-10k | 74,348 | 146,340 | **183,851** | 129,474 | 2,063,558* | 242 | erlang*（见注） |
| self-chain-ask-tell-20k | 86,539 | 138,396 | 195,469 | 232,725 | **3,833,253*** | 305 | erlang*（见注） |
| slow-consumer-8prod-40k | 4,875/s（8.2s） | 4,999/s（8.0s） | **5,063/s**（7.9s） | 5,222/s（7.7s） | 4,369/s（9.2s） | 55/s（**超时**） | 单 actor 串行上限 |
| spawn-stop-storm-5k | **26,865** | 25,574 | 26,662 | 27,808 | 31,353 | 150/s（300 规模） | erlang / 持平 |

**口径注释**（诚实呈现，防误读）：
- **\* Erlang ask 系数字地测量**：BEAM 同进程消息是纯内存 copy（无序列化、无 future、无 waker），同 VM 内 `Pid ! Msg` + selective receive 是六方最短路径。Erlang 的 1.6M~9M/s **真实但不可直接平移**——它不含分布式/跨节点成本，且数字依赖本机 scheduler 亲和高吞吐探针（starve 场景 120 万探针即此现象）。跨节点或 gen_server（带 monitor/timeout 层）时衰减一个数量级。
- **† 纯 actix 的 cpu-parallel 用 64 actor 口径**（与 rust 8 actor×25 场景名对齐的历史口径），数字与 parrot 双引擎同档。
- **‡ herd 对比须看语义**：erlang/akka 的"spawn"是用户态进程/对象（1.3M/s vs 149k/s），ray 是 OS 进程（150 个上限即内存预算耗尽），parrot thread 是数据结构+注册（518k/s，rust 语义下最优）。
- **ray 全列**为降级口径（actor 数/迭代数缩减，note 标注），仅作量级参照；`slow-consumer` 超时是 Ray 客户端大量 pending task 的固有退化。

### 13.2 能力等价性矩阵（六方）

| 能力 | thread | actix | 纯actix | akka | erlang | ray | 结论 |
|---|---|---|---|---|---|---|---|
| ask（请求-响应） | ✅ oneshot+timeout | ✅ actix timeout | ✅ | ✅ CompletionStage | ✅ receive after | ✅ ray.get | 六方等价 |
| tell（fire-and-forget） | ✅ deliver | ✅ do_send | ✅ | ✅ tell | ✅ `!` | ✅ remote() 不 get | 六方等价 |
| 超时控制 | ✅ | ✅ | ✅ | ✅ | ✅ after | ✅ | 六方等价 |
| stop 语义 | ✅ | ✅ | ✅ | ✅ | ✅（退出后消息死信） | ✅ RayActorError | 六方等价 |
| async handler（IO 型） | ✅ 原生 async fn | ✅ ctx.wait 异步路径 | ✅ | ✅ CompletionStage | ✅（进程天然挂起） | ✅（进程内串行） | 六方等价 |
| 多 actor 真并行 | ✅ 池化 worker | ✅ Arbiter 池 | ✅ | ✅ fork-join 15 | ✅ 15 scheduler 抢占 | ⚠️ 1 CPU/actor 预留 | rust 三方+erlang 优 |
| 海量 actor（20000） | ✅ **518k/s** | ✅ 305k/s | ✅ 453k/s | ✅ 149k/s | ✅ 1.34M/s | ❌ 150 上限（OS 进程） | erlang 语义最优 / thread 在"spawn 即数据结构"语义下最优 |
| 消息洪泛韧性（500k） | ✅ 331k/s | ✅ 369k/s | ✅ 511k/s | ✅ 1.9M/s | ✅ **4.5M/s** | ⚠️ 4k 即积压 | erlang 结构性最优 |
| 有界邮箱/背压策略 | ✅ **四策略 per-actor** | ❌ 无界 | ❌ 无界 | ❌ 无界（需 Externalizer） | ✅（进程邮箱可配） | ✅（队列对象） | **thread 独有细粒度矩阵** |
| 独占线程隔离 | ✅ DedicatedThread | ⚠️ 手工 arbiter | ⚠️ 同左 | ⚠️ PinnedDispatcher 可配 | ⚠️ scheduler 绑定 | ❌ | **thread 结构性独有** |
| 亲和性分片调度 | ✅ Sharded（1.76×） | ❌ | ❌ | ⚠️ 手工 router | ⚠️ +S 绑核粗粒度 | ❌ | **thread 独有** |
| 分布式/跨节点 | ❌（M6 规划中） | ❌ | ❌ | ✅ **原生** | ✅ **原生** | ✅ **原生** | akka/erlang/ray 结构性优势 |
| 热更新/容错树 | ❌ | ❌ | ❌ | ✅ supervision | ✅ **OTP supervisor** | ✅ | erlang 最深 / parrot M3 有 supervisor |
| 宿主嵌入零侵入 | ✅ **shared() 寄生宿主** | ⚠️ 需 System | ⚠️ 同左 | ⚠️ 需 ActorSystem | ⚠️ 需 node | ⚠️ 需 ray.init | **thread 独有** |
| 类型化消息轨道 | ✅ **静态轨零装箱** | ✅ 静态轨 | ✅ 泛型 Handler | ✅ Typed API | ⚠️ 动态类型 | ⚠️ 动态类型 | parrot 双轨+rust 生态最完整 |

### 13.3 性能画像（六方）

**thread 引擎（自研，池化 worker + 唤醒钩子调度 + 三调度模式）**
- 架构特征：中央 SchedulingQueue + N worker 抢占邮箱批次；ScheduleState 单 actor 互斥；弹性 burst worker + spawn_blocking 阻塞隔离；Sharded 亲和分片（S1 1.76×）；DedicatedThread 独占线程。
- 强项：海量 actor 创建（herd 518k/s，rust 语义最优）；中高并发 ask（c8 506k/s）；长任务治理三层机制化（M1 probe max 7.8ms 修复后）；有界邮箱四背压策略（六方唯一细粒度矩阵）。
- 税：包装+动态轨装箱使 ask 系列低于纯 actix（81k vs 165k 串行）——静态轨可收回大部分（C1 116k/s）。

**actix 引擎（适配层 + Arbiter 池 15 线程）**
- 强项：串行 ask 128k/s（parrot 系最快）；IO 密集 53× 收敛；混载尾延迟最稳（M1 probe max 4.8ms）。
- 包装税：vs 纯 actix 高并发 ask -65%（c64 600k vs 1.73M）——第十一轮已归因（uuid/envelope/装箱链）。

**纯 actix（零包装参照系）**
- 结构性上限：c64 ask **1.73M/s**、c8 1.12M/s、pingpong 184k/s——rust 单机 actor 通信的天花板参照。
- 弱项：无界邮箱无背压；海量 actor 453k/s 低于 thread（spawn 走 arbiter 分配）。

**Akka Typed（JVM 21 ZGC，fork-join 15）**
- 强项：混载 wall 最短（68.4s）且 probe max 仅 0.3ms（六方最优尾延迟）；纯投递 1.9M/s；生态成熟（分布式/持久化/cluster sharding 原生）。
- 弱项：herd 149k/s（六方垫底，guardian→cell→mailbox 链路）；GC 抖动在极端负载尾部显现（历史观测 7.3ms）。

**Erlang/OTP 29（BEAM，15 scheduler）**
- 强项：**消息传递结构性地表**——同 VM `!` 为纯 copy 无装箱：tell 9M/s、flood 4.5M/s、herd 1.34M/s；抢占式调度使任何负载下探针亚毫秒；2000 万级进程容量；OTP 容错/热更新不可替代。
- 弱项：纯算力慢（BEAM 36M iters/s vs rust 880M，1/24 缩放才时间对等；longrun 缩放后仍 2.09s——JIT 后逼近）；M1 混载 wall 86.4s（+22%，调度公平换吞吐）；类型动态、单机计算密集非主场。

**Ray（Python，分布式 task 框架）**
- 定位：六方中唯一**分布式优先**——actor/task 跨节点透明、自动重试、placement group。单机性能是代价：单客户端任务速率 ~330/s 上限（gRPC+GCS 中转），消息路径差距 2-3 个数量级；actor=OS 进程（25MB/个）使海量实体不可行；slow-consumer 5k 即超时。
- 结论：粗粒度任务编排（分钟级 task、进程隔离、ML pipeline）专属，与 actor 引擎不可互换。

### 13.4 最终推荐矩阵

| 场景 | 推荐 | 依据 |
|---|---|---|
| 串行 ask 延迟敏感（parrot 内） | **actix 引擎** | 128k/s，probe 稳 |
| 串行 ask（允许零包装） | **纯 actix** | 165k/s |
| 同 VM 极致消息吞吐（tell/flood） | **Erlang**（不可平移，见注*） | 4.5M~9M/s 结构性 |
| 高并发 ask 争用（>32 并发） | **纯 actix / thread 静态轨** | 1.73M / 569k（动态轨） |
| CPU 密集并行 | **rust 三方任选** | 48~50k/s 持平；计算主导 |
| CPU 密集 + 混合负载 + 尾延迟 | **Akka** | wall 68.4s + probe max 0.3ms |
| 长任务治理（响应敏感、可控隔离） | **thread 引擎** | DedicatedThread+弹性 burst+阻塞池三层机制 |
| IO 密集高并发 | **actix/akka/erlang 并列** | 51~58× 收敛持平 |
| 海量轻量 actor（单机） | **thread（rust 语义）/ erlang（进程语义）** | 518k/s / 1.34M/s |
| 海量 actor + 背压防 OOM | **thread 独有** | 有界邮箱四策略，A3 实证 200k 拒收 0.03s |
| 亲和性分片（会话粘滞/分区 topic） | **thread 独有** | Sharded 1.76× + 邻域隔离 48µs |
| 宿主 runtime 嵌入（库形态） | **thread 独有** | shared() 零侵入 |
| 分布式集群/远程 actor | **akka / erlang / ray**（parrot M6 规划中） | 原生网络层 |
| 容错树/热更新/电信级 | **erlang** | OTP 不可替代 |
| ML/数据管道粗粒度 task | **ray** | 分布式任务图原生 |
| 类型安全消息（零装箱+零 downcast） | **parrot 静态轨**（双引擎可用） | C1 116k/s + 8 allocs 全链路（第十二轮） |

**总体结论**：
1. **六方没有一个全能冠军**——rust 三方（thread/actix/纯actix）统治单机高并发消息与 CPU 密集；Erlang 统治消息传递结构与容错；Akka 在混载尾延迟与生态完整度上占优；Ray 是分布式 task 专精。选型按场景查上表。
2. **parrot 双引擎组合**在 rust 语义内覆盖了纯 actix 的多数优势域（高并发 ask 走 actix 引擎、背压/隔离/海量走 thread 引擎），并在**有界邮箱背压、DedicatedThread 隔离、Sharded 亲和、宿主零侵入嵌入、静态类型轨**五个维度结构性领先全部五方——这五项正是 akka/erlang/ray 架构上不具备（或需重量级手工配置）的能力。
3. **量级参照锚点**（同机对等口径）：单机消息路径 天花板 = Erlang 同 VM `!`（4.5M~9M/s）＞ 纯 actix do_send（3.1M/s）＞ akka tell（1.9M/s）＞ rust ask 系（80k~1.7M/s 按包装/并发分层）＞ Ray（330/s）。**ask 语义成本 everywhere**：任何带回复等待的路径都比纯投递慢一个数量级——这是语义代价而非调度缺陷，六方一致。

### 13.5 本轮基准资产与方法学沉淀

- 六方基准全部收敛到 `bench/` 目录：`actix-bench/`（纯参照）、`akka-bench/`、`erlang-bench/`、`ray-bench/`（降级版），`./run.sh` 一键复现。
- **Erlang SCALE 重标定**（2200→24）：旧标定使 CPU 场景过度缩放（计算被消息开销淹没、超时门控不确定）。修正后时间对等成立（longrun 2.09s ≈ rust 2.3s 档）。**教训：跨语言速率标定必须随运行时版本重测**（OTP JIT 使 bignum 快 ~90×）。
- Ray 降级版（15 核/10GB/5min）方法：内存预算倒推 actor 数（25MB/进程）、每场景 kill 防 CPU 配额泄漏死锁、全部 ray.get 带超时。
- 压测口径统一原则：同 LCG burn 内核、同分位数算法、同并发度、"计算时长对等"跨语言缩放并在 note 标注——六方数据可直接横比。
