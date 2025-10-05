# RB：远程层缺陷根因记录（Post-mortem）

> 状态：**活文档** · 2026-10-05 · 每条记录一次真实调试事件：现象 → 根因
> → 修复 → 防复发测试。新事件按序追加（RB5、RB6…），不删旧条。
> 用途：① code review 检查单 ② 同类设计的先验 ③ 测试用例的需求来源。

---

## RB1：TLV tag 冲突（DIRECT_ADDR 与 chosen_codec 同占 tag 8）

- **现象**：握手协商后 codec 退化为 pb-only，Rust↔网关消息编解码失败。
- **根因**：新增 `DIRECT_ADDR` TLV 时未扫描已有 tag 分配，与
  `chosen_codec` 撞号——解析端按前一个语义读值，得到垃圾。
- **修复**：`DIRECT_ADDR=8`、`chosen_codec=9`；握手测试补 tag 唯一性断言。
- **教训（流程性）**：**协议枚举/tag/帧类型码点是一张全局分配表**——
  新增码点必须先 grep 全部使用方（含五语言网关），再从"文档单一事实源"
  分配。handshake.rs 顶部已建 tag 表注释，新增改表不改散点。
- **防复发**：`handshake::tests::handshake_tlv_roundtrip`（tag 语义往返）。

## RB2：重排任务循环引用 → Ingress 泄漏（本轮覆盖率发现）

- **现象**：首轮实现 reorder_loop 任务持 `Arc<Ingress>`，代码"能跑"但
  关闭冲刷路径（recv None 分支）永远不可达——覆盖率报告 0% 命中暴露。
- **根因**：`Ingress.reorder_tx(map) → channel sender → 任务 → Arc<Ingress>`
  成环。任务永不退出，Ingress 永不析构，每源节点泄漏一个任务+channel。
- **修复**：任务改持 `Weak<Ingress>`；Ingress 析构 → sender drop →
  recv None → 冲刷（或 `REORDER_DROPPED_ON_SHUTDOWN` 计数）后退出。
- **教训**：**tokio::spawn + 闭包捕获 Arc 是隐式所有权转移**——编译器
  不会告诉你生命周期语义变了。任何长命任务捕获共享句柄，先画引用图，
  问一句"谁能让这个任务退出？"答不上来就是泄漏。
- **防复发**：`ro7_reorder_task_does_not_leak_ingress`（drop 后
  Weak::upgrade 必须 None）。

## RB3：未知帧类型静默断连（ROUTE_HINT 白名单事件）

- **现象**：hub 发 `ROUTE_HINT(0x24)` 给 Spoke A，A 的 Frame::decode 拒
  收 → TCP 连接被拉断 → 同连接在途的 REPLY 一起丢。表象"reply 偶发
  超时"，极难定位——错误在连接层被吞成"connection reset"。
- **根因**：decode 的帧类型白名单是新增码点时最容易漏改的散点（四层：
  frame.rs 枚举、known 白名单、各语言网关 dispatch、文档）；且失败路径
  只返回 Err 无日志——**静默失败把 5 分钟问题变成 2 小时问题**。
- **修复**：白名单补 ROUTE_HINT；本次再加固：UnknownFrameType /
  VersionMismatch / ReservedNotZero / TooLarge / Utf8 / MalformedLengths
  全部带上下文打 `tracing::warn!`（含 got 值 hex、长度、cid——一线定
  位"谁发的什么被拒了"）。
- **教训**：**match/白名单失败分支必须有日志**，尤其是"会断连接"的
  失败。没有观测的 reject = 未来的幽灵故障。
- **防复发**：`ro10_reserved_hi16_rejected` + frame_malformed 系列
  （每个 reject 路径至少一个测试断言 Err 变体）。

## RB4：`Option<ErrCode>` 双语义悬挂（try_relay_ask 早退）

- **现象**：非 hub 节点 try_relay_ask 返回 None 被调用方解释为"已转发
  "，ask 挂起到超时；测试全挂且无错误信息。
- **根因**：`Option::None` 承载了两种语义（"没有路由器" vs "已处理"）
  ——`read().ok()?` 链式传播把前者悄悄变成了后者。
- **修复**：非 hub 显式 `Some(ErrCode::ActorNotFound)`；语义拆成
  "Some(Err)=明确拒绝 / None=已转发完成"。
- **教训**：**Option 返回值禁止双语义**。一个函数的 None 只能有一个
  含义；两个结果就是两个分支（enum 或提前 Err）。
- **防复发**：RH3（RouteUnreachable 语义断言）、`ingress_dispatch_matrix`。

## RB5：RO5 断言全局计数器（测试间污染）

- **现象**：RO5 单跑绿、全量跑挂——`DEAD_TELL_DROPPED` 是全局 static，
  并行测试互相踩计数。
- **根因**：断言写了"绝对值等于"而非"相对增量"；且其他测试的副作用
  未隔离。
- **教训**：**全局计数器断言只允许 delta 形式**（before → 动作 →
  after-before），并接受其他测试的噪声（≥ 断言）或独占串行。
- **防复发**：RO2/RO3/RO8 的 delta 断言模式；RO5 改语义断言。

## RB6：regex 批量改代码误伤结构定义

- **现象**：批量给 FrameHeader 构造补 seq 字段时，正则把 struct 定义
  里的 `hop_limit: u8,` 也匹配了，插入 `seq: crate::frame::SEQ_NONE`
  到字段声明区——编译错一串。
- **根因**：跨行正则在"构造体字面量"与"struct 定义"之间没有区分度；
  贪婪匹配放大伤害。
- **教训**：**结构化修改用结构化工具**（rustfmt/IDE rename/编译器驱动）；
  批量正则后必须 `cargo check` + diff 审查再继续，绝不带病堆叠修改。
- **防复发**：无代码测试——流程规则：任何批量 sed/regex 后第一动作是
  编译，第二动作是 git diff 全读。

---

## 检查单（新协议功能合入前）

1. 新帧类型/TLV tag：扫全码点分配表（Rust + 5 网关 + 文档）✅？
2. 长命任务：引用图画一遍，"谁让它退出"有答案 ✅？
3. 所有 match/白名单 reject 分支：有 warn 日志 + 有测试 ✅？
4. Option 返回值：None 只有一个语义 ✅？
5. 全局计数器断言：delta 形式 ✅？
