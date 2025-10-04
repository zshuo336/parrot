# Parrot 性能基线（M0 建档）

> 数据源：`parrot/tests/stress_baseline.rs`（release 模式，`cargo test --release -p parrot --test stress_baseline -- --nocapture`）
> 机器：Apple M5 Pro（15 逻辑核，P 核 5）/ 2026-10-03
> 用途：M2/M4/M5 改造的防倒退对照。门禁断言在测试内（宽松 10 倍下限）；本表数字为精确参考值。

| 指标 | M0 基线 | M2 后 | 变化 |
|------|------|------|------|
| ask 往返 p50 | 24 µs | 24 µs | 持平 ✓ |
| ask 往返 p99 | 63 µs | 63 µs | 持平 ✓ |
| ask 吞吐 | ≈41,667 ask/s | ≈41,667 ask/s | 持平 ✓ |
| tell 入队吞吐 | 366,551 msg/s | 376,978 msg/s | +2.8% ✓ |

（ask 数字为 10,000 次串行 ask 的 BoxedMessage 擦除路径统计；tell 为 100k `send_msg` + ask 屏障 drain。）

**M2 落地说明**：双车道 `SchedulingQueue`（High 严格优先）、`MpscMailbox` 邮箱内 High 车道（pop 先排空）、worker re-queue/wake-hook 按邮箱内容分车道、reduction 预算（多 actor 竞争时让出并重置，单 actor 无自让churn）、死亡通知 High+Block 不可丢。公平性验证：慢 actor（2ms/条 × 500 积压）下快 actor ask p99 < 500ms（`test_m2_scheduling.rs`）。

## 预存环境性问题记录（M0 分诊）

- `test_thread_advantages.rs` a1/a5：阈值按 release 标定（burn_cpu 0.89G/s release vs 0.26G/s debug），debug 下必然超时/不达标。已标注 `#[ignore = "release-only thresholds"]`，CI 以 `--release --include-ignored` 跑全套。**非引擎 bug**（release 下 5/5 绿）。
- 全 workspace 测试基线：debug 模式除上述 2 个 ignore 外全绿（parrot 28 文件 + parrot-api/derive-tests 13 文件）。

## 后续里程碑对照计划

| 里程碑 | 预期影响 |
|--------|---------|
| M2 调度（双车道+预算） | ✅ 已完成：ask/tell 持平或微升；公平性/插队指标新增（见 test_m2_scheduling） |
| M4 静态轨 | ✅ 已完成，见下节 |
| M5 单块信封 | 动态轨 ask 每消息分配 2→1 次，p50 预期小幅改善；tell 吞吐不倒退 |

## M5 单块信封实测（2026-10-03）

数据源：`parrot/tests/stress_baseline.rs`（release，同机）+ `test_m5_single_alloc.rs`。

| 指标 | M0 基线 | M2 后 | M5 后 | 变化 |
|------|------|------|------|------|
| ask 往返 p50 | 24 µs | 24 µs | 24 µs | 持平 ✓ |
| ask 往返 p99 | 63 µs | 63 µs | 61 µs | -3% ✓ |
| tell 入队吞吐 | 366,551 | 376,978 | 369,691 | 持平 ✓ |
| ask 信封分配（≤16B） | 3 | — | **1**（inline oneshot） | -67% |
| ask 信封分配（>16B） | 3 | — | **2**（payload Box + oneshot） | -33% |

**落地说明**：`AskEnvelope` 分层载荷（`InlinePayload` SSO ≤16B / `BoxedMessage` >16B）+
信封按值入队（`Mailbox::push_ask`，flume 环形区按值存储，零信封装箱）+
`ask_inline` SSO 入口（asker 侧 1 分配：oneshot cell）。Phase B
`SingleAllocEnvelope`（unsafe 单块：header + payload + inline oneshot
一次分配）已移植 POC 并 miri 6/6 绿，作为 >16B 消息的下一步演进路径
（当前 >16B 走 safe 的 2 分配档）。`parrot_envelope_legacy` feature
逃生阀保留（首个稳定版移除）。

**门禁**：miri 全绿（`scripts/miri.sh`，macOS kqueue 限制下聚焦
single_alloc 模块）+ 计数分配器差分测试常驻（
`test_m5_single_alloc.rs::m5_allocation_counter_inline_vs_boxed`）。

## M4 静态类型轨实测（2026-10-03）

数据源：`parrot/tests/test_m4_typed.rs::m4_static_vs_dynamic_throughput`（release，同机）。

| 指标 | 静态轨 | 动态轨 | 比值 |
|------|--------|--------|------|
| 串行 ask 往返（N=5k） | 12.2 µs/次 | 11.8 µs/次 | 0.97x（持平） |
| tell 入队吞吐（N=50k） | **8,538,555 msg/s** | 837,360 msg/s | **10.2x** |

**解读**：
- **串行 ask 持平**是符合预期的结果：串行模式下每条消息的唤醒/调度成本
  （waker、task poll、channel 唤醒）占主导，一次 `Box` 装箱（~20ns）被
  摊薄到不可见。静态轨 ask 的价值在**零类型擦除的类型安全**与**tell 高
  吞吐**，而非串行往返延迟。
- **tell 10.2x** 是分配路径差异的直接体现：动态轨每条消息
  `Box::new(1u64)`（堆分配 + drop），静态轨枚举信封直接进 flume（无
  独立堆分配）。此场景（高频同类型小消息 fire-and-forget）正是静态轨
  的设计目标场景。
- M5（单块信封）将进一步把动态轨 ask 的 2 次分配降为 1 次，缩小但
  不消除与静态轨的差距。

## Ray 对等基线（降级版，2026-10-04）

数据源：`bench/ray-bench/ray_bench.py`（Ray 2.51.2，同机 Apple M5 Pro）。
运行约束：**15 核 / 10GB 内存预算 / 全套 ≤5 分钟**（实测 181.7s，20/20
场景完成，`slow-consumer` 1 项 drain 超时）。Python burn 速率 ~10.2M
iters/s（约为 Rust 的 1/86），CPU 场景迭代数按 86× 缩减保持标称计算
时长对等。Ray actor=OS 进程（~25MB/个）：herd 降至 150 个、spawn-storm
降至 3×100（内存预算内），两处降载在 note 中标注。

**关键数字（vs 双引擎，2026-10-04 空闲机器复测）**：

| 指标 | thread 引擎 | actix 引擎 | Ray（降级版） | Ray 差距 |
|------|------------|-----------|--------------|---------|
| 串行 ask 吞吐 | 81,125/s | 128,272/s | 273/s | **297x / 470x** |
| 串行 ask p50 | ~24µs | ~8µs | 3.6ms | 150x / 450x |
| 并发 ask（8 路） | 505,849/s | 551,512/s | 301/s | **1680x / 1833x** |
| tell 排空（100k） | 82,444/s | 162,336/s | 287/s（5k） | **287x / 565x** |
| CPU 并行（8 actor） | 49,552/s | 49,807/s | 776/s（4 actor） | ~64x |
| 洪泛 tell（500k） | 330,514/s | 369,000/s | 267/s（4k 已积压） | **1238x / 1382x** |
| pingpong RTT | 74,348/s | 146,340/s | 242/s | 307x / 605x |
| actor 创建 | 518,131/s | 305,323/s | 10,384/s（150 个上限） | ~50x（内存受限） |
| 2G 迭代长任务 | ~2.3s | ~2.3s | 1.92s | 持平（纯计算） |
| 饥饿隔离（长任务期间 echo p99） | 0.1ms | 0.1ms | 3.5ms | 35x |
| slow-consumer 40k/5k | 8.2s 排空 | 8.0s 排空 | **90s 未排空 5k** | 固有限制 |

**解读**：
- **消息路径差距 2-3 个数量级**是架构性的：Ray 每消息走 gRPC 序列化 +
  GCS 中转 + worker 进程间路由，单客户端任务速率上限 ~330/s（8 线程
  提交也无法突破，瓶颈在客户端提交路径）；双引擎为进程内 MPSC + 共享
  内存调度，无序列化（`BoxedMessage` 指针传递）。
- **纯计算持平**（2G 迭代 1.92s vs ~2.3s）：CPU burn 在各自进程内执行，
  引擎开销摊薄为零——验证基准的"计算时长对等"缩放正确。
- **饥饿隔离 35x**：Ray 进程级 actor 天然隔离（设计使然），双引擎用
  双车道调度 + reduction 预算把差距压在同一数量级内（0.1ms vs 3.5ms）。
- **slow-consumer 失败**（5000 任务突发 + 90s 未排空）：Ray 客户端在
  大量 pending task 下提交/回收严重退化，这是 Ray 任务模型的固有限制
  （对照：双引擎同场景 40k 消息 8s 排空）。
- 结论：Ray 适合**粗粒度任务编排**（分钟级 task、进程隔离、分布式），
  Parrot（双引擎）在**细粒度 actor 通信**上快 2-3 个数量级——两者
  定位互补，不可互换。