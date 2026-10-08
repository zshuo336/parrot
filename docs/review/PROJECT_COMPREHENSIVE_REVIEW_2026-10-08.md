# Parrot 项目全面评审 —— 分析过程与结论（归档）

> **归档日期**：2026-10-08  
> **评审范围**：全部 15 个 workspace 成员（约 90K 行 Rust）、5 语言 interop、docs/ 31 篇、examples、bench、deploy、CI、脚本  
> **评审角色定位**：分布式系统 / Actor 运行时 / Rust 工程资深视角（世界级标杆对照）  
> **说明**：用户规则曾写「当前实现都是 Python、忽略 Rust」；本次用户指令明确要求以 Rust/分布式专家身份做全面 review，故以本次指令为准。仓库主体为 Rust 实现。  
> **子代理**：远程/集群层 [8a608961-428d-4f35-8bad-987bc4f2d490]；应用/互操作层 [c75450d2-90d7-4256-b4e6-84e75cebff6a]（正文附录原文收录）。P0 结论已由主评审做源码复核。  
> **实证探针**：临时测试 `crates/parrot/tests/zz_tmp_probe_tell_order.rs` 已删除，结果见 §2.4；仓库工作树保持干净。

---

## 目录

1. [分析过程与方法](#第一部分-分析过程与方法)
2. [主评审报告（结论全文）](#第二部分-parrot-项目全面评审报告)
3. [附录 A：parrot-remote / parrot-node 深度评审报告](#附录-a-parrot-remote--parrot-node-深度评审报告)
4. [附录 B：应用体系 / 多语言互操作层深度审查报告](#附录-b-parrot-应用体系--多语言互操作层-深度审查报告)

---

# 第一部分 · 分析过程与方法

## 1.1 任务来源与目标

用户要求：全面 review 本项目全部内容（文档、代码、例子等），验证实现质量、架构水平、边界情况、与业界领先/世界级水平的差距，并给出各方面建议。评审以 Rust、分布式及 Actor 资深专家（世界级水平）视角展开。

## 1.2 执行步骤（时间顺序）

1. **仓库扫描**：`git log`、`git status`、目录结构、`Cargo.toml` workspace 成员、README、docs 索引。
2. **规模统计**：按 crate/目录统计 Rust LOC；interop/docs 行数；`find` + `rg` 文件类型分布。
3. **核心源码深读**（主评审亲自）：`docs/README.md`、`TECH_DESIGN_01`；`parrot-api` 的 `actor.rs`、`address.rs`、`types.rs`、`message.rs`（部分）、`engine.rs`；`parrot` 的 `actix/actor.rs`（含 transmute）、`thread/single_alloc.rs`、`thread/processor/core.rs`、`thread/scheduler/shared/worker.rs`、`thread/supervisor_exec.rs`、`thread/mailbox/mpsc.rs`、`actix/context.rs`；`TECH_DESIGN_03`、`TECH_DESIGN_07 §14`。
4. **质量信号统计**：`unsafe`/`unwrap`/`expect`/`unimplemented`/`transmute`/`std::sync::Mutex` vs `tokio::sync`；`#[test]` 数量；`SingleAllocEnvelope` 引用；CI/Makefile/scripts。
5. **构建与静态分析（后台）**：`cargo build --workspace --all-targets`；`cargo clippy --workspace --all-targets`。
6. **并行子代理深审**：
   - [远程/集群层](8a608961-428d-4f35-8bad-987bc4f2d490)：`crates/parrot-remote`、`crates/parrot-node` + 设计文档 04–06、ROOT_CAUSE；含 Wire/SWIM/Raft/Sharding/连接/测试/标杆对比。
   - [应用/互操作层](c75450d2-90d7-4256-b4e6-84e75cebff6a)：`parrot-app/config/wasm/abi`、apps、interop、tools、deploy、文档与 bench 报告可信度。
7. **全量测试（后台）**：`cargo test --workspace --no-fail-fast`，汇总 pass/fail/ignored。
8. **主评审复核子代理 P0**：`admin_allowed`、`raft advance_commit`、`frame body_len`、`seq_counters`、`quic dev_crypto_insecure`、`Cargo.lock` 是否入库等。
9. **实证探针（主评审）**：编写并运行 `zz_tmp_probe_tell_order` 验证 `ActorRefExt::tell` FIFO；运行后删除探针文件。

## 1.3 实测数据汇总（评审当日 HEAD）

| 项目 | 结果 |
|------|------|
| `cargo build --workspace --all-targets` | 成功（dev profile，约 2m 24s） |
| `cargo clippy --all-targets` | 约 11 条 warning（parrot-obs 9、parrot-remote 1、crawler-lab-dylib 1）；CI 配置为 `-D warnings` |
| `cargo test --workspace` | 115 个测试套件；**1194 passed / 0 failed / 34 ignored** |
| Rust LOC（约） | parrot ~39K；parrot-remote ~16K；parrot-api ~13K；parrot-app ~7K；合计 workspace Rust ~90K |
| docs/*.md | 约 8722 行（31 篇） |
| 非测试 `unsafe`（crates/apps/tools src） | 约 50 处（parrot-abi、single_alloc、actix transmute 等） |
| proptest/loom/turmoil/madsim/cargo-fuzz | 全仓库 0 |
| `Cargo.lock` 是否 git 跟踪 | **否**（`.gitignore` 首行忽略；Dockerfile 仍 COPY Cargo.lock） |

## 1.4 代码量参考（评审时统计）

| 单元 | LOC（约） |
|------|-----------|
| crates/parrot | 39292 |
| crates/parrot-remote | 15777 |
| crates/parrot-api | 12655 |
| crates/parrot-app | 7108 |
| crates/parrot-node | 2590 |
| interop 合计 | ~8777 |
| docs 合计 | 8722 |

## 1.5 Workspace 成员（Cargo.toml）

- crates: parrot, parrot-api, parrot-api-derive, parrot-api-derive-tests, parrot-config, parrot-remote, parrot-app, parrot-node, parrot-wasm, parrot-abi
- apps: crawler-lab, crawler-lab/dylib, websearch
- tools: federation-lab, parrot-obs

## 1.6 实证探针：tell 与 deliver 的 FIFO 对比

**目的**：验证 `test_correctness_suite` 中 C2「FIFO」是否覆盖 `ActorRefExt::tell`（fire-and-forget）路径。

**方法**：单发送者连续 20_000 次 `tell(Seq(i))` vs 同样数量 `deliver`；8 worker tokio runtime；等待 journal 收齐。

**结果（`cargo test -p parrot --test zz_tmp_probe_tell_order -- --nocapture`）**：

```
[PROBE] received=20000 inversions=7998 first20=[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19]
[PROBE] deliver: received=20000 inversions=0
```

**解读**：约 40% 相邻对发生逆序（7998/19999 窗口）；`deliver` 直投 mailbox 保持严格递增顺序。C2 测试使用顺序 `ask`，不能发现此问题。

## 1.7 子代理 P0 主评审复核摘要

| _claim_ | 复核 |
|--------|------|
| `admin_allowed()` 恒 true | `crates/parrot-remote/src/admin.rs:295-299` 确认 |
| Raft 偶数节点 commit 中位数 | `raft/mod.rs:535-543` 确认 |
| frame 解码 panic | `frame.rs` 先 `copy_to_bytes` 后校验；子代理 PoC |
| TELL seq 在 RemoteInner | `ref_.rs:33` + 每次 `Default::default()` 确认 |
| QUIC `dev_crypto_insecure` | `system.rs:244` 确认 |

---

# 第二部分 · Parrot 项目全面评审报告

## 一、总体判断（先说结论）

**一句话**：这是一个野心极大、文档极丰富、工程卫生良好、但"深度严重不均"的项目。进程内双引擎核心已经接近"可用的中等水平 actor 框架"；而文档中宣称的 SWIM / Raft / Sharding / mTLS / 热卸载 / 百万节点 / 99.999% 等分布式与联邦能力，**绝大多数停留在"库级状态机 + 自测自证"阶段，没有接入运行时**，且存在多个 P0 级安全与正确性缺陷。

**客观健康指标（实测）**

- `cargo build --workspace --all-targets`：0 错误、0 警告
- `cargo clippy --all-targets`：11 条警告（parrot-obs 9、parrot-remote 1、crawler-lab-dylib 1）——CI 用 `-D warnings`，**当前 HEAD 实际过不了自家 CI**
- `cargo test --workspace`：115 个套件、**1194 通过 / 0 失败 / 34 ignored**
- 代码规模：Rust ≈ 90K 行（文档里写的 15,276 行是 10/1 的旧值，严重滞后）
- 节奏：~70 个 commit、~75K 行在 7 天内落地（10/1–10/7），日均 >10K 行——这解释了下面所有"形状对、接线断"的模式

**分维度评分（满分 10，以业界一线开源项目为 8–9 分基准）**

| 维度 | 分 | 一句话 |
|---|---|---|
| 工程卫生（编译/测试/文档密度） | 8 | 零警告编译、1194 测试全绿、ADR 式文档，优于绝大多数开源项目 |
| 进程内 actor 核心（thread 引擎） | 5.5 | 能跑、有背压/监督/亲和调度，但有 FIFO 违约、每批次 spawn_blocking 等根本性问题 |
| actix 适配层 | 4 | 并行与异步路径已修；但 context 大面积是桩（stop/watch/spawner/stream 均未实现） |
| API 设计 | 4 | 抽象重叠严重（3 套 actor 面、4 套克隆、2 套 Factory/SystemError）、`BoxedMessage` 当 actor 传 |
| unsafe 与内存安全 | 4 | 有 SAFETY 注释，但 3 处论证不成立（transmute 别名、dylib 卸载 UAF、EngineContextHandle 无生命周期） |
| 远程/集群/联邦 | 2.5 | Wire 帧 + TCP/QUIC + hub 中继是真的；SWIM/Raft/Sharding/mTLS/重连未接线且含 P0 |
| 安全 | 1.5 | admin 面零鉴权 = 全栈远程代码执行；QUIC 默认跳过证书校验 |
| 测试方法学 | 4 | 数量大但多为脚本化状态机/宽松断言；无 proptest/loom/fuzz/确定性仿真 |
| 文档与代码一致性 | 3 | 自我批判态度好，但"完成度"叙述系统性超前于实现 |

**与"世界级"的距离**：进程内核心离 Akka Typed / Erlang OTP / Ractor 这类成熟运行时还有 1–2 个版本周期的距离；分布式层离 Akka Cluster / memberlist / openraft 还有数量级的距离。项目目前**没有任何一个方面处于业界领先**；最接近"有特色"的是：多语言 Wire 1.0 同构帧 + hub 中继的 POC 实证、Sharded 亲和调度器的量化实验（ADR-14/16 做法很专业）、以及"把实验失败也写进 ADR"的工程文化。

---

## 二、架构评估

### 2.1 做得好的架构决策

1. **"规范 / 实现 / 语法糖"三分**（parrot-api / parrot / parrot-api-derive）方向正确，derive 已改为 `__parrot_engine` 别名注入消除了对实现层的符号依赖。
2. **双引擎实证**：thread 引擎与 actix 适配层各有压测对照（`ENGINE_STRESS_REPORT`），并用纯 actix 基准量化了包装税（ADR-17 −45% 并发 ask），这是罕见的诚实。
3. **ADR 文化**：ADR-15（消息池首版因全局原子计数反而慢 3.4×）、ADR-16（work-stealing 微基准否决默认替换）——把否定结论留下来是成熟团队的做法。
4. **Wire 1.0 协议设计**：28B 定长头 + LE + 长度前缀 + golden vectors + 五语言实现，协议层面思路正确。

### 2.2 架构层面的硬伤

**H1 · API 面碎片化（parrot-api）**

同一个概念存在多套互不兼容的抽象：

- Actor：`Actor`（async BoxedFuture）/ `ErasedActor`（同步 receive）/ `ActixEngineExt`
- Ref：`ActorRef` / `ActorRefErased`（**同步阻塞 ask**）/ `WeakActorRef`（无 upgrade）/ `ArcActorRef` / `TypedActorRef` / `BoxedActorRef` / `WeakActorTarget`
- 克隆：`CloneableMessage` / `CloneableMessageTrait` / `AnyMessage` / `BoxedMessageClone`
- 工厂：`actor::ActorFactory<A>` 与 `context::ActorFactory` 同名异构；`SystemError` ×2
- `types.rs:55`：`pub type WeakActorTarget = Arc<dyn ActorRef>` ——名字叫 Weak 实际是强引用，文档"弱引用治理循环依赖"的叙述与此矛盾
- `engine.rs:61`：全局 `static ENGINE_REGISTRY: Mutex<HashMap>` 可变全局状态

**H2 · 类型安全在关键入口丢失**

`context.rs:65-69`：`ActorSpawner::spawn(actor: BoxedMessage, config: BoxedMessage)` ——把 actor 实例当 `Box<dyn Any>` 传，运行时 downcast 失败才报错。`message.rs:410`：生产代码里 `type_name_of_val(..).contains("TestMessage")` 测试逻辑泄漏。

**H3 · 文档声明层级与实现层级错位**

`TECH_DESIGN_07 §14` 把"世界级/工业级/电信级 99.999%/百万节点"定为**全系统验收基线**，列了 G1–G7 门禁（fuzz、semver-checks、覆盖率 ≥85%、混沌矩阵、`deny(missing_docs)`）。实测：仓库里**没有** cargo-fuzz、semver-checks、覆盖率 CI、missing_docs lint、混沌脚本；CI 只有 fmt/clippy/test 三步且仅 macOS self-hosted。

---

## 三、进程内核心：逐项缺陷（主评审亲自验证）

### P0-A · `ActorRefExt::tell` 破坏每发送者 FIFO（实证）

```276:281:crates/parrot-api/src/address.rs
    fn tell<M: Message>(&self, msg: M) {
        let actor_ref = self.clone_boxed();
        tokio::spawn(async move {
            let _ = actor_ref.deliver(Box::new(msg) as BoxedMessage).await;
        });
    }
```

每次 `tell` 单独 `tokio::spawn` 一个任务，多线程运行时下任务执行顺序不保证。探针（单发送者连发 20,000 条 `tell`）：**7,998 处逆序（40%）**；同样数据用 `deliver` 直投：0 逆序。Akka/Erlang/Orleans 都把"同一对 (sender, receiver) 之间 FIFO"作为基本契约。而 `test_correctness_suite.rs::c1_c2_c3` 的"C2 FIFO"用的是**顺序 await 的 ask**，天然有序，测不出这个问题。另外 `tokio::spawn` 在非 tokio 上下文直接 panic，且每条消息一次任务分配。

### P0-B · thread 引擎每个 mailbox 批次 `spawn_blocking + block_on`

```262:266:crates/parrot/src/thread/scheduler/shared/worker.rs
                    let rt = self.runtime_handle.clone();
                    let task = tokio::task::spawn_blocking(move || {
                        // block_on a dedicated blocking thread: the future
                        // (and any synchronous handler inside) runs here.
                        rt.block_on(fut)
                    });
```

ADR-11 为了修"CPU handler 饿死调度器"，把**所有** actor 批次都丢到阻塞线程池并 `block_on`。后果：① 每批次一次线程切换，短 handler 延迟与吞吐都受损；② async handler 的 IO 等待占着阻塞线程（默认上限 512），一旦 >512 个 actor 同时在 `ask` 链上等待，阻塞池耗尽 → **全系统死锁**（A 等 B，B 批次拿不到线程）；③ 这实质上把"异步 actor 运行时"退化成了 thread-per-batch 模型。正确做法是让 CPU 密集 actor 显式选 `DedicatedThread` 模式（框架已有），或 `block_in_place`、或独立 CPU runtime，而不是全局改道。

### P1-C · actix 异步路径的 `transmute` 别名论证不成立

```265:266:crates/parrot/src/actix/actor.rs
        let handler_fut: BoxedFuture<'static, ActorResult<BoxedMessage>> =
            unsafe { std::mem::transmute(handler_fut) };
```

handler future 持有 `&mut self.inner` / `&mut self.ctx`；随后 actix `ContextFut` 每次 poll 都会构造一个新的 `&mut ActixActor<A>` 传入 `AsyncDispatchFuture::poll(_act: &mut ActixActor<A>, ..)`，并在 stopping 路径调用 `Actor::stopping(&mut act)` 写 `self.state`。在 Stacked/Tree Borrows 模型下，对父结构体重新取 `&mut` 会使子字段的活跃 `&mut` 失效——这是 UB，不是"文档化不变式"能覆盖的。`scripts/miri.sh` 只跑 `single_alloc`，从未覆盖这里。

### P1-D · `EngineContextHandle` 可在安全代码中逃逸

`actor.rs:141-185`：结构体无生命周期参数，`downcast_ref(&self) -> Option<&T>` 返回借用绑定在 handle 上而非原始 ctx 上。handle 虽因 `NonNull` 为 `!Send` 不能存进 actor 字段，但可存进 `thread_local!`/`Rc<RefCell>` 跨调用使用 → 悬垂。应改为 `EngineContextHandle<'a>` 携带 `PhantomData<&'a mut ()>`。

### P1-E · 监督语义与 Akka 不等价且有计数 bug

- **restart = 新 mailbox**：`supervisor_exec.rs:176-186` 走 `spawn_at_path` 重新 `spawn_at`，旧 mailbox 的待处理消息全部丢弃，所有既有 `ThreadActorRef`（持旧 mailbox 的 `Weak`）全部变死——Akka 中 ActorRef 跨 restart 稳定、mailbox 保留。自称 "akka parity suite" 但这里不对等。
- **重启预算双计数**：`decide()` 在 Restart 分支 `restarts.push(now)`（L93），成功后 `record_restart` 再 push 一次（L190）。`max_retries=3` 实际只允许 2 次重启。`test_m3_supervision.rs:287` 用 `<= 3` 宽松断言恰好测不出来。
- `Resume` 对 panic 一律当 `Stop`；`OneForAll` 在 thread 引擎只做了枚举映射，无执行面。
- 重启后子 actor 的自定义策略被 `default_supervisor_strategy` 覆盖（L177）。

### P1-F · actix 上下文大面积为桩

`actix/context.rs`：`stop()` 返回空 `Ok`（L110-119）；`schedule_periodic` 是**不返回的无限循环 future**（L193-199，await 即永久挂起且无法取消，D7 自 10/1 挂到现在）；`watch/unwatch` 返回 Err；`stream_registry/spawner` 直接 `unimplemented!`。文档说"双引擎能力矩阵完全等价"，在 context 层并不成立。

### P2 级

- `single_alloc.rs`（281 行、11 处 unsafe、宣称 "M5 Phase B 单块信封"）**全仓库零调用方**——带 unsafe 的死代码。
- `processor/core.rs:273-274`：每条消息两次 `tokio::sync::Mutex::lock().await`（actor + context），且 `mailbox.pop_item().await`/`is_empty().await` 都是 async trait 虚调用，一条消息多次装箱 future。
- `mpsc.rs:272-289`：高优先级 Block 策略用 1ms sleep 轮询等容量，10s 兜底；`DropOldest` 在生产者侧 `receiver.try_recv()` 与消费者竞争，两个生产者同时满时会多丢。
- `worker.rs:108`：`panic_counts: HashMap<String, usize>` 按 path 累积永不清理。
- `derive/actor.rs:188-189`：`config` 类型解析失败静默回落 `EmptyConfig`，吞掉用户错误。
- `ParrotActorSystem::status()`（system.rs:770-779）全部硬编码 0；`uuid_v4_simple` 是"毫秒+计数器"并非 UUID。
- 依赖：`atty 0.2`（RUSTSEC-2021-0145，已无维护）、`lazy_static`（应换 `std::sync::OnceLock`）。

---

## 四、远程 / 集群 / 联邦层（parrot-remote / parrot-node）

### 4.1 真实度地图（主评审复核子代理判断）

| 模块 | 状态 |
|---|---|
| Wire 帧 / TLV 握手 / TCP / QUIC / mem transport / ingress 分发 / hub 中继 / admin 远程 spawn | **真实运行** |
| SWIM（swim.rs 897 行） | 合并器正确；`Swim::run` **无人调用**，`tick()` 从不推进，`indirect_probes/gossip_fanout` 从未读取 |
| Raft（raft/mod.rs 888 行） | 进程内教学级：**零持久化**、墙钟、无 pre-vote/check-quorum/snapshot/成员变更；`RaftRpc` 只在内存 `TestNet` 流转 |
| Sharding / Singleton / Durable / Directory / Topology / Cache / ACL | 纯状态机，**未接入 facade 路由** |
| tls.rs mTLS | **死代码**（TcpTransport 不调用） |
| 重连 / 退避 | 不存在（`transport_seed_table` 是空函数） |

### 4.2 P0 缺陷（均已源码复核）

1. **远端一包 panic**：`frame.rs:497` 先做 `body_len - 28 - path_len - key_len` 再在 L498 检查；`copy_to_bytes(path_len)` 在 remaining 不足时 panic。panic 发生在连接任务里，`on_disconnect` 不执行 → 挂起的无超时 ask 永久泄漏。
2. **TELL 静默丢失**：`seq_counters` 挂在每个 `RemoteInner`（`ref_.rs:33`，每次 `remote_ref()`/`get_actor` 都 `Default` 新建），接收端按 `from_node` 重排，第二个 ref 的 seq=1 被判"迟到帧"丢弃。子代理 PoC：10 条投递 1 条。这是最常见用法（多次 `get_actor` 后 `deliver`），**没有任何测试覆盖**。
3. **零鉴权管理面 = RCE**：`admin.rs:295-299` `admin_allowed()` 恒 `true`；任意 TCP 对端发 TLV 握手 + `SYSTEM_EVENT` 即可远程 spawn/stop/deploy（含 dylib/wasm 本地路径）。三语言网关 admin-v2 同样无认证（见附录 B P0-15）。
4. **Raft 偶数节点提交违反多数派**：`raft/mod.rs:543` 取中位数 `indexes[len/2]`，4 节点时 2 副本即提交；应为 `indexes[len - quorum]`。加上零持久化，重启后同任期可重复投票。
5. **QUIC 默认 `dev_crypto_insecure()`**（system.rs:244）：客户端跳过所有证书校验、SNI 硬编码 localhost。

### 4.3 P1 缺陷

- 读/写/心跳同一个 `select!` 任务 + 全局 1024 入站队列跨连接共享 → 背压→心跳饥饿→对端 10s 判半开→**背压演化为级联断连**。
- 入站 ASK 每条 `tokio::spawn` 且 `local_ref.send()` 无超时 → DoS 面。
- hub `RelayTable` 只在 `take` 时惰性过期，REPLY 永不到达的条目永久泄漏。
- 入站 gossip 无条件合并进成员表，任何连接可标任意节点 Dead。
- `tls.rs::extract_cn` 扫到的是 **Issuer** CN 而非 Subject CN。
- `HopExceeded` 定义了但全仓库从不产生；中继只 `hop_count+1` 不检查上限。
- follower 伪造 `match_index` → leader 索引越界 panic。

### 4.4 测试深度

209 单测 + 60 集成 + 38 node 测试，帧/TLV/合并器/哈希环的纯函数测试质量不错。但：无 proptest/loom/turmoil/madsim/fuzz；SWIM/Raft 集成测试多为脚本化状态机；无 TCP RST/kill/恶意帧/慢读者背压下心跳测试；无"多次 get_actor 后 deliver"回归。

---

## 五、应用体系 / 多语言 / 工具链

### 5.1 parrot-abi（dylib 热卸载）——问题最严重的 crate

- **P0 TOCTOU → UAF**：`loader.rs:232`（检查 `quarantined`）与 L251（in-flight +1）不在同一临界区；`unload` 在 L279 置位、L284 看到 busy==0、L335 `dlclose`，线程 A 随后读已释放 vtable 并跳入已卸载代码页。"drain 栅栏"在现有实现下不是栅栏。
- **P0 cdylib 自带 std TLS 析构**：合规 fixture 的 `nm` 显示 `__tlv_atexit` + 多个 `3std...$tlv$init`；`loader.rs:393-395` 的禁止清单扫描**刻意豁免了 `3std`** 符号。dlclose 后线程退出必 UB。业界已基本放弃"运行时 dlclose Rust cdylib"。
- P1：禁止清单扫描只匹配 Mach-O 格式，Linux 下 GNU nm 输出永不命中且静默；宿主侧 `catch_unwind` 不能跨另一份 std 的 unwind。

### 5.2 parrot-wasm / parrot-app

- `memory_limit_mb` 全仓库无消费者（没接 `ResourceLimiter`）；`tick_epoch()` 只在测试里调用，生产无 epoch 驱动线程→抢占形同虚设；WIT 没有 `send/ask/spawn` 宿主函数，Wasm 组件只能被动应答。
- Wasm/Dylib 制品在本地装配器（assemble.rs:240-245）与远端执行器（parrot-node lib.rs:457-465）**两条路径都被拒绝**——`crawler.app.toml` 里的 Dylib 组件根本部署不了；`parrot-abi`/`parrot-wasm` 与编排层之间零接线。
- `component.config` 恒为 `None`（supervisor.rs:342、host.rs:305）；`stop_gateway` 停一个组件会 `kill_all()` 三个网关（host.rs:336-342）；示例 manifest 的 `[config_overlay]` 过不了自己的 `apply_overlay`。

### 5.3 示例应用与五语言互操作

- `crawler-lab` / `websearch` 的 `main.rs` **零 parrot-app 依赖**，自己手拼 `ComponentDeploy`；`*.app.toml` 仅被 dev-dep 测试读取。09 §13 "迁移完成前应用体系不算交付"——按其自身标准未交付。
- 帧布局五语言一致、4 条 golden 向量都能过——这是真的。但 Python/Erlang/TS 解码器**无最大帧长**；**admin-v2 = 未认证远程代码执行**（三网关均无 TLS、无 token）；golden 向量仅 4+6 条，Erlang 与 cpp 把 hex 硬拷进源码；admin-v2 body 用 Rust `bincode` 派生顺序做五语言契约。

### 5.4 federation-lab "数字孪生" 与 parrot-obs

`twin.rs` 的 `verify()` 只是：生成地址字符串 → 解析回来 → 再格式化一次 → 比对相同 → `HashRing::node()` 非 None。**它不接触 Directory、路由表、SWIM、border 转发中的任何真实代码路径**，是字符串格式化的自反测试；"百万地址 100% 正确"必然成立且与系统正确性无关。

`parrot-obs`：轮询 CLI + admin-v2 MetricsReport；没有 Prometheus/OTel 导出（OBSERVABILITY.md 自己列为 TODO）。

### 5.5 构建 / CI / 仓库卫生

- **`Cargo.lock` 被 `.gitignore` 忽略**，而 `deploy/Dockerfile:12` `COPY Cargo.toml Cargo.lock`。干净 clone 下 docker build 失败。
- CI 仅 `self-hosted, macOS`；不跑 `make test-polyglot`、不构建 Docker、不跑覆盖率、无 Linux job。
- 覆盖率：`docs/coverage/dev09.txt` 的 Branches 列全为 `0 0 -`——**从未测过分支覆盖**，而 waiver 写"分支 ≥95%"。
- 二进制入库：6 个 jar、23 MB `gw-jvm-libs.tar` 在工作区根。

### 5.6 性能/规模报告可信度

`PERF_BASELINE` / `ENGINE_STRESS_REPORT` 标注硬件与 warmup 是优点；CPU 场景对齐墙钟而非计算量、单次无方差、Ray 降级规模并排给 "1680x" 等数字需谨慎引用。`SCALE_REPORT.md` 的 12 行"✅"**没有一行来自真实多节点运行**。

更完整的缺陷清单、文档不一致表、业界标杆段落见**附录 B**（子代理原文）。

---

## 六、文档评估

**优点**：31 篇、8.7K 行，mermaid 图齐全，ADR 式决策记录，F 台账/D 设计债态度诚实。

**问题**：README/03 代码量与测试数严重滞后；完成度叙述系统性超前；E 系列 G1–G7 门禁未进 CI。

建议在每个设计文档加一列**"状态：已接线 / 库级可用 / 仅设计"**。

---

## 七、与业界标杆的差距矩阵

| 维度 | Parrot | 标杆 |
|---|---|---|
| 消息顺序 | `tell` 经 `tokio::spawn`，FIFO 40% 违约 | Akka/Erlang：pairwise FIFO 是基本契约 |
| 执行模型 | 每批次 spawn_blocking+block_on | Ractor/Kameo/Actix：任务级协作调度，CPU 密集由用户显式隔离 |
| 监督 | restart 换 mailbox、ref 失效、预算双计数 | Akka：ref 稳定、mailbox 保留、OneForAll/AllForOne 真执行 |
| 失败检测 | 固定心跳；SWIM 未运行 | memberlist：随机 probe + ping-req + Lifeguard；Akka：Phi Accrual |
| 共识 | 内存 Raft，偶数节点不安全 | openraft/tikv-raft：Storage trait、pre-vote、joint consensus、snapshot |
| 分片 | 哈希环，无 handoff，单活不保证 | Akka Sharding / Orleans 分布式目录 |
| 传输 | 单任务读写心跳、全局队列 | Artery 控制/数据流分离；Erlang dist 独立 tick |
| 安全 | 零鉴权、TLS 死代码 | Erlang cookie+TLS；Akka mTLS+角色 |
| 插件 | dlclose Rust cdylib | abi_stable / 永不卸载 / 进程隔离 |
| 跨语言控制面 | bincode 派生顺序 | protobuf/gRPC（Dapr/Akka） |
| 测试 | 脚本化状态机 | Jepsen、turmoil/madsim、loom、proptest、cargo-fuzz |

---

## 八、建议路线（按优先级）

**立即（安全/正确性，1–2 周）**

1. `admin_allowed` 真实判定 + 握手预共享 token + `cluster/realm` 比对；QUIC 去掉默认 insecure；三语言网关解码器统一 16 MiB 上限。
2. `frame.rs` 先校验再切片；panic 必须触发 `on_disconnect`；接 cargo-fuzz。
3. `seq_counters` 上提到 `RemoteActorSystem` 按目标节点全局；补"多次 `get_actor` 后 `deliver`"回归。
4. `ActorRefExt::tell` 改为同步 `try_send`/有界队列入队，不再 `tokio::spawn`；把 FIFO 测试改成真·并发 tell。
5. `parrot-abi`：隔离检查与 in-flight 自增同一锁；对 dlclose 做诚实决策（推荐只加载不卸载）。
6. Raft：`indexes[len - quorum]`；明确标注实验性/未持久化/未上网，或换 openraft。

**短期（1 个月）**

7. 修正 ADR-11：撤回全局 `spawn_blocking`，CPU 密集走 `DedicatedThread`。
8. 监督：restart 复用 mailbox 与 ref；修双计数；OneForAll 真执行；补精确断言。
9. actix context：实现 stop/watch/spawner，`schedule_periodic` 改后台任务+取消句柄。
10. unsafe 整改：`EngineContextHandle<'a>`；actix 异步路径不用别名 transmute；miri 覆盖；删除或接线 `single_alloc.rs`。
11. `Cargo.lock` 入库、`rust-toolchain.toml`、CI 加 Linux + `test-polyglot` + 分支覆盖率；清 clippy；清理入库二进制与 `gw-jvm-libs.tar`。

**中期（1 个季度）**

12. API 收敛：一套 Actor trait、一套 Ref（`Arc` 基）、一套克隆、一个 Factory/SystemError；`WeakActorTarget` 改名或真 Weak。
13. SWIM 真接线或文档降级为"静态种子 + 心跳半开"。
14. 传输层拆读/写/心跳、per-连接队列、重连退避、ASK 限流+超时、RelayTable 清扫。
15. 引入 turmoil/madsim、proptest、loom；补 TCP RST/kill/恶意帧测试。
16. admin-v2 改 protobuf；扩充 golden vectors。
17. 文档三态标注；E 系列基线降为愿景直至门禁进 CI。

---

## 九、最后的评价

这个项目最大的资产不是代码，而是**它的工程叙事能力**——ADR、量化实验、失败记录、设计债台账，这些是世界级团队才有的习惯。最大的风险也在同一处：叙事的速度远远超过了实现的速度，7 天 75K 行的产出让"形状正确"大面积取代了"接线正确"，并且测试套件在用宽松断言和自反验证为这种错位背书。

如果把文档中所有未接线的能力如实降级，Parrot 是一个**有潜力的进程内多引擎 actor 框架 + 可用的多语言 Wire 协议 POC**，这个定位本身已经有价值。要走向"世界级"，接下来一个季度该做的不是再加能力，而是：修 5 个 P0、砍掉死代码、把 CI 门禁做实、让每一个 ✅ 都能被一条端到端测试证伪。

---


# 附录 A · parrot-remote / parrot-node 深度评审报告

> 子代理 ID：`8a608961-428d-4f35-8bad-987bc4f2d490`  
> 以下正文为子代理输出原文归档（与评审当日 transcript 一致）。

> 说明：用户规则要求"忽略一切 Rust 相关内容"，但本次父任务明确要求评审 Rust crate `parrot-remote`，且该仓库全部实现均为 Rust（无 Python 实现），两者直接冲突。本报告按父任务执行；若该规则应优先，请父代理裁定。

# parrot-remote / parrot-node 深度评审报告

## 一、概览与文件地图

`crates/parrot-remote` 共 15,777 行（27 个 src 文件 + 5 个 bin），其中约 40% 是 `#[cfg(test)]` 单测；`crates/parrot-node` 约 2.6K 行。`unsafe` 使用：**0 处**（好）。

| 模块 | 行数 | 真实度 | 是否接入运行时 |
|---|---|---|---|
| `frame.rs` Wire 1.0 帧 | 1024 | 真实实现 | 是 |
| `handshake.rs` TLV 握手 | 433 | 真实实现（无鉴权） | 是 |
| `transport.rs` + `tcp.rs` + `quic.rs` + `memory.rs` | ~1560 | 真实实现 | 是 |
| `ingress.rs` 入站分发/hub 中转/TELL 重排 | 1578 | 真实实现 | 是 |
| `system.rs` RemoteActorSystem | 1063 | 真实实现 | 是 |
| `ref_.rs` / `registry.rs` | 588 | 真实实现 | 是 |
| `admin.rs` / `admin_v2.rs` 远程 spawn/部署 | 1427 | 真实实现（**无鉴权**） | 是 |
| `swim.rs` | 897 | **合并器真实；驱动循环死代码** | 仅 gossip 合并钩子接入；`Swim::run` 无人调用 |
| `raft/mod.rs` | 888 | **进程内教学级 Raft，无持久化、无网络** | 否（`RaftRpc` 从不上线） |
| `roles/directory.rs` | 578 | 库级状态机 | 否（`RESOLVE_Q` 在 transport 层收到即断连） |
| `sharding.rs` | 228 | 哈希环 + 表，**无切换协议** | 否（仅测试） |
| `singleton.rs` / `durable.rs` / `topology.rs` / `cache.rs` / `roles/hub.rs` / `roles/route_reflector.rs` | ~2300 | 纯状态机 | 否（仅测试） |
| `tls.rs` mTLS | 291 | **死代码** | 否（`TcpTransport` 不调用） |
| `acl.rs` | 241 | 纯函数 | 否 |
| `livekit.rs` / `mqtt.rs` | 593 | 领域模型 + 几个纯函数 | 否 |

**结论先行**：真正跑在网络上的只有 Wire 帧 + TCP/QUIC 连接 + ASK/TELL/REPLY 分发 + hub 中转 + admin 远程 spawn。文档所称的 SWIM、Raft、Sharding、Singleton、mTLS、重连退避，要么是没接线的库代码，要么只存在于文档中。

---

## 二、各子系统实现真实度评估

### 1. Wire 协议（`frame.rs`）——真实实现，但解码不健壮

优点：固定 LE、长度前缀、16 MiB 上限、golden vector 跨语言冻结、半包不消费。

**P0 · 远端可触发 panic（已 PoC 实证）**。解码只校验 `body_len ≤ MAX`，不校验 `body_len ≥ 28`，且 `path_len`/`key_len` 在读取后**先 `copy_to_bytes` 再做长度一致性检查**：

```473:498:crates/parrot-remote/src/frame.rs
        let path_len = buf.get_u32_le() as usize;
        let path = String::from_utf8(buf.copy_to_bytes(path_len).to_vec()).map_err(|e| {
        // ...
        let key_len = buf.get_u32_le() as usize;
        let key = String::from_utf8(buf.copy_to_bytes(key_len).to_vec()).map_err(|e| {
        // ...
        let payload_len = body_len as usize - BODY_FIXED_OVERHEAD - path_len - key_len;
        if path_len + key_len + BODY_FIXED_OVERHEAD > body_len as usize {
```

`bytes::Buf::copy_to_bytes`/`get_u64_le` 在 remaining 不足时 panic。我在 /tmp 用 path 依赖写 PoC：14 字节帧（body_len=10）→ `advance out of bounds` panic；`path_len=0xFFFFFFFF` 的完整 44 字节帧 → panic。panic 发生在 `run_connection` 的 spawn 任务里，连接死亡但 `on_disconnect` 回调**不会**执行（`transport.rs:431` 在 loop 之后），该节点的挂起 ask 永不 fail（无超时的 `send()` 永久挂起）。一个恶意/错误对端一个包即可打掉连接并泄漏回调。

其他：无校验和、无帧级认证（依赖 TCP/TLS，而 TLS 未接线）；握手协商的 `max_frame_len` 从未用于解码（`MAX_FRAME_LEN` 是常量）；`FrameError::HopExceeded` 定义了但**整个 crate 从不产生**，中转路径 `ingress.rs:422` 只做 `hop_count+1`，注释说"由对端 Frame 校验拒收"但 `decode` 并不校验。`route_hint` 的 `node.len() as u16` 截断无检查。无 fuzz 测试。

### 2. SWIM（`swim.rs`）——不是 SWIM

- `Membership` 合并器（incarnation + 状态偏序）是正确且有测试的。
- 但 `Swim::run` 的 probe 分支**每 tick 顺序探测全部存活节点**（`swim.rs:320-326`），而非 SWIM 的随机单目标；`indirect_probes`、`gossip_fanout` 配置字段**从未被读取**——没有 ping-req 间接探活，gossip 发给全部 `alive_nodes()` 而非 fanout 子集。
- 驱动循环**从不调用 `tick()` 也从不推进 `set_clock`**：Suspect 永远不会变 Dead，`mark_suspect` 硬编码 3000ms 忽略配置（`swim.rs:226`）。
- `refute()` 只在测试里被调用；运行时收到"怀疑自己"没有任何反驳路径。
- **最关键**：`Swim::run` 在 `system.rs`、`parrot-node` 中都没有被实例化。运行时只有 `AdminHook::on_event` 把任何对端发来的 gossip **无条件合并**进本地成员表（`system.rs:927-936`），没有身份校验——一个连接即可把任意成员"注入"或标 Dead。
- 无 Lifeguard，无分区处理（无多数派概念）。

集成测试 `swim_convergence_kill9` 名为 kill -9，实际是手工 `mark_suspect` + `set_clock(3500)` 的脚本（`test_swim_cluster.rs:46-73`），与网络无关。

### 3. Raft（`raft/mod.rs`）——教学级内核，有安全缺陷

是真实的选举 + 日志复制状态机，奇数节点、无崩溃场景下逻辑正确。但：

- **P0 · 偶数节点集群提交违反多数派**（已 PoC 实证）。`advance_commit` 取中位数：

```535:543:crates/parrot-remote/src/raft/mod.rs
        let mut indexes: Vec<LogIndex> = self.peers.iter()
            .filter_map(|p| self.match_index.get(p).copied()).collect();
        indexes.push(self.last_log_index());
        indexes.sort_unstable();
        let majority = indexes[indexes.len() / 2];
```

N=4 时 `indexes[2]` 只保证 2 个副本拥有该条目，而 `quorum()` 要求 3。PoC：4 节点集群，1 个 follower ack → `commit_index=1`。应取 `indexes[len - quorum]`。

- **P0 · 零持久化**。`current_term`/`voted_for`/`log` 全在内存，无 `Storage` trait、无 fsync。重启后 term 归 0、`voted_for=None` → 同一任期可投两票，选举安全性不成立。`RealClock` 用 `SystemTime`（墙钟，`raft/mod.rs:85-90`），与模块头"勿用系统时间"自相矛盾。
- **P1 · 恶意/错误 follower 可让 leader panic**（已实证）：`step_append_resp` 把 `match_index` 直接 `max` 进 `next_index`，下一次 `broadcast_append` 索引 `self.log[(ni-2)]` 越界（`raft/mod.rs:406`）。
- 无 pre-vote / check-quorum / leader lease：测试自己承认分区少数派"term 因反复竞选升高，leader 见高 term 退位重选"（`raft/mod.rs:827-828`）——即 Raft 论文已知的分区节点扰动问题未处理；陈旧 leader 可继续服务读（`DirectoryReplica::query` 本地直读）。
- 无 membership change、无 snapshot（文档已声明不做），日志无上限。
- `step_append_resp` 失败分支线性回退并对**全部** peer `broadcast_append`（放大）。
- **从未接入网络**：`RaftRpc` 只在 `TestNet` 内存总线里流转；`DirectoryShards` 每个 shard 都是 `peers=vec![]` 的单节点"集群"。

### 4. Sharding / Placement（`sharding.rs`）——只有哈希环

一致性哈希（256 vnode，FNV+splitmix）数学正确。但 `ShardCoordinator::rebalance` 只是把表项标 `Migrating` 并返回列表；没有 handoff 协议、没有 owner 确认、没有"旧 owner 停止后新 owner 才激活"的顺序约束。membership 视图在各节点独立收敛（且 SWIM 本身未运行），因此**单活（single activation）无法保证**，重均衡窗口内两节点可同时持有同一实体。无 at-most-once/at-least-once 去重机制，与 Akka ShardCoordinator（集中式 + 状态持久化）或 Orleans directory 相差一个数量级。该模块未接入 facade 路由。

### 5. 连接管理（`transport.rs` / `system.rs`）

- **无重连**。文档 05 §2 承诺"退避 1/2/4/8s 上限 30s + 定时重连任务"，实现里 `transport_seed_table` 是空函数（`system.rs:857-860`），`start()` 不连种子，全仓库无 reconnect/backoff 逻辑；`parrot-node/main.rs:66-71` 对种子只尝试一次。
- **读写同任务 HOL 与心跳饥饿**：`run_connection` 单 `select!` 里 `framed.send(frame).await`、`inbound.send(...).await`（`transport.rs:346,380,407`）都会阻塞读循环与心跳 tick。全局 `inbound` 通道容量 1024 由所有连接共享（`system.rs:199`），单个慢 actor 的 TELL 背压会让本端停止回 HEARTBEAT_ACK → 对端 10s 后判半开断连，背压演化为级联断连。双方同时大量写时存在经典互等死锁风险。
- `links: Vec` 线性扫描且允许同一 node 重复 push（`system.rs:422-425`）；`remote_ref` 是同步函数用 `try_lock`，锁忙时 `nodes` 取空 Vec（`system.rs:512-519`），返回的 ref 随机不可用。
- hub `RelayTable` 条目 TTL 65s 但**只在 `take` 时惰性检查**（`ingress.rs:86-92`），REPLY 永不到达的条目永久泄漏。
- 握手无 nonce/token/证书绑定；`cluster`/`realm` 字段解析后不比较；任意 TCP 客户端声明任意 `node_id` 即可被接纳为该节点。
- QUIC 生产配置默认 `dev_crypto_insecure()`（`system.rs:244`）：server 不要客户端证书，client `DevSkipVerify` 跳过一切校验，SNI 硬编码 `"localhost"`（`quic.rs:85`）。

### 6. 并发

- `std::sync::Mutex` 在 async 中使用的点（`ingress.learned/reorder_tx/relay_table`、`system.dial_addrs`）临界区均无 await，可接受。
- **无界任务生成**：每个入站 ASK `tokio::spawn` 一个任务（`ingress.rs:230-236`），且 `local_ref.send()` 明示"无界"无超时（`ingress.rs:336`）。对端可用 ASK 洪泛制造无限任务与回调挂起。
- `membership` 锁在 gossip 处理中被三次独立获取（`system.rs:934-946`），digest 与 members 快照之间可能不一致（TOCTOU）。
- `AdminHook` 内 `handle_admin_command` 的 spawn `.await` 在**唯一的**入站分发循环中内联执行（`system.rs:296-310` → `ingress.rs:244-247`），阻塞所有连接的 TELL/REPLY 分发。
- 单测 `heartbeat_half_open_detected` 真实等待 10s+，tokio `test-util` 已引入却仅 2 处使用 paused clock。

### 7. 错误处理

非测试代码 `unwrap/expect/panic` 约 156 处（含 5 个 bin ~60 处；库代码 ~96 处）。多数是 `Mutex::lock().unwrap()`（毒锁即崩）和 bincode `encode_to_vec().unwrap()`。吞错模式普遍：`let _ = back.send(...)`、`let _ = inbound.send(...)`，REPLY 发送失败零观测。`FrameError::from(io::Error)` 把所有 IO 错误压扁成 `MalformedLengths{0,0,0}`（`transport.rs:146-156`），错误分类丢失。

### 8. 安全（横切，最严重）

- **P0 · 未认证远程代码执行面**：`admin_allowed()` 恒返回 `true`（`admin.rs:295-299`）。任何能连到 9801 端口的客户端，发一个 TLV 握手 + `SYSTEM_EVENT` 即可 `SpawnLocal` 任意已注册工厂、`AdminStop` 任意 actor、`DeployComponent`（含 Dylib/Wasm artifact 路径——`parrot-node` 的 `materialize` 接受任意本地绝对路径）。
- `tls.rs` 全部为死代码；即便接线，`extract_cn` 手写 DER 扫描匹配的是**第一个** CN OID，而 Issuer DN 在 Subject 之前——真实 CA 签发的证书会返回 CA 的 CN，身份绑定失效（自签测试恰好 issuer==subject 所以绿）。

---

## 三、具体缺陷列表

| 级别 | 位置 | 问题 |
|---|---|---|
| **P0** | `frame.rs:388-511` | `body_len<28` 或巨型 `path_len/key_len` → panic（远端一包可触发，已实证）；panic 后 `on_disconnect` 不执行，挂起 ask 泄漏 |
| **P0** | `system.rs:520-526` + `ref_.rs:33,183` + `ingress.rs:675-678` | **TELL 静默丢失**：每次 `remote_ref()`（facade `get_actor` 的路径，`parrot/src/system.rs:400`）新建 `RemoteInner{seq_counters: Default}`，seq 从 1 重计；接收端按 `from_node` 重排，后续 ref 的 seq=1 被当"迟到帧"丢弃。PoC：10 条 TELL 投递 1 条，`LATE_TELL_DROPPED=9` |
| **P0** | `admin.rs:295-299`, `system.rs:886,904` | admin/admin-v2 零鉴权；任意连接可远程 spawn/stop/deploy |
| **P0** | `raft/mod.rs:535-543` | 偶数节点 commit 多数派计算错误（4 节点 2 副本即提交，已实证） |
| **P0** | `raft/mod.rs:130-134` | Raft 持久态全内存，重启可重复投票 |
| **P0** | `system.rs:244`, `quic.rs:160-229` | QUIC 默认跳过证书校验；`tls.rs` 未接线 |
| **P1** | `raft/mod.rs:406, 520-524` | follower 伪造 `match_index` → leader 索引越界 panic（已实证） |
| **P1** | `swim.rs:299-363` | SWIM 驱动从未调用 `tick()`/推进时钟；`indirect_probes`/`gossip_fanout` 未用；且 `Swim::run` 无调用方 |
| **P1** | `system.rs:927-936` | 入站 gossip 无条件合并，任何连接可篡改成员表 |
| **P1** | `transport.rs:328-429`, `system.rs:199` | 读/写/心跳同任务，背压→心跳饥饿→对端误判半开；全局 1024 入站队列跨连接 HOL |
| **P1** | `ingress.rs:230-236,336` | 每 ASK 无界 spawn + 无超时 `send()`，DoS 面 |
| **P1** | `ingress.rs:64-92` | `RelayTable` 无主动过期扫描，hub 内存泄漏 |
| **P1** | `system.rs:857-860`, `main.rs:66-71` | 无重连/退避，文档承诺与实现不符 |
| **P1** | `tls.rs:152-174` | `extract_cn` 取 Issuer CN 而非 Subject CN |
| **P1** | `ingress.rs:418-422` | 中转不检查 `hop_count ≥ hop_limit`；`HopExceeded` 从未产生 |
| **P2** | `ingress.rs:423-434` | 中转先 `send` 后 `relay_table.insert`，REPLY 可能抢先到达被丢 |
| **P2** | `system.rs:489-519` | `remote_ref` 用 `try_lock`，锁忙返回"无链路"的 ref |
| **P2** | `system.rs:422-425` | 同一 node 重复链路不去重，断连 `retain` 一次清光 |
| **P2** | `transport.rs:146-156` | IO 错误全映射为 `MalformedLengths{0,0,0}` |
| **P2** | `handshake.rs` / `transport.rs:270-294` | `cluster`/`realm` 解析后不校验；`max_frame_len` 协商结果不用 |
| **P2** | `sharding.rs:100-117` | rebalance 无 handoff，单活不保证 |
| **P2** | `topology.rs:182` | `hop_count + 1` 在 255 时溢出 |

---

## 四、测试深度评估

- `parrot-remote` 内 209 个单测 + `crates/parrot/tests` 60 个集成测试 + `parrot-node` 38 个。帧 golden vector、TLV、合并器偏序、哈希环分布等**纯函数测试质量不错**。
- **没有** proptest/quickcheck/loom/turmoil/madsim/cargo-fuzz（全仓库 grep 为 0）。
- Raft 的 `TestNet` 是确定性内存总线，有分区矩阵，这是最接近"模拟测试"的部分；但 `flush()` 按 HashMap 迭代序投递，无随机种子、无消息丢失/重复/延迟注入、无崩溃重启（也无法做，因为无持久化）。
- SWIM、Singleton、Sharding、Durable 的"集成测试"全部是**直接驱动状态机 + 手拨时钟**，不经网络；`swim_convergence_kill9`、`swim_partition_heal` 的命名严重高估其覆盖。
- 唯一真实网络路径测试：mem/TCP/QUIC 握手往返、hub 三节点中转、K6 多联、半开检测（真实等 10s）。没有任何测试覆盖：进程 kill、TCP RST、丢包、重排（除了单测里人造 seq）、慢读者背压下的心跳行为、恶意帧。
- 关键回归缺口：没有一个测试覆盖"多次 `get_actor` 后 `deliver`"这一最常见用法，所以 P0 丢消息未被发现。

---

## 五、与业界标杆的差距

| 维度 | Parrot 现状 | 标杆 |
|---|---|---|
| 失败检测 | 固定心跳 2s×5；SWIM 未运行 | memberlist：随机探测 + ping-req + Lifeguard 自适应；Akka：Phi Accrual |
| 成员一致性 | 任意连接可改表 | Akka Cluster：leader 驱动状态转移 + 向量时钟 gossip；memberlist：加密 gossip（keyring） |
| 共识 | 内存 Raft，偶数节点不安全 | openraft/tikv-raft：RaftStorage trait、pre-vote、joint consensus、snapshot、read-index、batch/pipeline |
| 分片 | 哈希环，无 handoff | Akka Sharding：集中协调 + handoff + remember-entities；Orleans：分布式目录 + 单活保证 |
| 传输 | 单流 TCP，读写同任务 | Artery：控制/数据流分离 + 压缩；Erlang dist：按连接心跳与 tick 独立 |
| 可靠投递 | at-most-once；seq 重排反而丢消息 | Akka：reliable delivery（ProducerController）；Erlang：连接内 FIFO 保证 |
| 安全 | 无鉴权、TLS 死代码 | Erlang cookie + TLS dist；Akka：mTLS + 角色 |
| 重连 | 无 | 全部标杆均有退避重连 + quarantine |
| 测试 | 脚本化状态机 | Jepsen（etcd/tikv）、turmoil/madsim（tokio 生态） |

Ractor/Kameo 这类轻量框架的远程能力同样有限，但它们**没有声称**拥有 SWIM/Raft/Sharding；Parrot 文档的声明与实现之间的落差才是主要风险。

---

## 六、建议（按优先级）

1. **立即修 Wire 解码**：先校验 `body_len ≥ 28`、`28+path_len+key_len ≤ body_len` 再切片；用 `Buf::remaining()` 守卫；用 `cargo-fuzz` 对 `Frame::decode`、TLV、BATCH、`parse_route_hint` 做 fuzz；panic 应触发 `on_disconnect`（把 `dis()` 放进 drop guard）。
2. **修 TELL seq 丢失**：`seq_counters` 必须挂在 `RemoteActorSystem`（per 目标节点全局），而非 `RemoteInner`；或者改为 per-连接的传输层序号并在接收端按连接而非 `from_node` 重排。补"多次 `get_actor` 后 deliver"回归测试。
3. **关闭未认证管理面**：在 TLS 接线前，至少用握手共享密钥 + `cluster` 匹配拒绝未知对端；`admin_allowed` 必须真实判定。QUIC 去掉默认 `dev_crypto_insecure`。
4. **Raft**：`indexes[len - quorum]`；加 `Storage` trait（term/vote/log 持久化 + fsync）；`match_index` 做 `min(last_log_index)` 校验；加 pre-vote 与 check-quorum；或者直接换 openraft。
5. **SWIM**：要么真正接入 `Swim::run`（随机单目标 + ping-req + 用配置 + 推进时钟 + refute），要么文档如实改为"静态种子 + 心跳半开检测"。
6. **连接层**：拆分读/写任务，心跳独立；入站队列 per-连接；加重连退避与 quarantine；ASK 并发用 `Semaphore` 限流并带超时；`RelayTable` 周期清扫。
7. **测试**：引入 turmoil 或 madsim 做确定性网络故障注入；至少补 TCP RST、kill 进程、丢包/重排的集成测试；全局计数器改 per-system 实例。
8. **文档对齐**：在 04/05/06 中明确标注"已接线 / 库级可用 / 未实现"，`ROOT_CAUSE` 文档的检查单应增加"新子系统必须有端到端接线测试"。

---

# 附录 B · Parrot 应用体系 / 多语言互操作层 深度审查报告

> 子代理 ID：`c75450d2-90d7-4256-b4e6-84e75cebff6a`  
> 以下正文为子代理输出原文归档（与评审当日 transcript 一致）。

> 说明：用户规则第 2 条要求"忽略一切 Rust 相关内容"，但本任务（由上级代理下达）明确要求审查 Rust crate，且本仓库主体即 Rust。两者冲突时我以任务指令为准完成全量审查；若需要仅审 Python 面，请明示。

# Parrot 应用体系 / 多语言互操作层 深度审查报告

## 0. 总体判断（一句话）

**数据面（Wire 1.0 帧编解码 + 三网关）是真实可跑的；控制面（parrot-app 编排、Wasm/Dylib 制品部署、"数字孪生"、规模报告）大部分停留在"自测自证"的脚手架阶段，文档对完成度的描述显著超前于代码。** 另有两处 P0 级安全/内存安全缺陷。

---

## 1. 文件地图（非测试 LOC 以实际统计为准）

| 区域 | 关键文件 | LOC | 实际用途 |
|---|---|---|---|
| `crates/parrot-app` (7.1K 含测试) | `manifest.rs` 1215（595 非测试）、`assemble.rs` 1130（540 非测试）、`orchestrator/{supervisor,rollout,health}.rs`、`planner.rs`、`host.rs` 344、`bin/cli.rs` 179 | ≈2.6K 非测试 | **仅被自身测试、`tools/federation-lab/twin_app.rs` 和 `apps/crawler-lab/tests/manifest_migration.rs`（dev-dep）引用**；两个示例应用主程序零依赖 |
| `crates/parrot-config` | `lib.rs` 1105（835 非测试） | ≈0.8K | 真实使用，质量尚可 |
| `crates/parrot-wasm` | `src/lib.rs` 503、`wit/parrot-actor.wit` 52 | 0.5K | 仅 `parrot-node::WasmActor`（feature `wasm`，默认关）包装；生产路径无调用方 |
| `crates/parrot-abi` | `lib.rs` 257、`loader.rs` 408 | 0.6K | `DylibLoader` **全仓库仅测试调用**（`rg DylibLoader` 无生产命中） |
| `apps/crawler-lab` | `src/main.rs` 788、`dylib/src/lib.rs` 102、`wasm/src/lib.rs` 70、`crawler.app.toml` | 1.7K | 手写 `parrot-remote` 编排；manifest 是装饰品 |
| `apps/websearch` | `src/main.rs` 1888、`websearch.app.toml` | 3.3K | 同上 |
| `apps/{erlang,jvm,python}` | 空目录 / 仅 `jvm/target/*.jar` | 0 | 无源码 |
| `interop/jvm` | 14 个 Scala 文件 | 2.8K | Akka Typed 网关 + admin-v2 + child-first loader |
| `interop/python` | `wire.py` 210、`admin_v2.py` 429、`ray_gw.py` 862 | 2.5K | Ray 网关 |
| `interop/erlang` | `parrot_gw.erl` 1014 | 1.5K | BEAM 网关 + `code:load_file` 热加载 |
| `interop/typescript-lite` / `cpp-lite` | `wire.ts`+`lite.ts` 576 / `parrot_lite.{h,cpp}` | 0.7K / 0.2K+ | 纯客户端，无 admin |
| `tools/federation-lab` | `twin.rs` 459、`composegen.rs` 266、`twin_app.rs` 236 | 1.1K | 见 §4 |
| `tools/parrot-obs` | `main.rs` 639 | 0.8K | 轮询 CLI + 内嵌 HTML |
| `bench/` | actix 1328 / akka 725 / erlang 507 / ray 579 | 3.2K | 可复现脚本存在 |

### unwrap/expect/panic!/unsafe 计数（非测试代码，已排除 `#[cfg(test)]` 模块、`tests/`、`benches/`、`test_support.rs`）

| crate | unwrap | expect | panic! | unsafe |
|---|---|---|---|---|
| parrot-app | 9 | 9 | 0 | 0 |
| parrot-config | 0 | 0 | 0 | 0 |
| parrot-wasm | 1 | 2 | 0 | 0 |
| parrot-abi | 7 | 0 | 0 | **18**（lib 12 / loader 6） |
| apps/crawler-lab | 43 | 1 | 5 | 6（dylib） |
| apps/websearch | 36 | 3 | 3 | 0 |
| tools/federation-lab | 2 | 0 | 0 | 0 |
| tools/parrot-obs | 2 | 0 | 0 | 0 |

（若不排除测试模块，`rg` 原始数为 parrot-app 143/9/11，大部分在 `#[cfg(test)]`。）核心 crate 的 unwrap 主要是 `Mutex::lock().unwrap()`，可接受；两个示例 app 的 unwrap/panic 密度偏高但属示例代码。

---

## 2. 区域 A：核心 crate 实现真实度与质量

### 2.1 `parrot-abi` —— 问题最严重的 crate

**真实度**：ABI 类型定义（`repr(C)`、布局断言）是规范的；加载器能跑通 macOS 下的 happy path；但"安全卸载四步协议"在并发与平台层面不成立。

**P0-1 ｜ `handle_msg` 与 `unload` 之间的 TOCTOU → use-after-free + 调用已 dlclose 代码**
`crates/parrot-abi/src/loader.rs`

```232:251:crates/parrot-abi/src/loader.rs
        if h.quarantined.load(Ordering::Acquire) {
            return Err((crate::ABI_ERR_STATE, "quarantined: no new messages".into()));
        }
        // in-flight +1（登记在案才计——外来指针拒）
        let counter = h
            .registry
            .alive
            .lock()
            .unwrap()
            .get(&(comp as usize))
            .cloned();
        ...
        counter.fetch_add(1, Ordering::AcqRel);
```
与
```279:300:crates/parrot-abi/src/loader.rs
        h.quarantined.store(true, Ordering::Release);
        // 2. Drain：等 in-flight 归零
        ...
            if busy == 0 {
                break;
            }
```
线程 A 在 L232 通过隔离检查后、L251 自增前，线程 B 在 L279 置隔离位、L284 统计 busy==0、L304–316 `destroy` 并 L335 `drop(h.lib)`（dlclose）。线程 A 随后 L252 `(*comp).vt` 读已释放内存，L259 跳入已卸载的代码页。隔离位检查与计数自增必须在同一临界区（例如在持 `alive` 锁时同时检查 `quarantined` 并自增），或改用 `RwLock`/epoch 栅栏。**"drain 栅栏"在现有实现下不是栅栏。**

**P0-2 ｜ 规范产物自身带 std TLS 析构，dlclose 后必然 UB；禁止清单扫描刻意豁免了真正的危险源**
`loader.rs:385-403` 只扫描 `$tlv$init` 且显式豁免含 `3std` 的符号。实际检查"合规"fixture：

```
$ nm -u libparrot_abi_testcomp.dylib | grep tlv
__tlv_atexit
__tlv_bootstrap
$ nm libparrot_abi_testcomp.dylib | grep 'tlv$init'
..._3std3sys12thread_local11destructors4list5DTORS$tlv$init
..._3std6thread7current7CURRENT$tlv$init
..._3std9panicking11panic_count17LOCAL_PANIC_COUNT...$tlv$init
...
```
cdylib 静态链接了自己的一份 libstd，`panic_count`/`thread::current` 等 TLS 在任何曾调用过该库的线程上都已注册 `__tlv_atexit` 析构回调，指向库内代码；dlclose 后线程退出即跳转到已卸载页。这正是 09 设计文档 §4.3 "禁止注册 TLS 析构" 想防的事，而实现把它豁免掉了（`loader.rs:393-395` 注释称"那是 Rust 运行时固有形态"）。`unload_then_reload_fresh_state`（`loader_tests.rs:380-424`）也只断言实例计数，承认"库级静态计数是否归零是平台行为"。**结论：D 阶段"真卸载"没有达成；要么接受永不 dlclose（像 glibc 对带 TLS 的库做的那样），要么要求 dylib 以 `panic=abort` + `-C target-feature=-tls` 风格的无 std-TLS 配置构建并在扫描中真正拒绝 `__tlv_atexit`。**

**P1-3 ｜ 禁止清单扫描仅在 macOS 上生效**
`loader.rs:366-376` 用 `line.trim() == "pthread_create"` 匹配 `nm -u` 输出。GNU nm 输出形如 `                 U pthread_create@GLIBC_2.34`，永远不等于裸符号名；TLS 分支只找 `$tlv$init`（Mach-O 专有）。注释声称"macOS/Linux 双平台"，实则 Linux 下 `scan_violations` 恒返回空且不报警。`nm` 不存在时也静默返回空（L354-355 自称"诚实边界"，但 `load()` 不做任何告警/拒载）。

**P1-4 ｜ 跨 FFI panic 防线有误导**
dylib 侧确实做了 `catch_unwind`（`testcomp/src/lib.rs:45-51`），这是对的。但宿主侧 `loader.rs:200-203` / `258-260` 的 `catch_unwind` 不能捕获来自另一份 std 的 unwind（Rust ≥1.81 `extern "C"` 边界 panic 直接 abort），文档称其为"第二道防线"是错误认知；`format!("{p:?}")` 对 `Box<dyn Any>` 只会打印 `Any { .. }`。`construct` 的 `.unwrap_or(99)` 把宿主 panic 变成魔数 99，而非 `ABI_ERR_*` 常量。

**P2-5 ｜ 其它**
- `AbiStr::as_str`/`AbiMsg::key`/`AbiMeta::name_str` 对 dylib 传入数据用 `from_utf8_unchecked` 并返回无界生命周期 `&'a str`（`lib.rs:47-49, 76-81, 166-171`）——`load()` L181 直接信任 dylib 的 `name`/`name_len`；`apps/crawler-lab/dylib/src/lib.rs:99-100` 的 `name_len: 12` 比 `"crawler-hub"` 多了 1 字节 NUL，名字里带 `\0`。
- 读文件算 digest（L151）与 `Library::new(path)`（L164）是两次打开，可被替换（TOCTOU）；应 hash 后以 fd/内存映像加载或至少比对 inode。
- ABI 版本只有 `abi_version == 1` 严格相等 + `parrot_min`，`AbiMeta` 无 `size` 字段，无法做前向兼容扩展（对比 wasmtime component model 的结构化版本或 C ABI 常见的 `sizeof` 首字段惯例）。
- `set_quarantined` 以 `#[doc(hidden)] pub` 暴露的测试钩子违反 09 §11 "生产代码不允许测试分支"的自定纪律。

### 2.2 `parrot-wasm`

**真实度**：wasmtime 49 component API 手写绑定、fuel/epoch、trap 后重建 Store 都存在且有测试。但：

- **P1-6 内存上限未实施**：`WasmConfig.memory_limit_mb`（`lib.rs:30`）全仓库无消费者，没有 `store.limiter(...)`/`ResourceLimiter`。文档 09 §4.2 称"内存隔离"。
- **P1-7 epoch 抢占无生产驱动者**：`tick_epoch()`（L168）只在测试里被调用；生产中无心跳线程递增 epoch，`epoch_deadline` 实际永不触发。
- **P2-8** `enter_message_scope` 重建路径 `.expect("re-instantiate after trap")`（L441-446）会在 actor 消息处理内 panic；`clock-now-ms` 文档写"单调时钟"却用 `SystemTime`（L294）。
- **设计差距**：WIT 只有 `self-ref/log/config-get/clock-now-ms`，没有 `send`/`ask`/`spawn` 宿主函数——Wasm 组件无法主动与其它 actor 通信，只能被动应答，离"Parrot 语言运行时"（09 §9）很远。`PARROT_ACTOR_WIT` 嵌入后未用于任何校验。

### 2.3 `parrot-app`

**真实度**：manifest 解析/校验、Kahn 拓扑、`RolloutTracker` 状态机、`AppSupervisor::reconcile_once` 都是干净的纯逻辑，单测充分。问题在于它们与真实执行面之间断裂：

- **P1-9 Wasm/Dylib 制品在两条路径上都被拒绝**：本地装配器 `assemble.rs:240-245` 返回 `"wasm/dylib artifacts require feature-gated runtime (C/D 阶段)"`；远端执行器 `crates/parrot-node/src/lib.rs:457-465` 对非 Props 制品返回 `DIALECT_MISMATCH`。因此 `crawler.app.toml` 里 `artifact = { Dylib = ... }` 的 crawler 组件根本部署不了；`parrot-abi`/`parrot-wasm` 与编排层之间没有任何接线。09 §12 表格中 "C. wasm `DeployComponent{Wasm}` 全链"、"D. dylib 全链" 均未达成。
- **P1-10 组件 config 被静默丢弃**：`supervisor.rs:342` 与 `host.rs:305` 都写死 `config: None`；manifest 的 `component.config` 永远到不了网关。
- **P1-11 `stop_gateway` 杀掉所有网关**：`host.rs:336-342` `if had { self.procs.kill_all(); }`——停一个组件会把 erl/ray/jvm 三个子进程全部 kill。
- **P2-12 hooks 语义与文档不符**：`fire_hook` 忽略 `_hook_path`（`assemble.rs:303-310`），永远发给 `refs.first()`；回执不匹配也返回 `Ok(())`（L321-322），"hook 回执校验"形同虚设。
- **P2-13 overlay 校验不一致**：`apply_overlay` 声称"未知键拒绝"，但 `remote.node/transport/reorder/codec` 子表未调用 `track_unknown`（L422-482），拼错键静默忽略；同时它只接受 `thread`/`remote`，而两个示例 manifest 的 `[config_overlay]`（`pages/depth/sites/port/...`）会直接触发 `Config("overlay unknown keys")`——即 **示例 manifest 过不了自己的装配器**。
- **P2-14** `host.rs` 在 `block_in_place` 内嵌 `block_on` 并 `std::thread::sleep` 轮询最长 90s（L245-256），在 current_thread runtime 下会 panic；网关 node_id 硬编码（`erl-gw-1/ray-gw-1/jvm-gw-1`），一引擎只能一个网关。
- `supervisor.rs:262` 版本比对硬编码 `"1"`；`deploy_payload` 写 `"1"` 而 `host.rs:301` 写 `"1.0.0"`，两处不一致。

### 2.4 `parrot-config`

质量最好的一个：全 `Option` 分层、未知键 warn、文档化键表、测试覆盖高。两个小点：`expand_env`（L772-793）未设变量且无默认值时替换为空串无告警；替换值若再含 `${...}` 会递归展开（可构造死循环）。

---

## 3. 区域 B/C：示例应用与多语言互操作

### 3.1 示例应用（crawler-lab / websearch）

两者的 `main.rs` **只依赖 `parrot-api` + `parrot-remote`**（`apps/*/Cargo.toml`），自己 `connect` 网关、自己拼 `ComponentDeploy`（`crawler-lab/src/main.rs:487`、`websearch/src/main.rs:28`）。`*.app.toml` 仅被一个 dev-dependency 静态测试读取。`websearch.app.toml` 把 crawler 声明为 `Dylib { uri = "file://bin/websearch" }`（一个可执行文件）且 `digest = "sha256:__BUILD__"`，`manifest::validate` 不校验 digest 格式（`manifest.rs:259-278` 无相关 variant），占位符能通过校验。`apps/erlang|python` 为空目录，`apps/jvm` 只有产物 jar。**09 §13 "crawler-lab 改造为首个多引擎 App 验收场景…迁移完成前应用体系不算交付"——按其自身标准未交付。**

### 3.2 五语言 Wire 1.0 一致性

**帧布局**一致（`[u32 len][ver u8][ft u8][flags u16][cid u64][hop u8][hop_limit u8][rsv 48][path][key][payload]`），四条 golden 向量各实现都能过。但"同一协议"在以下方面不成立：

| 维度 | Rust | JVM | Python | Erlang | TS-lite | cpp-lite |
|---|---|---|---|---|---|---|
| 最大帧长 | — | 16 MiB（`WireFrame.scala:15`） | **无限制**（`wire.py:110-118`） | **无限制** + badmatch 崩溃（`parrot_gw.erl:59-68`） | 无限制 | 16 MiB（`parrot_lite.cpp:581`） |
| TLV 声明 MAX_FRAME_LEN | — | 16 MiB | 1 MiB（`wire.py:190`） | ? | 1 MiB | — |
| TLS/mTLS | 有 `tls.rs` | 无 | 无 | 无 | 仅类型字段 | 参数位 |
| admin-v2 | ✓ | ✓ | ✓ | ✓ | ✗（设计如此） | ✗ |

- **P0-15 ｜ admin-v2 = 未认证远程代码执行**。admin-v2 命令经普通数据连接的 `SYSTEM_EVENT(0x20)` 帧投递（`ray_gw.py:705`、`AdminPort.scala:13-15`、`parrot_gw.erl:441-467`）。三网关均无 TLS、无 token、无 realm 校验。任何能 TCP 连上网关的对端可发 `DeployComponent{Beam{uri}}` → `code:load_file`、`{Jvm{uri}}` → `URLClassLoader.loadClass` 反射构造、`{PyModule}` → `importlib.import_module`。07 §3.1 写 "realm = 信任边界（mTLS 域）"，实现侧只有 Rust 节点有 TLS 代码。在任何非 loopback 部署（`deploy/parrot-stack`、`apps/websearch/deploy/docker-compose.yml` 跨容器）中这是直接的 RCE 面。
- **golden 向量**：`docs/vectors/wire1.json` 仅 4 条（ask/tell/reply-err/heartbeat），**无 HANDSHAKE/ACK、无 REPLY 成功、无 SYSTEM_EVENT、无 >64KB 载荷、无多跳**；`admin_v2.json` 6 条。Python/TS/JVM 从文件读取（JVM `WireFrameSpec.scala:20` 兜底写死 `/Users/biluochun/...` 绝对路径），**Erlang 与 cpp-lite 把 hex 硬拷到源码里**（`test_wire.erl:36-41`、`test_vectors.cpp:47-58`），且 Erlang 只对 2/4 条。向量不是"共享验证"，而是"各自抄一份"。
- **bincode 作为跨语言线格式**：admin-v2 body 用 Rust `bincode` 标准编码（varint/Option 存在字节），四个方言手写解析器（`parrot_gw.erl:138-147`、`AdminV2Codec.scala`、`admin_v2.py`）。`admin_v2.rs:38-43` 的注释已暴露脆弱性："不可加 skip_serializing_if…四方言解码器按位置读存在字节，skip 会错位致崩溃"。业界（Akka/Dapr/Orleans）在跨语言管理面一律用 protobuf/JSON；这里选择了一个没有规范文档、以 Rust serde 派生顺序为事实源的格式。
- **热加载质量**：JVM child-first loader 有 `Try(l.close())`（`AdminPort.scala:143,230`）但没有等待在途消息；Erlang `code:purge` + `code:load_file` 是 OTP 正规手段但 `purge` 会杀死仍跑旧代码的进程、没有 `soft_purge` 判断；Python 侧 `importlib.import_module` 无 `reload`，重复 deploy 同模块实际不会换代码。三者都不是"统一的升级语义"，RolloutTracker 的 Drain→Deploy→Verify→Switch 在网关侧没有对应动作（Verify/Switch 完全没有执行面）。

---

## 4. 区域 D：federation-lab 与 parrot-obs

**"数字孪生"实际验证的是什么**（`tools/federation-lab/src/twin.rs`）：
`address_of(idx)` 生成 `parrot://fed/c{c}/n{n}`（L51-55）；`expected_of()` 把它解析回来并格式化成 `tcp://10.{c}.{n%256}:7`（L114-130）；`verify()` 判据是：解析成功、再算一遍结果相同、`HashRing::node(addr)` 非 None（L133-150）。**它不接触 Directory、路由表、SWIM、border 转发中的任何真实代码路径**，是一个字符串格式化的自反测试；"百万地址 100% 正确"必然成立且与系统正确性无关。"耗时外推 ≤10min（×3 安全系数）"（`SCALE_REPORT.md:24,101`）是对这个纯函数循环的外推。`composegen` 只是 YAML 文本生成器，没有任何证据显示 50×200 compose 拓扑被真正拉起过。

`parrot-obs`：一个带 RTT/ask 探针和 admin-v2 `MetricsReport` 拉取的 CLI，内嵌轮询 HTML。没有 Prometheus/OTel 导出（`OBSERVABILITY.md:223-224` 自己列为 TODO）、没有 trace 上下文传播。`tests/e2e.sh:4` 写死 `/Users/biluochun/...`，`pkill -f 'ray_gw'` 会误杀同机其它进程。

---

## 5. 区域 E：构建、CI、覆盖率

- **P1-16 `Cargo.lock` 被 `.gitignore` 忽略**（`.gitignore:1`），而 `deploy/Dockerfile:12` `COPY Cargo.toml Cargo.lock`。干净 clone 下 docker build 失败，且发布构建不可复现。`.gitignore` 还有 `.d*`、`.c*`、`.C*`、`.D*` 这类通配，会吞掉 `.cargo/config.toml`、`.dockerignore` 等。无 `rust-toolchain.toml`。
- **CI（`.github/workflows/ci.yml`）**：仅 `self-hosted, macOS`；只跑 `cargo fmt/clippy/test --workspace` + 一个 release 测试。**不跑** `make test-polyglot`（五语言）、不跑 `twin-app`、不构建 Docker、不跑覆盖率、无 Linux job——因此 §2.1 的 Linux 扫描失效、§3.2 的向量漂移都不会被 CI 捕获。`parrot-node` 的 `wasm` feature 默认关，`wasm_actor_tests.rs` 在 `cargo test --workspace` 下**不编译**。
- **覆盖率**：`docs/coverage/dev09.txt` 是一次性 llvm-cov 文本快照，`Branches` 列全为 `0 0 -`——**从未测过分支覆盖**，而 `coverage-waiver.md` 开头写"分支 ≥95%"。根目录 `tarpaulin-report.html`（10-01，未入库）早于 DEV_09，与现状无关。waiver W-2 称 CLI "由 `app run` 冒烟（run-lab.sh）承载"，但 `run-lab.sh` 不调用 `parrot-app`（`rg parrot-app apps/ Makefile scripts/` 无命中）。
- **仓库卫生**：`bench/akka-bench/lib/*.jar`（6 个）、`apps/websearch/jvm/lib/*.jar`、`*.beam`、`*.wasm` 二进制入库；23 MB `gw-jvm-libs.tar` 在工作区根；`Dockerfile` HEALTHCHECK 直接执行 `parrot-node` 无参数（会再起一个节点？需确认 `main` 是否有 healthcheck 模式）。
- 正面：`Makefile` 分层清晰，`make test-polyglot` 存在；`deploy/parrot-mesh/gen-mesh.sh` 可参数化。

---

## 6. 区域 F：文档与报告可信度

**性能报告（`PERF_BASELINE.md`、`ENGINE_STRESS_REPORT.md`）**
- 可取之处：标注硬件（Apple M5 Pro 15 核）、日期、profile（debug/release 均标）、有 warmup（`parrot_bench.erl:154`）、统一分位数算法、记录了测量修正与噪声事件（`ENGINE_STRESS_REPORT.md:195-196`）、`bench/*/run.sh` 一键可跑。这比多数项目诚实。
- 方法学缺陷：① **CPU 场景不是同等工作量**——Erlang 迭代数按"~2200×"缩放（`parrot_bench.erl:5`）、Python 按 86× 缩放（`PERF_BASELINE.md:80-81`），对齐的是墙钟时间而非计算量，因此"CPU 并行 ~64x"等列比较的是"不同任务"；② 单次运行、无重复次数、无方差/置信区间；③ 全部在一台开发机上，无隔离；④ Ray 基线"降级版"（150 actor、3×100 spawn）与双引擎的 5000 actor 场景不对等，但表格并排给出 "1680x" 这类数字；⑤ 对 Akka 2.6.20（2022 年版本，非 Pekko/Akka 2.9）比较，JVM 参数固定 `-Xmx1g`，JIT 预热未说明轮数。结论"快 2-3 个数量级"在消息路径上大体可信（架构性差异），但具体倍数不应被引用。
- `SCALE_REPORT.md` 的 12 行"✅"中，**没有一行来自真实多节点运行**：收敛是 xorshift 扩散模拟、带宽是 `38B × 15 帧/s` 算术、内存是 `200B × 1M` 算术、孪生是 §4 的自反测试。应如实标为"模型/模拟"而非"实测"。

**设计文档与代码的主要不一致**

| 文档声明 | 代码现实 |
|---|---|
| 09 §12 "C. wasm / D. dylib `DeployComponent` 全链" | 两条执行路径均拒绝（§2.3 P1-9） |
| 09 §4.2 "内存隔离" | `memory_limit_mb` 无消费者 |
| 09 §4.3 "禁止清单 CI 扫描 … violation = 加载拒绝" | 不在 CI；Linux 无效；std TLS 豁免 |
| 09 §4.3 "验证：卸载后立刻 dlopen 同名新版本…无污染" | 测试承认静态状态是否重置"是平台行为" |
| 07 §3.1 "realm = mTLS 域" | 三网关明文、无认证 |
| 09 §11 "生产代码不允许 `#[cfg(test)]` 分支" | `set_quarantined` 测试钩子；`FakeClock` 在生产模块 |
| 09 §12 测试清单 "句柄表越界拒绝 / 无权限 ctx 调用被拒" | parrot-wasm 无句柄表、无权限模型 |
| 09 §13 "crawler-lab 改造为 App" | 两 app 零 parrot-app 依赖 |
| coverage-waiver "分支 ≥95%" | 分支覆盖未测量 |
| `ENGINE_MIGRATION_GUIDE.md` 自称 clippy 0 警告 | 可信（CI 有门禁） |

---

## 7. 缺陷清单汇总

| 级别 | 编号 | 位置 | 摘要 |
|---|---|---|---|
| P0 | 1 | `parrot-abi/src/loader.rs:232-251` vs `279-335` | 隔离检查与 in-flight 自增非原子 → 卸载期 UAF / 跳入已卸载代码 |
| P0 | 2 | `loader.rs:385-403`；fixture 产物 | cdylib 自带 std TLS 析构（`__tlv_atexit`），dlclose 后线程退出 UB；扫描刻意豁免 |
| P0 | 15 | `ray_gw.py:705`、`AdminPort.scala`、`parrot_gw.erl:441-467` | admin-v2 无认证，可远程加载任意 beam/jar/py 模块 |
| P1 | 3 | `loader.rs:359-383` | 禁止清单扫描在 Linux（GNU nm 格式）失效且静默 |
| P1 | 4 | `loader.rs:200-203, 258-264` | 宿主侧 `catch_unwind` 无法跨 FFI 生效；错误信息无内容 |
| P1 | 6 | `parrot-wasm/src/lib.rs:30` | `memory_limit_mb` 未实施 |
| P1 | 7 | `lib.rs:168` | epoch 无生产驱动者，协作抢占形同虚设 |
| P1 | 9 | `assemble.rs:240-245`、`parrot-node/src/lib.rs:457-465` | Wasm/Dylib 制品无法部署 |
| P1 | 10 | `supervisor.rs:342`、`host.rs:305` | 组件 config 恒为 None |
| P1 | 11 | `host.rs:336-342` | 停一个组件 kill 全部网关 |
| P1 | 16 | `.gitignore:1`、`deploy/Dockerfile:12` | Cargo.lock 未入库，构建不可复现 |
| P1 | — | `wire.py:110-118`、`parrot_gw.erl:59-68` | 无最大帧长，恶意 `body_len` 可耗尽内存 / badmatch 崩溃 |
| P2 | 5 | `lib.rs:47,76,166`；`crawler-lab/dylib:99-100` | `from_utf8_unchecked` 信任外部数据；name 带 NUL |
| P2 | 8 | `lib.rs:441-446, 294` | 重建路径 `expect`；"单调钟"用 SystemTime |
| P2 | 12 | `assemble.rs:303-322` | hook 路径被忽略、回执不校验 |
| P2 | 13 | `assemble.rs:422-489` | overlay 未知键检查不完整；示例 manifest 过不了装配 |
| P2 | 14 | `host.rs:49-63, 245-256` | 阻塞轮询嵌套 block_on；node_id 硬编码 |
| P2 | — | `WireFrameSpec.scala:20`、`parrot-obs/tests/e2e.sh:4` | 写死开发者本机绝对路径 |
| P2 | — | `docs/vectors/*` | 向量集过小；Erlang/cpp 硬拷贝 |
| P2 | — | `parrot-config/src/lib.rs:772-793` | 未设环境变量静默置空；可递归展开 |

---

## 8. 与业界标杆的差距

- **wasmtime component model**：wasmtime 自身提供 `ResourceLimiter`、`epoch` 后台线程惯例（`Engine::increment_epoch` 定时器）、`Linker` 能力注入、WASI 预览接口。本项目用了 component API 但没接 limiter、没接 epoch 驱动、WIT 不含 send/spawn；相较 Spin/wasmCloud 这类 actor-on-wasm 系统缺少组件间通信与资源回收语义。
- **Akka/Pekko**：Akka 的 Artery 传输有 TLS、握手鉴权、帧上限、流控（背压）、以及 `Cluster` 成员协议；这里的 JVM 网关是单连接、无认证的桥。Akka 的热部署不是靠 child-first classloader 在同一 JVM 内换类（这是 OSGi/Tomcat 的历史教训区），而是靠滚动重启。
- **Erlang 热代码加载**：OTP 的 `code:soft_purge`、`sys:suspend/change_code/resume`、`appup`/`relup` 提供了带状态迁移的两版本并存语义；`parrot_gw.erl:466-467` 的 `purge` + `load_file` 丢掉了 `code_change/3` 与软清理。
- **Orleans**：placement/instance policy/版本化 grain 接口、silo 成员资格、持久化 reminder 都有真实运行时支撑；`AppSupervisor::reconcile_once` 是正确形状的纯函数，但没有调度循环、没有持久化（`MemStateStore`）、没有 Verify/Switch 执行面。
- **Dapr**：sidecar 用 gRPC/HTTP + mTLS（Sentry）+ 组件 YAML 做多语言控制面，格式有 proto 规范；本项目用 Rust bincode 派生顺序做跨 5 语言契约，缺规范、缺版本协商（除 `tag` 外无 schema version）。
- **dylib 插件系统**（对比 `libloading` 社区共识 / Bevy dynamic plugins / `abi_stable`）：业界已基本放弃"运行时 dlclose Rust cdylib"——要么永不卸载，要么用 `abi_stable` 这类带 vtable 版本和 `#[sabi_trait]` 的稳定 ABI 层，要么走进程隔离。这里的四步协议在工程上不可能做到安全卸载，建议明确降级为"进程生命周期内只加载、不卸载；升级 = 重启节点或起新进程"。

---

## 9. 建议（按优先级）

1. **立即**：修复 §2.1 P0-1（在同一锁内检查 `quarantined` 并自增）；对 P0-2 做诚实决策——推荐"加载后永不 dlclose + 文档如实声明"，若坚持卸载，则 fixture/模板必须以 `panic=abort`、禁用 std TLS（或 `-Zbuild-std` + `no_std` 组件）构建并把 `__tlv_atexit`/`__cxa_thread_atexit_impl` 列入拒载。
2. **立即**：admin-v2 增加认证/授权（最低：握手 TLV 携带预共享 token + 节点白名单；正确做法：三网关接 TLS/mTLS，并把 `DeployComponent` 限制在 realm 内可信 node_id）。所有解码器加 `MAX_FRAME_LEN` 并统一值。
3. **短期**：把 `Cargo.lock` 入库、加 `rust-toolchain.toml`、CI 增加 Linux job 与 `make test-polyglot`、让 Erlang/cpp 向量从 `docs/vectors` 读取、把向量扩到握手/系统事件/大载荷。
4. **短期**：删除或标注 `SCALE_REPORT.md` 中"模拟/算术"行的 ✅；在 `PERF_BASELINE.md` 加入重复次数与方差，CPU 场景改为同等迭代数并单列 wall 时间。
5. **中期**：要么真的把 `crawler-lab`/`websearch` 迁到 `parrot-app run --manifest`（并让 `apply_overlay` 接受应用自定义键、`exec_deploy_v2` 支持 Wasm/Dylib），要么把 `parrot-app` 标为实验性并从 09 文档的"已交付"中移除；把 federation-lab 的 twin 改成对真实 `Directory`/路由代码的属性测试，或改名为"地址文法自检"。
6. **中期**：WIT 增加 `send/ask` 宿主函数、接 `ResourceLimiter` 与 epoch 定时器；admin-v2 管理面改为 protobuf（仓库已有 `pb.rs` 双栈）。

---

*文档结束*
