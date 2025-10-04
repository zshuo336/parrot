# Parrot 显式语义变更清单

本文件记录所有**有意为之**的行为语义变更（v2 计划批准范围）。每项包含：变更前语义、变更后语义、理由、影响面、验证方式。

## 变更 #1：panic 后监督决策真正执行（M3）

- **前**：actor panic 被 `catch_unwind` 捕获后仅上报 `WorkerStateError`，actor 停止，无任何恢复动作（`SupervisorStrategy` 枚举存在但全库无消费者）。
- **后**：panic → `ChildFailure` 系统消息发往父级 → 按策略执行 Restart（窗口限频）/ Stop / Escalate。默认策略值不变（`Restart{3 次/10s}`）。
- **理由**：F8 监督执行器落地，填补执行真空；"默认值不变"保证未显式配置的用户获得合理默认行为。
- **影响面**：依赖"panic 后 actor 静默死亡"的代码（理论上不应存在——该行为本身就是 bug）。
- **验证**：M3 新增集成测试 + akka parity §2。

## 变更 #2：`Terminated` 增加 `reason` 字段并公开（M3）

- **前**：`pub(crate) struct Terminated { path: String }`（crate 私有，用户无法模式匹配死因）。
- **后**：公开 + `reason: DeathReason`（Normal/Panic/Killed/Escalated）。
- **理由**：关闭 parity GAP-1（Akka 的 Terminated 是公开库消息）；DeathWatch 消费者需要区分死因。
- **影响面**：additive，旧字段保留。

## 变更 #3：死亡/系统通知不可丢（M2）

- **前**：`Terminated` 以 `BackpressureStrategy::DropNewest` 投递——watcher 邮箱满时死亡通知被丢弃。
- **后**：系统消息（`ControlMessage::*`、`Terminated`）恒走高优先级车道 + `Block` 背压，不可丢。
- **理由**：bug 修复。系统信号可丢会破坏 watch 语义（watcher 永远收不到死亡通知且无感知）。
- **验证**：M2 集成测试（邮箱满时死亡通知仍送达）。

## 变更 #4：`MessagePriority` 默认生效（M2）

- **前**：`#[message(priority = N)]` 声明后无任何调度效果（声明未消费）。
- **后**：priority ≥70（HIGH）走高优先级车道，可越过 NORMAL 积压；<70 与现状 FIFO 等价。
- **理由**：实现已声明 API 的语义；未声明 priority 的消息默认 NORMAL，同优先级内保持 FIFO——对不使用 priority 的用户行为等价。
- **验证**：M2 集成测试（High 越过积压 / 默认消息 FIFO 保持 / 存量 FIFO 测试全绿）。

## 变更 #5：API 中立化一次性符号迁移（M6）

- **前**：规范层 `parrot-api` 含 actix 专属符号（`EngineContextHandle`、`receive_message_with_engine` 挂在规范 `Actor` trait）；derive 宏生成 `parrot::actix::*` 硬编码。
- **后**：规范面仅含引擎中立符号；actix 专属降级为引擎扩展；全部测试/例子同步迁移。
- **理由**："框架的框架"定位（D2 债务）；无历史包袱，直接全量迁移优于长期双面并存。
- **验证**：M6 语义对照表（逐文件断言核对）+ 全量测试绿。

## 预存环境性调整（非语义变更）

- `test_thread_advantages.rs` a1/a5 标注 `#[ignore]`（release-only 阈值，debug 下 burn_cpu 慢 3.4 倍）。CI 以 release 补跑。见 `docs/PERF_BASELINE.md`。
