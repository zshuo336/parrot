# Parrot runtime tests

这些集成测试通过 `cargo test --test <文件名，不带 .rs>` 运行；它们不是可由 `cargo run --bin` 启动的二进制。本目录没有名为 `integration_test` 的聚合目标。以下命令在仓库根目录执行，固定编译并发为 2；这与 libtest 的测试线程数是两个不同设置。

## 常用入口

运行 `parrot` 包的默认功能测试及 doctest（不包含显式忽略的实验）：

```bash
CARGO_BUILD_JOBS=2 cargo test --locked -p parrot
```

运行一个真实目标，或定位其中一个测试：

```bash
CARGO_BUILD_JOBS=2 cargo test --locked -p parrot --test test_actor_basics
CARGO_BUILD_JOBS=2 cargo test --locked -p parrot --test test_actor_basics test_actor_message_handling -- --exact --list
CARGO_BUILD_JOBS=2 RUST_BACKTRACE=1 cargo test --locked -p parrot --test test_actor_basics test_actor_message_handling -- --exact --nocapture
```

使用过滤器时，先确认 `--list` 中出现完整测试名，再检查执行结果包含预期的非零测试数。Cargo 在过滤后运行 **0 tests** 时也可能退出成功；这不表示验证通过。模块内测试的完整名称可能包含 `tests::` 前缀。

需要优化模式时在 Cargo 参数中加 `--release`。`--test` 只选择该集成目标，不包含库单测或 doctest；`--all-targets` 也不等于运行 doctest。完整工作区及外部运行时入口见根 [Makefile](../../../Makefile) 和 [CI](../../../.github/workflows/ci.yml)，它们的功能组合、依赖和范围需分别确认，不能用本包通过替代全项目验收。

## 按验证目的选测试

- 基础行为与宏：`test_actor_basics.rs`、`test_actor_macro.rs`、`test_async_message.rs`、`test_sync_message.rs`、`test_system.rs`。
- 消息与业务不变量：`test_correctness_suite.rs`、`test_business_logic.rs`、`test_message_batch.rs`、`test_tell_admission.rs`、`test_message_options_contract.rs`。关注具体回复、顺序、准入结果和状态守恒。
- 生命周期与所有权：`test_typed_lifecycle.rs`、`test_thread_typed_cancellation.rs`、`test_shutdown_deadline.rs`、`test_factory_retirement_contract.rs`、`test_facade_shutdown_ownership.rs`。超时回执与实际清理完成须分别观察。
- 调度与并行：`test_async_scheduler_progress.rs`、`test_m4_typed_actix.rs`、`test_sharded_scheduler.rs`、`test_elastic_scaling.rs`。Sharded 的独立 owner 线程不等于硬延迟或零资源干扰保证。
- 诊断实验：`test_message_pool.rs` 和 `test_steal_bench.rs` 含普通正确性用例及显式忽略的计时实验。历史池化 2.83× 或分片 1.76× 样本不代表当前实现的稳定加速；独立队列比较也不证明生产调度器的优劣。
- `engine_stress_thread.rs`、`engine_stress_actix.rs` 共享工作量与检查器，普通用例检查其业务/收尾判据，完整重负载另行运行。`engine_stress_common/` 等辅助模块不是完整验收的替代入口。

这里列出的是导航，不是所有目标清单；以 `cargo metadata` 和实际 `--list` 输出为准。API 层的 mock 组合测试见 [parrot-api 测试说明](../../parrot-api/tests/README.md)，不能代替本目录的真实引擎测试。

## 性能与显式实验

先阅读 [压测契约](../../../docs/ENGINE_STRESS_CONTRACT.md)，再在有足够资源、没有同时编译或测量任务的主机执行。双引擎完整实验分别运行，耗时取决于主机，不承诺固定分钟数：

```bash
CARGO_BUILD_JOBS=2 cargo test --locked --release -p parrot --test engine_stress_thread thread_engine_full_suite -- --ignored --exact --nocapture --test-threads=1
CARGO_BUILD_JOBS=2 cargo test --locked --release -p parrot --test engine_stress_actix actix_engine_full_suite -- --ignored --exact --nocapture --test-threads=1
```

显式性能门禁有独立入口：

```bash
CARGO_BUILD_JOBS=2 make check-m4-perf
CARGO_BUILD_JOBS=2 make check-a5-perf
CARGO_BUILD_JOBS=2 make check-a5-stretch
```

M4 原门槛为配对延迟中位比 ≤1.15；A5 近期参考机门槛为严格 >50000 注册/秒，原 >300000/s 保留为 stretch，见 [A5 标准与实测](../../../docs/A5_PERFORMANCE.md)。这些命令运行真实门禁，提供入口不表示当前已达标。`--ignored` 只执行忽略项，`--include-ignored` 执行普通及忽略项；外部运行时、性能和长时故障实验的前置条件各不相同，不应盲目全局开启。

## 结果与覆盖率

记录源码版本、功能、profile、命令、通过/失败/忽略数量以及外部依赖。测试数量不是覆盖率，本指南不声称覆盖率达到 95%。覆盖率报告必须绑定测量源码和工具/平台、功能集合、分母与排除项；历史 HTML 不能代表当前源码。正确性测试应检查实际业务结果、失败传播及资源退休，避免只统计 `Ok`、依靠固定 sleep 推断并发，或把构建成功/零测试当作通过。
