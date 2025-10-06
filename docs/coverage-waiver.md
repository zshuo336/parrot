# DEV_09 覆盖率豁免清单（waiver）——待需求方签批

> 依据 DEV_09 §5.4：新 crate 行覆盖 100% / 分支 ≥95%；不可测项入本清单（≤10 条）。
> 报告：`docs/coverage/dev09.txt`（llvm-cov，workspace + host/runtime/loader 全 feature，忽略 tests/benches）。

## 新 crate 现状摘要（行覆盖）

| 模块 | 行覆盖 | 距 100% 缺口 |
|---|---|---|
| parrot-app/manifest.rs | 98.07% | 13 行 |
| parrot-app/planner.rs | 97.86% | 8 行 |
| parrot-app/orchestrator/* | 95.5–98.0% | 29 行 |
| parrot-app/debug/* | 92.3–99.3% | 14 行 |
| parrot-app/assemble.rs | 89.13% | 97 行 |
| parrot-app/host.rs | 69.27% | 67 行 |
| parrot-abi/lib.rs | 100% | 0 |
| parrot-abi/loader.rs | 82.33% | 41 行 |
| parrot-wasm/lib.rs | 83.75% | 45 行 |

## 豁免条目（≤10 条）

1. **W-1 parrot-app/host.rs `ProcessGatewayFactory` 子进程异常分支**（67 行缺口主体）：网关 90s 注册超时、`spawn()` 系统调用失败、`remote_ref` 拒绝——需注入故障子进程/破坏内核态资源，CI 不可稳定复现。断言路径由 chaos_scenarios（kill-9/双杀）在进程外覆盖语义。
2. **W-2 parrot-app/src/bin/cli.rs（0%）**：CLI 入口 `main`——参数解析 + 进程生命周期。端到端形态由 `app run` 冒烟（run-lab.sh）承载；llvm-cov 不跨进程归因。
3. **W-3 parrot-abi/loader.rs `dlclose` 失败 + 禁清单 `nm` 缺失分支**（41 行缺口主体）：macOS dyld 成功路径之外的内核错误分支；`nm` 不存在/非 ELF 解析失败在 macOS 构建环境不可触发（需 Linux 双测）。
4. **W-4 parrot-wasm/lib.rs epoch 死亡恢复 + store poison 后 instantiate 失败分支**：epoch 中断在 fuel 耗尽测试已覆盖主路径，poison 后重实例化的 `Err` 传播分支需破坏 wasmtime 内部状态。
5. **W-5 parrot-app/assemble.rs hooks 超时 + gateway stop 失败回滚交错分支**（97 行缺口主体）：hooks 3s 超时与 stop_gateway Err 的组合时序；现有 21 单测覆盖主回滚弧。
6. **W-6 apps/crawler-lab/src/main.rs（0%）**：集成实验室二进制——由 `run_regression.sh`（golden 12 断言）行为等价门禁承载，不属单测范畴。
7. **W-7 tools/federation-lab/src/main.rs（0%）**：CLI 分发层（<10 行有效逻辑）；twin_app/twin 核心已 99%+ 覆盖。
8. **W-8 parrot-node/src/main.rs + bin/parrot-probe.rs（0%）**：节点进程入口——由 `builtin_manifest.rs` 与 run-lab 集成承载。

（8 条 ≤ 10 上限）

## 请求

以上 8 条为"进程外/故障注入不可稳定单测"项；核心逻辑（manifest/planner/orchestrator/twin/abi-lib）已达 96–100%。请需求方签批或指明须补测条目。
