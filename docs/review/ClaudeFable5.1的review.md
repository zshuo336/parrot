# Parrot 项目全面评审报告

> 评审范围：全部 15 个 workspace 成员（~90K 行 Rust）、5 语言 interop、docs/ 31 篇、examples、bench、deploy、CI、脚本。方法：全量编译 + clippy + 1194 个测试实跑、核心路径逐行阅读、对可疑点写临时探针测试实证（已删除）、两个子代理分区深审（[远程/集群层](8a608961-428d-4f35-8bad-987bc4f2d490)、[应用/互操作层](c75450d2-90d7-4256-b4e6-84e75cebff6a)），子代理的 P0 结论我都做了源码复核。
>
> 说明：你的个人规则写"当前实现都是 Python、忽略 Rust"，但本仓库主体就是 Rust，且本次指令明确要求以 Rust/分布式专家身份评审，故按本次指令执行。

---

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

## 三、进程内核心：逐项缺陷（我亲自验证）

### P0-A · `ActorRefExt::tell` 破坏每发送者 FIFO（实证）

```276:281:crates/parrot-api/src/address.rs
    fn tell<M: Message>(&self, msg: M) {
        let actor_ref = self.clone_boxed();
        tokio::spawn(async move {
            let _ = actor_ref.deliver(Box::new(msg) as BoxedMessage).await;
        });
    }
```

每次 `tell` 单独 `tokio::spawn` 一个任务，多线程运行时下任务执行顺序不保证。我写了探针（单发送者连发 20,000 条 `tell`）：**7,998 处逆序（40%）**；同样数据用 `deliver` 直投：0 逆序。Akka/Erlang/Orleans 都把"同一对 (sender, receiver) 之间 FIFO"作为基本契约。而 `test_correctness_suite.rs::c1_c2_c3` 的"C2 FIFO"用的是**顺序 await 的 ask**，天然有序，测不出这个问题。另外 `tokio::spawn` 在非 tokio 上下文直接 panic，且每条消息一次任务分配。

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

### 4.1 真实度地图（我复核了子代理的核心判断）

| 模块 | 状态 |
|---|---|
| Wire 帧 / TLV 握手 / TCP / QUIC / mem transport / ingress 分发 / hub 中继 / admin 远程 spawn | **真实运行** |
| SWIM（swim.rs 897 行） | 合并器正确；`Swim::run` **无人调用**，`tick()` 从不推进，`indirect_probes/gossip_fanout` 从未读取，probe 是全表顺序扫描不是随机单目标 |
| Raft（raft/mod.rs 888 行） | 进程内教学级：**零持久化**、墙钟、无 pre-vote/check-quorum/snapshot/成员变更；`RaftRpc` 只在内存 `TestNet` 流转，从未上网 |
| Sharding / Singleton / Durable / Directory / Topology / Cache / ACL | 纯状态机，**未接入 facade 路由** |
| tls.rs mTLS | **死代码**（TcpTransport 不调用） |
| 重连 / 退避 | 不存在（`transport_seed_table` 是空函数） |

### 4.2 P0 缺陷（均已源码复核）

1. **远端一包 panic**：`frame.rs:497` 先做 `body_len - 28 - path_len - key_len` 再在 L498 检查；`copy_to_bytes(path_len)` 在 remaining 不足时 panic。panic 发生在连接任务里，`on_disconnect` 不执行 → 挂起的无超时 ask 永久泄漏。
2. **TELL 静默丢失**：`seq_counters` 挂在每个 `RemoteInner`（`ref_.rs:33`，每次 `remote_ref()`/`get_actor` 都 `Default` 新建），接收端按 `from_node` 重排，第二个 ref 的 seq=1 被判"迟到帧"丢弃。子代理 PoC：10 条投递 1 条。这是最常见用法（多次 `get_actor` 后 `deliver`），**没有任何测试覆盖**。
3. **零鉴权管理面 = RCE**：`admin.rs:295-299` `admin_allowed()` 恒 `true`；任意 TCP 对端发 TLV 握手 + `SYSTEM_EVENT` 即可 `SpawnLocal`/`AdminStop`/`DeployComponent`（含 dylib/wasm 本地绝对路径）。三个语言网关（erl `code:load_file`、jvm `URLClassLoader`、py `importlib`）同样无认证——在任何非 loopback 部署下是全栈远程代码执行。
4. **Raft 偶数节点提交违反多数派**：`raft/mod.rs:543` 取中位数 `indexes[len/2]`，4 节点时 2 副本即提交；应为 `indexes[len - quorum]`。加上零持久化，重启后同任期可重复投票，选举安全性不成立。
5. **QUIC 默认 `dev_crypto_insecure()`**（system.rs:244）：客户端跳过所有证书校验、SNI 硬编码 localhost。

### 4.3 P1 缺陷

- 读/写/心跳同一个 `select!` 任务（transport.rs:328-429）+ 全局 1024 入站队列跨连接共享 → 慢消费者背压→心跳饿死→对端 10s 判半开→**背压演化为级联断连**。
- 入站 ASK 每条 `tokio::spawn` 且 `local_ref.send()` 无超时（ingress.rs:230-236, 336）→ 对端可无限制制造任务。
- hub `RelayTable` 只在 `take` 时惰性过期，REPLY 永不到达的条目永久泄漏。
- 入站 gossip 无条件合并进成员表（system.rs:927-936），任何连接可标任意节点 Dead。
- `tls.rs::extract_cn` 扫到的是 **Issuer** CN 而非 Subject CN（自签测试 issuer==subject 所以绿）。
- `HopExceeded` 定义了但全仓库从不产生；中继只 `hop_count+1` 不检查上限。
- follower 伪造 `match_index` → leader `self.log[ni-2]` 越界 panic。

### 4.4 测试深度

209 单测 + 60 集成 + 38 node 测试，帧/TLV/合并器/哈希环的纯函数测试质量不错。但：`swim_convergence_kill9`、`swim_partition_heal` 实际是手工 `mark_suspect` + `set_clock`，与网络无关；Raft `TestNet` 无随机种子、无丢包/重复/延迟/崩溃重启注入；无 TCP RST / kill 进程 / 恶意帧测试；全仓库 **0 处** proptest / loom / turmoil / madsim / cargo-fuzz。

---

## 五、应用体系 / 多语言 / 工具链

### 5.1 parrot-abi（dylib 热卸载）——问题最严重的 crate

- **P0 TOCTOU → UAF**：`loader.rs:232`（检查 `quarantined`）与 L251（in-flight +1）不在同一临界区；`unload` 在 L279 置位、L284 看到 busy==0、L335 `dlclose`，线程 A 随后读已释放 vtable 并跳进已卸载代码页。"drain 栅栏"在现有实现下不是栅栏。
- **P0 cdylib 自带 std TLS 析构**：合规 fixture 的 `nm` 显示 `__tlv_atexit` + 多个 `3std...$tlv$init`；`loader.rs:393-395` 的禁止清单扫描**刻意豁免了 `3std`** 符号。dlclose 后线程退出必 UB。业界（abi_stable / Bevy dynamic plugins / glibc 对带 TLS 库的策略）已基本放弃"运行时 dlclose Rust cdylib"。
- P1：禁止清单扫描只匹配 Mach-O 格式，Linux 下 GNU nm 输出永不命中且静默；宿主侧 `catch_unwind` 不能跨另一份 std 的 unwind（≥1.81 直接 abort），"第二道防线"是错误认知；`from_utf8_unchecked` 信任 dylib 传入数据。

### 5.2 parrot-wasm / parrot-app

- `memory_limit_mb` 全仓库无消费者（没接 `ResourceLimiter`）；`tick_epoch()` 只在测试里调用，生产无 epoch 驱动线程→抢占形同虚设；WIT 没有 `send/ask/spawn` 宿主函数，Wasm 组件只能被动应答。
- Wasm/Dylib 制品在本地装配器（assemble.rs:240-245）与远端执行器（parrot-node lib.rs:457-465）**两条路径都被拒绝**——`crawler.app.toml` 里的 Dylib 组件根本部署不了；`parrot-abi`/`parrot-wasm` 与编排层之间零接线。
- `component.config` 恒为 `None`（supervisor.rs:342、host.rs:305）；`stop_gateway` 停一个组件会 `kill_all()` 三个网关（host.rs:336-342）；示例 manifest 的 `[config_overlay]` 过不了自己的 `apply_overlay`。
- 两个示例 app 的 `main.rs` **零 parrot-app 依赖**，自己手拼 `ComponentDeploy`，`*.app.toml` 只被一个 dev-dep 测试读取。按 09 文档自己的标准（"迁移完成前应用体系不算交付"）未交付。

### 5.3 多语言互操作

- 帧布局五语言一致、4 条 golden 向量都能过——这是真的。
- 但：Python/Erlang/TS 解码器**无最大帧长**（Rust/JVM/cpp 16 MiB），恶意 `body_len` 可耗尽内存；golden 向量只有 4+6 条（无 HANDSHAKE/成功 REPLY/SYSTEM_EVENT/大载荷/多跳），Erlang 和 cpp 是把 hex **硬拷进源码**而非读共享文件；JVM 测试兜底写死 `/Users/biluochun/...`。
- admin-v2 管理面用 Rust `bincode` 派生顺序做五语言契约（`admin_v2.rs:38-43` 自己注释"不可加 skip_serializing_if 否则四方言错位崩溃"）——业界一律用 protobuf/JSON；仓库里已有 `pb.rs` 却没用在这里。
- 热加载：Erlang 用 `purge+load_file` 丢掉了 `code_change/3` 与 `soft_purge`；Python `import_module` 无 `reload`，重复 deploy 不换代码；JVM child-first loader 不等在途消息。RolloutTracker 的 Verify/Switch 在网关侧没有执行面。

### 5.4 federation-lab "数字孪生" 与 SCALE_REPORT

`twin.rs` 的 `verify()` 只是：生成地址字符串 → 解析回来 → 再格式化一次 → 比对相同 → `HashRing::node()` 非 None。**不触碰 Directory / 路由表 / SWIM / border 的任何真实代码**，是字符串格式化的自反测试，"百万地址 100% 正确"必然成立。`SCALE_REPORT.md` 12 行 ✅ 没有一行来自真实多节点运行（收敛是 xorshift 扩散模拟、带宽/内存是算术）。应如实标为"模型"。

### 5.5 构建 / CI / 仓库卫生

- **`Cargo.lock` 被 `.gitignore` 忽略**但 `deploy/Dockerfile:12` 要 `COPY Cargo.lock` → 干净 clone 下 docker build 必失败，发布不可复现；无 `rust-toolchain.toml`。
- `.gitignore` 含 `.d*` `.c*` 等通配，会吞掉 `.cargo/config.toml`、`.dockerignore`。
- CI 仅 macOS self-hosted；不跑 `make test-polyglot`、不跑 Docker、不跑覆盖率、无 Linux job；`parrot-node` 的 `wasm` feature 默认关，`wasm_actor_tests.rs` 在 `cargo test --workspace` 下不编译。
- 覆盖率：`docs/coverage/dev09.txt` 的 Branches 列全是 `0 0 -`——**从未测过分支覆盖**，而 waiver 开头写"分支 ≥95%"。
- 二进制入库：6 个 jar、`.beam`、`.wasm`，根目录 23 MB `gw-jvm-libs.tar`、256 KB `tarpaulin-report.html`。
- `parrot-obs/tests/e2e.sh` 写死开发者本机路径，`pkill -f 'ray_gw'` 会误杀同机进程。

### 5.6 性能报告可信度

可取：标注硬件/日期/profile、有 warmup、统一分位算法、记录测量修正、`bench/*/run.sh` 一键可跑——比大多数项目诚实。缺陷：CPU 场景对齐的是**墙钟而非计算量**（Erlang 迭代数缩放 ~2200×、Python 86×），所以"CPU 并行 64×"比较的是不同任务；单次运行无方差；Ray 用降级规模却并排给出 "1680×"；对比对象 Akka 2.6.20（2022）。"消息路径快 2–3 个数量级"大体可信（架构差异），具体倍数不应被引用。

---

## 六、文档评估

**优点**：31 篇、8.7K 行，mermaid 图齐全（架构/依赖/状态机/时序/ER），ADR 式决策记录含失败实验，`TECH_DESIGN_03` 的设计债台账（D1–D15）和功能缺口台账（F1–F19）态度罕见地诚实。

**问题**：
1. **版本漂移**：README 说 15,276 行 / 632 测试，01 说 474，03 说 339，实际 ~90K 行 / 1194 测试；03 的 D2 标"未变"而 derive 早已改为别名注入。
2. **完成度叙述系统性超前**：09 §12 "wasm/dylib DeployComponent 全链 ✅" vs 两路径均拒绝；07 §3.1 "realm = mTLS 域" vs 三网关明文无认证；06 "SWIM ≤3.5s 收敛" vs `Swim::run` 无调用方；05 "退避重连 1/2/4/8s" vs 不存在。
3. **E 系列基线（世界级/电信级/百万级）全部是"宣言"**，对应的 G1–G7 门禁没有一项进 CI。

建议在每个设计文档加一列**"状态：已接线 / 库级可用 / 仅设计"**，这一列比任何新功能都重要。

---

## 七、与业界标杆的差距矩阵

| 维度 | Parrot | 标杆 |
|---|---|---|
| 消息顺序 | `tell` 经 `tokio::spawn`，FIFO 40% 违约 | Akka/Erlang：pairwise FIFO 是基本契约 |
| 执行模型 | 每批次 spawn_blocking+block_on | Ractor/Kameo/Actix：任务级协作调度，CPU 密集由用户显式隔离 |
| 监督 | restart 换 mailbox、ref 失效、预算双计数 | Akka：ref 稳定、mailbox 保留、OneForAll/AllForOne 真执行 |
| 失败检测 | 固定心跳；SWIM 未运行 | memberlist：随机 probe + ping-req + Lifeguard；Akka：Phi Accrual |
| 共识 | 内存 Raft、偶数节点不安全 | openraft/tikv-raft：Storage trait、pre-vote、joint consensus、snapshot |
| 分片 | 哈希环，无 handoff，单活不保证 | Akka Sharding 集中协调 + remember-entities；Orleans 分布式目录 |
| 传输 | 单任务读写心跳、全局队列 | Artery 控制/数据流分离；Erlang dist 独立 tick |
| 安全 | 零鉴权、TLS 死代码 | Erlang cookie+TLS；Akka mTLS+角色；Dapr Sentry |
| 插件 | dlclose Rust cdylib | abi_stable / 永不卸载 / 进程隔离 |
| 跨语言控制面 | bincode 派生顺序 | protobuf/gRPC（Dapr/Akka gRPC） |
| 测试 | 脚本化状态机 | Jepsen、turmoil/madsim、loom、proptest、cargo-fuzz |

---

## 八、建议路线（按优先级）

**立即（安全/正确性，1–2 周）**
1. `admin_allowed` 真实判定 + 握手预共享 token + `cluster/realm` 比对；QUIC 去掉默认 insecure；三语言网关解码器统一 16 MiB 上限。
2. `frame.rs` 先校验 `body_len ≥ 28` 与 `28+path_len+key_len ≤ body_len` 再切片；panic 必须触发 `on_disconnect`（drop guard）；接 cargo-fuzz。
3. `seq_counters` 上提到 `RemoteActorSystem` 按目标节点全局；补"多次 `get_actor` 后 `deliver`"回归。
4. `ActorRefExt::tell` 改为同步 `try_send`/有界队列入队，不再 `tokio::spawn`；把 FIFO 测试改成真·并发 tell。
5. `parrot-abi`：隔离检查与 in-flight 自增放进同一锁；对 dlclose 做诚实决策（推荐"只加载不卸载，升级=起新进程"）。
6. Raft：`indexes[len - quorum]`；`match_index` 做上界校验；明确标注"实验性、未持久化、未上网"，或直接换 openraft。

**短期（1 个月）**
7. 修正 ADR-11：撤回全局 `spawn_blocking`，CPU 密集 actor 走 `DedicatedThread`/独立 runtime。
8. 监督：restart 复用 mailbox 与 ref（引入 cell/indirection 层）；修双计数；OneForAll 真执行；补精确断言。
9. actix context：实现 `stop/watch/spawner`，`schedule_periodic` 改为后台任务+取消句柄。
10. unsafe 整改：`EngineContextHandle<'a>`；actix 异步路径用 `Rc<RefCell>`/`Pin<Box<ActorCell>>` 等不依赖别名 UB 的方案，并把 miri 跑到这两处；删除 `single_alloc.rs` 或接线。
11. `Cargo.lock` 入库、`rust-toolchain.toml`、CI 加 Linux job + `test-polyglot` + llvm-cov 分支覆盖；清理 11 条 clippy；移除入库二进制与 `gw-jvm-libs.tar`。

**中期（1 个季度）**
12. API 收敛：一套 Actor trait、一套 Ref（`Arc` 基）、一套克隆机制、一个 `ActorFactory`/`SystemError`；`WeakActorTarget` 要么真 Weak 要么改名。
13. 要么把 `Swim::run` 真接线（随机单目标 + ping-req + 用配置 + 推进时钟 + refute），要么文档改为"静态种子 + 心跳半开检测"。
14. 传输层拆读/写/心跳任务、per-连接入站队列、重连退避 + quarantine、ASK `Semaphore` 限流 + 超时、`RelayTable` 周期清扫。
15. 引入 turmoil/madsim 做确定性网络故障注入；proptest 覆盖帧/TLV/合并器；loom 覆盖 mailbox 调度槽状态机。
16. admin-v2 改 protobuf（已有 `pb.rs`）；向量扩充并让五语言从 `docs/vectors` 共享读取。
17. 文档：所有"✅"按"已接线/库级/仅设计"三态重标；`SCALE_REPORT` 改标"模型"；E 系列基线从"验收"降为"愿景"直到对应门禁进 CI。

---

## 九、最后的评价

这个项目最大的资产不是代码，而是**它的工程叙事能力**——ADR、量化实验、失败记录、设计债台账，这些是世界级团队才有的习惯。最大的风险也在同一处：叙事的速度远远超过了实现的速度，7 天 75K 行的产出让"形状正确"大面积取代了"接线正确"，并且测试套件在用宽松断言和自反验证为这种错位背书。

如果把文档中所有未接线的能力如实降级，Parrot 是一个**有潜力的进程内多引擎 actor 框架 + 可用的多语言 Wire 协议 POC**，这个定位本身已经有价值。要走向"世界级"，接下来一个季度该做的不是再加能力，而是：修 5 个 P0、砍掉死代码、把 CI 门禁做实、让每一个 ✅ 都能被一条端到端测试证伪。