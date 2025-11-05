# Parrot API tests

`parrot-api` 提供引擎中立接口与共享组件。`src/` 内有库单测，本目录的每个 Rust 文件是独立集成测试目标。`integration_test.rs` 使用进程内 mock reference 组合 API；它验证接口行为和回复内容，不提供真实 Thread/Actix 调度、监督重启、配置执行或吞吐保证。真实引擎契约见 [parrot 测试指南](../../parrot/tests/README.md)。

## 运行

以下命令在仓库根目录执行，固定编译并发为 2。默认功能与可选远程功能分别运行；常规 `cargo test` 包含 doctest，而 `--all-targets` 不包含 doctest：

```bash
CARGO_BUILD_JOBS=2 cargo test --locked -p parrot-api
CARGO_BUILD_JOBS=2 cargo test --locked -p parrot-api --all-features
```

选择 mock 组合目标：

```bash
CARGO_BUILD_JOBS=2 cargo test --locked -p parrot-api --test integration_test
```

精确选择并发请求的回复内容用例，先核对完整名称：

```bash
CARGO_BUILD_JOBS=2 cargo test --locked -p parrot-api --test integration_test tests::test_mock_reference_concurrent_replies -- --exact --list
CARGO_BUILD_JOBS=2 cargo test --locked -p parrot-api --test integration_test tests::test_mock_reference_concurrent_replies -- --exact --nocapture
```

应看到选中并通过 1 个测试。过滤器匹配不到测试时，Cargo 仍可能退出 0；`0 passed` 不能作为有效验证。该用例发起 100 个 mock 请求并逐项核对回复与请求的对应关系；`join_all` 不证明 handler 在多个 OS 线程上并行。

在 Cargo 参数中加 `--release` 可复验优化模式。运行单个 `--test` 目标不会同时覆盖库单测、其他集成目标或 doctest。

## 范围导航

- `actor_test.rs`、`actor_factory_test.rs`、`actor_config_test.rs`：actor、factory 与配置接口；生产引擎的注册、重启及退休由引擎层测试覆盖。
- `address_test.rs`、`message_test.rs`、`errors_test.rs`：引用、消息与错误类型行为。
- `context_test.rs`、`runtime_test.rs`、`system_test.rs`、`config_test.rs`：API 及配置数据契约；构造配置对象不等于真实运行时执行该政策。
- `tell_contract.rs`、`typed_bridge_input_contract.rs`、`message_conversion_contract.rs`、`delegate_future_contract.rs`：投递、转换、委托 future 与输入所有权。
- `engine_catalog*.rs`、`local_stream_registry.rs`、`test_watch_inputs.rs`：目录、stream 与 watch 的拥有、取消和清理边界。
- `remote_decode_limits.rs` 及功能相关库测试：启用相应 feature 时的远程编解码约束；使用 `--all-features` 后仍应核对实际选中的用例。

上述是导航，不是完整目标清单。测试文件中的 mock 和受控 future 用于隔离接口边界，不能泛化为完整系统的负载均衡、容错树或跨网络可靠性验收。

## 覆盖率与性能

本指南没有与当前源码绑定的覆盖率测量，不声称“超过 95%”。既有 HTML/历史记录需核对源码版本、平台、profile、功能、工具版本、分母和排除项后才能引用；测试数量与全绿不能替代覆盖率，也不能证明所有竞争窗口都被覆盖。实际生成覆盖率前应选择本机支持的工具并明确测量范围。

`integration_test.rs` 是 mock 正确性测试，不是吞吐、延迟或恢复时间基准。真实性能实验及原 M4/A5 门禁见 [运行时测试指南](../../parrot/tests/README.md) 和 [压测契约](../../../docs/ENGINE_STRESS_CONTRACT.md)。性能数字应来自固定工作量、精确结果验证和真实清理完成后的独立测量。
