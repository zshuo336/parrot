# Parrot Actor Framework Tests

This directory contains tests for the Parrot Actor Framework. The tests are designed to cover code and exercise all major functionalities of the library.

## Test Files

- **test_actor_basics.rs**: Tests basic actor creation and message handling without macros.
- **test_actor_macro.rs**: Tests actors and messages created using the `ParrotActor` and `Message` macros.
- **test_async_message.rs**: Tests asynchronous message handling patterns.
- **test_sync_message.rs**: Tests synchronous (immediate) message handling patterns.
- **test_system.rs**: Tests actor system operations, including multiple systems, broadcasting, and system management.
- **test_helpers.rs**: Common utilities used across test files.
- **integration_test.rs**: A single test that runs all the individual tests.
- **engine_stress_thread.rs / engine_stress_actix.rs**: 双引擎压测套件（20 场景逐字节对齐；`--ignored --nocapture` 运行，分钟级场景约 3.5 分钟）。
- **test_correctness_suite.rs**: C1–C8 引擎语义不变量（恰好一次/FIFO/回复路由/背压精确拒绝/停止与超时语义）。
- **test_business_logic.rs**: B1–B7 业务逻辑正确性（银行守恒/统计精确/状态机/Saga 补偿零泄漏/错误契约/审计序）——模型对照 + 守恒不变量方法。
- **test_thread_advantages.rs**: A1–A5 thread 引擎结构优势专项（独占线程并行/背压四策略/内存防护/宿主嵌入/海量 spawn）。
- **test_sharded_scheduler.rs**: ADR-14 亲和性分片调度器（S1 吞吐 1.76×、S2 尾延迟硬隔离）。
- **test_message_pool.rs**: ADR-15 消息池化微基准（并发 2.83×；含池上限/脏槽复用断言）。
- **test_steal_bench.rs**: ADR-16 work-stealing 评估（StealDeque/StealRing 正确性 + 与中央队列对比，结论：粗粒度下中央队列最优）。
- **test_elastic_scaling.rs**: ADR-11 弹性扩缩容（饱和保护/收缩后复救/线程上限）。
- **test_adr_stress_suite.rs / test_akka_parity_suite.rs**: ADR 回归聚合与 Akka 对等性检查。
- **test_dispatch_matrix.rs / test_derive_dispatch_paths.rs**: 派发路径与宏生成代码覆盖矩阵。
- **test_usage_scenario_catalog.rs / test_thread_engine_scenarios.rs**: 使用场景目录与 thread 引擎场景演练。
- **test_actix_adapter_coverage.rs**: actix 适配层覆盖（含 raw context 直用）。

外部基准（跨语言/跨框架，不在 cargo test 内）：
- `bench/akka-bench/`（Akka Typed 2.6.20 对等实现，`./run.sh`）
- `bench/actix-bench/`（纯 actix 零 parrot 依赖，量化 parrot 包装税，`./run.sh`）


## Running Tests

You can run all tests at once using:

```bash
cargo test --test integration_test
```

Or run an individual test file using:

```bash
cargo run --bin test_actor_basics
cargo run --bin test_actor_macro
cargo run --bin test_async_message
cargo run --bin test_sync_message
cargo run --bin test_system
```

## Test Coverage

These tests aim to achieve over 95% code coverage by exercising:

1. **Manual Actor Creation**: Testing basic actors created manually.
2. **Macro-based Actor Creation**: Testing actors created with the `ParrotActor` derive macro.
3. **Message Types**: Testing all message patterns (request-response, fire-and-forget).
4. **Asynchronous Patterns**: Testing async message handling with callbacks and futures.
5. **System Management**: Testing actor system lifecycle and configuration.
6. **Actor References**: Testing actor path resolution and actor lookup.
7. **Broadcast Messaging**: Testing system-wide message broadcasting.

## Debugging

If a test fails, you can run it individually with more detailed output:

```bash
RUST_BACKTRACE=1 cargo run --bin test_actor_basics
``` 