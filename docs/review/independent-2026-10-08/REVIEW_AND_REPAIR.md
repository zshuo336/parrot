# Parrot 独立核查与修复记录

基线：6573d71ecff1e7559962b4f5ccf950293abf213f；2026-10-08；macOS arm64，rustc/cargo 1.98.1。

## 结论与证据边界

两份既有评审属于同一评审的短版和归档扩展版，不是独立的两次复核。对运行时接线、语义边界、测试判据和发布工程的主体批评成立；其中若干 P0 的可达性和全称断言不成立或证据不足。

Parrot 已有可运行的双引擎、本地/远程消息路径、多语言网关与实际应用，不能称为纯文档工程。但统一 API 目前掩盖了不同执行路径的生命周期、排序、背压和故障恢复差异。当前最重要的是统一可验证的运行时契约，并落实安全边界、完整装配和恢复路径。不能用代码量、测试数量或主观评分推断生产成熟度。

本次核查覆盖 15 个 workspace 成员及 interop、apps、docs、examples、bench、deploy、tools、scripts、poc 的结构、调用关系、构建与测试入口；关键消息/生命周期/协议/部署路径深读和探针验证。不是对所有 467 个已跟踪文件逐行完成形式化证明。未执行公网爬取、百万节点实测、完整 Linux 容器部署或所有 release 性能场景；这些不算通过。

基线实跑：
- `cargo test --workspace --no-fail-fast`：115 个 suite，1194 passed / 0 failed / 34 ignored（沙箱外完整重跑；首次沙箱内端口失败已排除）。
- `make test-polyglot`：成功；TS 16、Python 40、JVM 34，Erlang wire/admin 与 C++ vectors/C ABI 通过。
- `cargo fmt --all --check`：失败，49 个差异块。
- `cargo clippy --workspace --all-targets -- -D warnings`：失败，首先报告 crawler dylib 的手写 C 字符串。
- 独立 12 个 Rust 探针全部确认预期的缺陷现象；另有 TS 重复握手与协议码点探针。这里“探针通过”指确认旧实现有问题，不代表产品正确。源和日志随本目录保留；修复必须另加断言正确行为的正式回归测试。

## 原评审需要纠正的论断

1. dylib TOCTOU：`handle_msg(&DylibHandle)` 借用与 `unload(DylibHandle)` 消费所有权，句柄不 Clone。原报告未展示遵守 Rust 所有权的并发调用方，不能凭两个原子操作间隙认定可达 UAF。ABI 安全边界、恶意/不合规插件、TLS、drop 生命周期仍需审计。
2. dylib TLS：存在 TLS 符号不等于必然注册析构且平台必然 unmap；“退出必 UB”和“工程上不可能安全卸载”超出证据。必须按平台、实际注册、句柄/回调存活分析。
3. 无重连：Rust `remote_ref` 有 learned address 按需重拨；TS 有指数退避。缺少的是可靠、统一、受控的恢复机制，且旧引用持链路快照。
4. Wasm 零接线：`WasmActor` 已接入 thread 引擎并有 feature-gated 测试；manifest/admin 执行器尚拒绝该制品。不能混为一谈。
5. “全仓库无混沌脚本”：存在 `chaos_scenarios.rs`，但主要是直接驱动状态机，不等于 kill 进程或网络故障注入；应批评验证层级。
6. `spawn_blocking` 问题的主要门槛不是 512：shared worker 在等待整批消息完成时占槽；1 worker 的 A→B ask 也能阻塞；同 shard 同样如此。用 block_in_place 替换并不自动解决调度槽占用。
7. API 中 typed/erased 双面本身可以合理；不应一律合并为一个 trait。真正必须统一的是契约、所有权、桥接和能力声明。
8. 性能：按墙钟校准 CPU 工作量可以测试调度隔离，但不能据此宣称相同计算吞吐的语言倍数。JVM 源码已写明 3 轮 echo warmup。静态轨和动态轨执行机制/生命周期不同，10.2x 不能全部归因于一次 Box。
9. P0 分级须包含可达配置、影响范围、触发条件；未上线 Raft 的错误与默认网络攻击面不应无区别排列。1–2 版本周期、季度即可世界级、完全没有领先方面均不是经验证据。
10. 工程卫生不能只看编译和测试数量；format/锁文件/新 clone/feature 矩阵也是必要证据。tarpaulin-report.html 实际在 git 跟踪中，原报告“未入库”不对。

## 已确认问题与修复验收清单

状态含义：待修复 / 部分修复 / 已修复待验证 / 已验证。未实现能力必须落实实现，不能仅改名称视作修复完成。

### A 消息、协议和存储正确性
- A01 已验证（首批修复）：Frame 先校验最小体长与 path/key 边界，按单帧 split 解析；补充坏长度、拼接尾帧、hop 耗尽和半包回归测试。仍需跨语言 fuzz 矩阵。
- A02 部分修复：系统共享序号，队列预留后才分配序号（取消不制造空洞）；旧 ref 动态读实时路由，旧连接关闭只移除自己的 sender。已验证连续新 ref、旧 ref 重连和 hub 两源投递。Wire 1.0 缺原始发送方/会话标识，中转采用 bypass 避免把多源当重复包；跨 hub/直连切换的 FIFO 和发送方重启序列仍待协议级修复。
- A03 已验证（首批修复）：PendingCallback RAII 覆盖 future 丢弃，timeout 包含排队与等待回复全过程；保留连接故障/回调容量压力测试。
- A04 部分修复：Rust/TS 拒绝 hop 耗尽；relay 出站检查 hop，先预留队列→登记映射→无 await 发布；映射容量 65536，65s TTL 每秒清理，按真实目标断连清理。已验证容量/过期/目标隔离及真实 hub 往返。回程队列满的可靠错误反馈和关闭时在途任务回收仍待完善。
- A05 部分修复：严格多数（2/4/6 节点回归），拒绝未知/伪装来源、旧任期响应和越界索引；AppendEntries 全批预检且 ACK/commit 仅覆盖本次匹配前缀，保护已提交项；生产时钟改为 Instant。持久化、恢复、网络接线和读一致性仍未完成。
- A06 部分修复：ACK 水位取 max、恢复过滤已确认记录、跨段同步/封段损坏处理和乱序去重已补实现与测试；仍需故障注入及多进程恢复验证。
- A07 部分修复：TS SYSTEM_EVENT 已统一为 0x20，TS 解码器增加最大帧、最小体长、变量长度和 hop 边界；其它语言解码器仍需同一恶意帧矩阵。
- A08 已验证：注册表按 (key,path) 去重，握手 ACK 重放不会自增长；新增回归测试覆盖重连注册。
- A09 待修复：ActorRefExt::tell spawn-per-message 不保证顺序（20k 探针数千逆序），吞错且要求 Tokio 上下文。

### B 运行时与内存安全
- B01 已验证（正确性回归）：shared 改为每次 poll 占用阻塞线程与执行许可，Pending 释放执行容量；sharded 在所属线程复用多个 actor future。新增 1 worker / 2 blocking threads / 8 actor 调用链、初始化等待、FIFO 和 CPU 并发上限回归。四个原始正确性回归在旧实现全部失败、新实现通过；当前吞吐与复杂负载仍需测量。
- B02 部分修复（第九批）：两个后端的静态 typed actor 已纳入各自 registry、原子同名检查、停止/关闭、生命周期与取消；两组各五个旧版失败回归已修复。Actix 扩展专项覆盖系统隔离、owner 亲和性和外部线程准入；typed 自动重启与完整调度/drain 整合仍待完成。原始探针确认同名重复、actor_count=0、shutdown 后仍回复。
- B03 已修复待最终整体验证：EngineContextHandle 显式借用上下文，安全构造和 downcast 受生命周期约束；Actix async 由 future 拥有 actor/context，移除 transmute 与手写自引用投影。三个编译失败测试与真实 arbiter 取消析构顺序回归通过，旧版同一回归失败。外部 unsafe from_raw 仍由调用方承担契约。
- B04 部分修复（第七批）：supervision restart 预算单计、原引用跨代保留、完整配置继承、旧代回调隔离、工厂与停止竞争已补齐并测试；完整父子与 Resume 语义仍待实现。原始发现包括预算双计、旧 ref/mailbox 失效、配置丢失、模式/子树语义不完整。
- B05 部分修复：Context::send 的非应答语义、thread Context::ask 的跨引擎信封和超时、Actix 超时错误分类已修复并有真实回归；第九批补齐 Actix 托管 actor 初始化、停止钩子、Context 自停和路径；timer/watch/stream/spawn 与父子管理仍未完整执行。
- B06 部分修复：动态 actor 系统关闭使用配置 deadline，隔离并发停止钩子，通知 shared（含 burst）/dedicated/sharded 取消异步任务；分片和独占线程初始化/批处理失败通知监督。ActorRef::is_alive 检查 mailbox closed。第四批已补齐单 actor stop deadline、取消和并发 shutdown 完成确认、ActorRef/Context 的停止入口和专用线程退休配额。第六批已补齐 dedicated/sharded 停止钩子亲和性，第七批补齐监督重启代际隔离；typed 生命周期、用户 future/actor 析构隔离和父子生命周期仍待闭环。
- B07 待修复：Wasm 内存 limiter / epoch 生产驱动 / WIT 主动通信能力未接齐。
- B08 待核查修复：dylib 加载信任、FFI 参数寿命、析构/显式卸载、平台扫描 fail-open 与实际 TLS 注册边界。

### C 应用、配置和可观测性
- C01 部分修复：组件在 start hook 前登记到逆序回滚栈，hook 失败会停止当前组件；hook_path 仍未真正路由到指定 actor，stop/drain 与回执契约待补齐。
- C02 待修复：assemble 克隆并 overlay cfg 后未交给执行器；component.config 在 host/supervisor 被丢弃；CLI 建默认引擎忽略文件参数。
- C03 待修复：stop 一个组件 kill 全网关；多实例部分成功失败的回滚、所有权与组件命名隔离不完整。
- C04 待修复：manifest wiring / placement / upgrade 声明与执行面缺口；Wasm/Dylib admin 部署，应用迁移与 reconcile/rollback 执行闭环。
- C05 待修复：MirrorRef 仅输出 skipped 日志，没有镜像投递；record/replay 与运行时接线、非法 hex 处理需补齐。
- C06 待修复：观测指标部分硬编码，需明确 unavailable 与真实计数，提供 trace 传播和实际 exporter。
- C07 部分修复：expand_env 改为扫描原始输入一次，变量值按字面量插入；空值/缺失值使用 `:-default`。默认值以外的未知键、数值边界、配置生效验证仍待完成。

### D 安全、分布式和互操作
- D01 待修复：admin 与 STOP/成员/route hint 控制面身份与授权缺失；默认监听公网，QUIC dev skip verify，mTLS 未接线。
- D02 待修复：transport 读写/heartbeat 阻塞互相影响；全局入站 HOL；入站 ASK 并发和回复清理缺资源界限。
- D03 待修复：SWIM、Directory/Raft、Sharding/Singleton、durable、ACL、MQTT/LiveKit 实际组装和故障恢复与文档不一致。
- D04 待修复：跨语言消息模式演化/golden vectors/大帧/错误/重启兼容性；不能仅凭 4 条向量宣布全协议一致。
- D05 待修复：RemoteMessage 缺省键不含模块路径；同 crate 同名类型冲突；pb 前缀仍生成 bincode 编码的语义不一致。

### E 示例、构建、测试和交付
- E01 部分修复：根 Cargo.lock 已保留为交付文件；移除隐藏 .cargo/.dockerignore 的宽泛忽略；fmt 和 workspace/all-targets Clippy 均通过。工具链 pin、feature/Linux/多语言/制品 CI 仍待建设。
- E02 已修复待容器验证：Dockerfile COPY apps、cargo build --locked、healthcheck 显式 PARROT_HEALTHCHECK=1，增加 .dockerignore 和根锁文件。本机 Docker daemon 未运行，未完成镜像 build/run 验收。
- E03 部分修复：MODE=release 传 --release；lab/regression 脚本路径及 run-lab 自身 cd 根目录路径已修正；明确选择 debug/release 二进制避免误用旧产物；缺工具链直接失败。dry-run、bash -n 和缺 npm 返回非零已验证；完整 crawler-lab 两种组网场景仍待跑。
- E04 待修复：README 示例属性/类型/调用漂移；文档、测试名、coverage waiver 和实现证据不一致。
- E05 待修复：DigitalTwin 的 n0/n256 映射相同，endpoint 还是三段 IPv4 文本；verify 自反不能证明双射或目录路由；需真实模型/运行时对照。
- E06 待修复：websearch 在 resp.bytes().await 后截 1MiB，未限制下载内存；robots cache 全局锁跨网络等待、origin 处理、爬取/索引恢复契约需审计。
- E07 待修复：测试断言仅范围/计数/静态 golden/跳过，遗漏跨生命周期组合；需要 deterministic faults、边界/property、取消/关闭/重启、对抗解码与内存模型验证。
- E08 待核查修复：许可文件/依赖和制品来源、供应链扫描、发行契约、性能基线重复次数与公平性。

## 执行顺序

1. 协议/消息/存储/回滚的确定性缺陷与可证伪测试。
2. 运行时调度、生命周期、监督、内存安全。
3. 默认安全、认证/传输、资源上限、恢复与持久性。
4. 应用编排/多引擎能力真实闭环、示例迁移与观测。
5. 全 feature、多语言、Linux/容器、故障/性能/兼容矩阵；文档与发布审计。

任何阶段发现的新问题并入台账。最终完成需逐项验收，不以此台账的存在或测试数量代替证据。

## 首批实现与验证记录（2026-10-08—2026-10-09）

这是第一批修复，不表示整个目标完成；B 类运行时和 D 类默认安全等关键工作仍未落实。

### 新发现并修复

- hub 先查本地同名 actor 会错投其他节点的 ASK/TELL/STOP；目标节点判定现在在本地查找/业务解码之前，中转可转发不认识的业务 payload。
- hub 多个来源带相同 seq，按 hub 身份重排会误丢消息；现在中转 bypass，完整端到端身份仍待协议升级。
- 旧连接 close watcher 曾移除该 node 全部链路；现在按具体 channel 移除并保留替换连接。
- WAL 去重曾越过缺口推进水位；现在只推进连续已收序号。跨段同步、GC 全水位 checkpoint、封段损坏不静默截断、乱序 ACK 取 max、恢复过滤已确认项已修复。
- Raft heartbeat 曾把未验证的本地旧 suffix 当作复制成功并提交；现在只确认本次匹配范围。
- hook 错误测试原先实际注入 spawn 错误；现注入真实 hook 错误，断言当前组件 ref 和路径均停止。timeout 测试也不再只看空 started 栈。
- durable 集成测试现经真实 mem transport 投递，读取精确业务处理序列，断言重复只处理一次，再验证 ACK 后 reopen 为空。
- ACK 性能测试原先复用旧 seq，裁掉最慢 10% 却报告 p99；现用递增合法 seq、完整样本分位数，作为显式 `--release --ignored --nocapture` 磁盘实验，取消未经控制环境验证的通用 1ms 阈值。默认 ignored 因此增加 1 项，并非隐藏正确性失败。

### 实跑结果

- `cargo test --workspace --no-fail-fast`：116 suites，**1208 passed / 0 failed / 35 ignored**；沙箱外执行，含真实 TCP/QUIC。日志 `/tmp/parrot-independent-tests-unrestricted.log`。
- `cargo test -p parrot-remote --lib --test remote_lifecycle_regressions`：**220 单元 + 4 链路回归通过**。日志 `/tmp/parrot-repair-relay.log`。
- `cargo test -p parrot-app --lib`：**128 通过**；额外开启 host 的 app_run_scenarios/test_assemble_helper：**8 + 7 通过**。日志 `/tmp/parrot-repair-app.log`、`/tmp/parrot-repair-app-host.log`。
- `make test-polyglot`：TS **17 pass / 1 lab skip**、Python **40**、JVM **34**，Erlang wire/admin、C++ vectors/C ABI 通过。日志 `/tmp/parrot-independent-polyglot-unrestricted.log`。lab skip 不等于 crawler 全链通过。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` 均通过。Clippy 日志 `/tmp/parrot-repair-clippy.log`。
- shell 语法和 Make release/lab dry-run 通过；故意移除 npm 的 PATH 后 check-ts 正确返回非零。
- Docker daemon 未运行；未执行 Linux 镜像 build/run、完整 feature 矩阵、全部压力测试。原始探针仅留作旧版缺陷证据，正确行为断言位于正式测试目录。

### 下一阶段必须继续

1. shared/sharded handler 占槽死锁、typed registry/shutdown、Actix transmute/context 生命周期、监督旧引用与预算。
2. TLS/admin 授权与监听默认值；ASK/relay 任务取消、资源上限、连接身份和回复背压。旧连接的断连回调仍可能按 node 批量 fail 新连接 pending asks，须按连接/代际补齐。
3. WAL 单写者互斥、写入中途失败后的状态修复、崩溃故障注入、durable 自动驱动；Raft durable/network/read-index 闭环。
4. hook 目标/回执/stop/drain、config 生效链、多实例失败清理、Wasm/Dylib admin 执行、部署/升级/回滚控制器。
5. 其它语言恶意帧矩阵、环境变量/replay/抓取内存边界、真实数字孪生映射、示例文档校准、Linux/feature/发布 CI。


## 第二批：运行时进度与关闭（2026-10-09）

首批修复已由工作区的新提交 `31f7b2c` 保存；本批基于该提交继续，未回退其中内容。

### 实现

- Shared worker 对多个在途 mailbox future 进行复用，每个 worker 的 semaphore 限制同时执行的 poll 数为 1；poll 在 blocking pool 中执行，Pending 后释放线程和许可。dequeue 前预留许可，避免隐藏积压、破坏 priority/burst 调度。初始化也受同样隔离。
- Sharded 使用 current-thread runtime 的异步收件与 FuturesUnordered，同 shard 的非循环 actor ask 可以推进；future 的全部 poll 留在所属 OS 线程。队列长度改为实际 channel 长度，deschedule 令注册 token 失效，Drop 通知线程退出。
- ScheduleState 的 ownership/wake 改为同一个原子状态，通过 CAS 决定由 producer 或 worker 入队，消除两个独立 bool 间重复抢占的窗口。增加 100000 次并发 wake/release 验证；该压力测试不是形式化证明。initial enqueue 也检查获取所有权是否成功。
- burst worker 的 idle 回收保留所有等待中的 actor；core 与 burst 的任务句柄统一归池所有，shutdown 超时明确 abort 异步任务。同步 poll 无法被安全抢占，返回后释放其资源。
- 动态 actor 的注册和调度在同一同步临界区完成，调度失败撤回注册，避免先执行用户初始化后发现重复路径，以及初始化失败回调先于 registry 的竞态。
- 修复正常 stop 和 panic stop 中构造但未 await 的 shared deschedule；补上 shard deschedule。
- ThreadActorSystem shutdown 使用配置的统一 deadline：关闭邮箱、并发隔离执行 before_stop、通知所有调度器回收；异步 handler/停止钩子不会使关闭无限等待。停止钩子错误向调用方报告。`is_alive` 不再把关闭但仍被在途任务引用的邮箱算作存活。
- dedicated 的线程回收不再通过阻塞池内无限 join；先发取消，在线程退出后 join，截止时间耗尽时报告错误。
- 新回滚测试发现 `max_dedicated_threads` 未传给调度工厂，已补齐配置接线。

### 验证与边界

- 修改前四个 acyclic ask/init 回归全部失败，日志 `/tmp/parrot-scheduler-before.log`；修改后全部通过，日志 `/tmp/parrot-scheduler-after.log`。
- 第一轮调度专项：152 单元 + 6 新调度回归 + 3 弹性 + 3 公平性 + 1 高并发唤醒通过；3 个 release 性能实验仍 ignored。此结果早于后续生命周期修复，不代替最终验证。
- 新增 `test_actor_registration.rs`、`test_async_scheduler_progress.rs`、`test_shutdown_deadline.rs`。关闭测试等待真实 future 析构与在途 ask 失败，不以仅返回时间或 registry 数量判断回收成功。
- 仍需继续 B02/B03/B04/B05、完整 C/D/E 类问题；不宣称整个项目修复完毕。特别是 typed actor 还未纳入统一注册/取消，单 actor stop 仍可能无限等待，监督重启的旧引用与预算问题未在本批解决，mailbox 满队列与 close 的竞态在第三批继续修复。


### 第二批最新验收状态

- 最终新增回归：`test_actor_registration` **3/3**、`test_async_scheduler_progress` **7/7**、`test_shutdown_deadline` **5/5** 通过，日志 `/tmp/parrot-runtime-final-regressions.log`。覆盖三模式同名并发创建、初始化 panic 清理、调度失败回滚、低容量 acyclic ask/init、FIFO、实际 poll 并发上限、异步取消、同步停止钩子隔离。
- `cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all --check`、`git diff --check` 通过；Clippy 日志 `/tmp/parrot-runtime-clippy.log`。
- 第一轮完整 workspace：118 suites，**1219 passed / 1 failed / 35 ignored**。唯一失败为 `m3_panic_restart_recovers_service`：新增重复调度保护暴露了 panic-stop 路径漏 await 的旧问题。修复后 `test_m3_supervision` **3/3** 通过，日志 `/tmp/parrot-runtime-supervision.log`。不能把这一轮完整测试写成全通过。
- 第二轮完整 workspace 结束，退出码 0：119 suites，**1227 passed / 0 failed / 34 ignored**，日志 `/tmp/parrot-runtime-workspace-final.log`。边界：编译完成后、测试仍运行时已开始第三批 Actix 修改，因此前面执行的二进制验证第二批运行时代码，末尾 doctest 读到第三批 API 文档。此结果不等于第三批最终源的一致性全量验收。


## 第三批：Actix 借用安全与上下文消息语义（2026-10-09，进行中）

- `EngineContextHandle<'ctx>` 持有真实 `&'ctx mut dyn Any`；新增安全 `new`，生产适配器使用此入口。外部 `unsafe from_raw` 保留但明确有效性、独占性及完整借用期契约。纠正三个测试从不可变数据伪造可变上下文的构造方式。
- 异步分发 future 拥有 actor/context，内部借用调用 receive，完成（包括业务错误）后恢复两者。取消时由编译器管理析构，移除 transmute、手写投影与自引用 future。新增 `try_inner_mut` 明确执行期间 actor 不在适配器槽内。
- 同一 arbiter 取消回归在旧 `31f7b2c` 适配器观察到 **actor → handler** 析构并失败，在修复后为 **handler → actor** 并通过。只替换旧适配器的隔离探针日志保存在 `actix-cancellation-before.txt`。证明原文档的寿命保证错误；没有声称探针直接观察到任意内存损坏。
- Actix 状态串行、错误后恢复、取消安全及既有 dispatch/derive/adapter 专项：**26/26** 通过；API/derive lib 与 tests：**273/273** 通过；API doctest：**20 passed / 4 ignored**，含禁止句柄逃逸、重叠访问、返回局部借用的三个 compile-fail 用例。
- 新发现：两个引擎 Context::send 误用等待回复接口；thread Context::ask 双重封装私有信封，超时在等待业务结果之后才开始；thread boxed/inline ask 未把等待邮箱空位纳入 deadline。六个真实 actor 回归在修复前全部失败，修复后全部通过；跨引擎双向 ctx.ask 正确回 42。typed 同类容量超时单元回归旧版失败；修复后 typed 16 个单元通过。Actix 初始化/停止/定时器等 B05 项仍独立待完成。


### 第三批追加：邮箱关闭、容量和错误契约

- Thread boxed/inline 与 typed ask 使用覆盖入队、等待回复的同一个 timeout。Actix ActorRef 使用外层 timeout，并向调用方返回 TimeoutDetail；ActixContext::ask 保留错误类型。Actix 满邮箱探针在旧版仍会按时返回，但返回 Other，因此只能作为错误分类缺陷的直接证据，不能声称该探针复现了 Actix 无限等入队。
- MPSC 原先 close 只是 drop(sender.clone())，等待发送在 drain 后仍返回成功。现在 enqueue/close 在同一个同步 gate 内提交，Block 用 Notify 等待，close 取出并 drop receiver，底层 sender 也断开。high/normal 共用 capacity 检查和发布边界，DropOldest 按跨 lane 到达顺序淘汰。
- Tokio SPSC close 真正关闭 receiver；Block reserve 后在 receiver 锁内提交且复查关闭；长度改读实际队列，消除 raw sender 漏计和发布后再增计数的竞态。
- Ring buffer producer 与 close 共用 producer 锁；Block 以空位通知驱动，移除未声明的约 500ms 重试截止；长度改读实际 ring 占用，移除发布/计数错序窗口。三个 mailbox 零容量统一为 1。
- 初始五个邮箱回归：旧版 **4 fail / 1 pass**，修复后通过。新增覆盖低层 sender 关闭、64 个并发普通/优先级生产者共享容量、ring 持续背压、DropOldest 跨 lane 到达顺序。最终 mailbox **9/9**、context **7/7**、调度 **7/7**、注册 **3/3**、shutdown **5/5** 均通过（共 31），日志 `/tmp/parrot-runtime-new-contracts.log`。修复前证据归档于 `context-mailbox-before.txt`。
- 低层 sender() 仍是绕过 scheduler hook 的显式 adapter 边界，MPSC raw sender 不参与跨 lane 容量策略，Tokio raw permit 保留原生通道的关闭语义；这些接口不应当作已具备 ActorRef 的完整契约。后续需做 API 收口与性能验证，不能据此声称 mailbox 形式化验证完成。


### 第三批验证状态

- Strict Clippy `cargo clippy --workspace --all-targets -- -D warnings` 已结束，退出码 0，日志 `/tmp/parrot-safety-clippy.log`。fmt 与 diff check 通过。
- 首次第三批 workspace 复验 `/tmp/parrot-third-batch-workspace.log` 在补充发现 watcher 背压传播问题后主动中止。已经通过的 suites 只算阶段证据，不算最终全量结果；追加通知 deadline 修复后重新完整复验。
- `parrot_envelope_legacy` 专项 **23/23** 通过，覆盖消息契约、邮箱契约、调度进度，日志 `/tmp/parrot-third-batch-legacy.log`。此结果早于后续 watcher deadline 修复。


### 第三批补充：控制消息的显式截止时间

移除 mailbox 内隐藏重试期限后，进一步检查调用方发现死亡通知原先按 watcher 顺序逐个等待，满邮箱会阻塞健康 watcher 并拖住 stop。新增真实 watcher 测试旧版失败（外层 500ms 超时）。系统现并行投递通知，共用 `shutdown_timeout` 截止时间；正常 stop 返回通知失败/超时，不再静默成功，监督路径明确记录失败。向父级升级故障的投递也有显式 deadline。此边界不代表控制消息实现了持久可靠投递：已停止的 actor 不会因为通知失败而恢复，调用方需区分停止事实和通知失败；控制消息独立通道、重启代际与 watch 重注册仍待完善。


第三批最终复验已重新启动：`/tmp/parrot-third-batch-workspace-final.log`。watcher deadline 专项 + M2/M3 共 **7/7** 通过；最终严格 Clippy、fmt、diff check 通过（`/tmp/parrot-third-batch-final-clippy.log`）。后续先核验全量进程结果，再开始下一批生产代码改动；未完成项继续保留。


## 第四批：单 actor 停止与关闭操作所有权（2026-10-09，进行中）

第三批全量测试仍在运行时，已编译快照用于持续发现兼容性问题；开始本批后，后续同路径测试二进制可能被专项重编译替换，末尾 doctest 也会读取新源码。因此 `/tmp/parrot-third-batch-workspace-final.log` 必须按阶段混合验证记录，不能据其文件名宣称第四批最终源码全量通过。完成本批后再启动源一致的整体验收。

- 新增六个真实停止回归：shared/sharded/dedicated 分别等待不返回的 handler 或 before_stop。旧版均由 600ms 外层兜底超时；修复后均在配置的 50ms deadline 返回 Timeout，并在不关闭系统的情况下释放等待 future，邻居 actor 仍回复 42。修复前证据 `individual-stop-before.txt`。
- ActorProcessor 增加执行取消通知；shared/sharded/dedicated 消费循环选择性取消该 actor 的 suspended future，不需要关闭整个调度器。
- 单 actor stop 先关闭入口和清理排队消息，隔离执行 before_stop，所有阶段共用配置截止时间；deadline 后发取消并 deschedule，死亡通知也使用同一个 deadline。同步用户代码不能安全强制抢占，其返回后才释放持有的 future。
- 单 actor stop 由系统 runtime 中的独立操作持有；多个调用方加入同一次操作，丢弃任意调用方不丢弃清理。注册表保留占位直到 deschedule/通知完成；移除时比较 mailbox 身份，避免移除替换实例。系统 shutdown 会加入已在运行的 stop，避免重复 before_stop。
- 系统 shutdown 同样由一次 owned task 驱动，所有调用方等同一完成结果，取消首个调用方不把系统永久卡在半关闭。重复调用保留之前的失败，不再在关闭尚未结束或曾超时时静默返回成功。相应旧测试的重复 shutdown 成功断言已改为检查仍返回 Timeout。
- 最新生命周期专项：新增 **11/11**（六种 deadline、并发 stop 与路径占位、取消 stop 调用方、system shutdown 加入 stop、并发 shutdown、取消 shutdown 调用方）通过；配套注册/系统 deadline/watcher 验证正在完成。单 actor 同步 hook 隔离、更多退出故障/重启组合和后续 strict Clippy 仍需最终核验。
- 本批尚不等于 B06 完成：ActorRef::stop/Context::stop 仍需统一路由且避免 self-stop 死锁；生命周期钩子的 dedicated/sharded 亲和性、panic 回调代际、独占线程超时但仍活跃时的容量计数、typed actor 注册和销毁仍未完成。


### 第四批追加：关闭时的消息析构隔离

- 两个 current-thread runtime 回归加入一个析构阻塞 250ms 的排队消息，系统配置 40ms stop deadline。旧行为约 296ms 才返回（`/tmp/parrot-destructor-before.log`），证明超时计时器被用户析构阻塞。
- Mailbox 新增同步 `seal`，仅关闭投递入口并唤醒等待生产者，不运行排队消息析构；`close` 在释放内部锁后清空消息。系统用独立拥有的隔离任务执行 close，超时不放弃清理，也不会在超时后新启动 before_stop。
- Flume 接收端 Drop 只断开通道，未析构仍由 sender 持有的队列；新 seal/close 回归最初准确捕获 drop count 仍为 0。现显式 drain 普通队列，析构由清理任务执行。raw sender 仍绕过高层入口，尚需 API 收口以封闭其与 drain/disconnect 间的竞态。
- 修复后 `test_individual_stop_deadline` **14/14**、`test_mailbox_close_contract` **12/12**、`test_shutdown_deadline` **5/5** 通过（`/tmp/parrot-drain-fixed-tests.log`），watcher 专项仍运行。不是整个 workspace 最终验收。
- 另一个新单元回归限定 Tokio 阻塞池为一个线程，排队的 poll 在取消后不得执行用户代码。旧实现 polls=1，修复后 polls=0，取消句柄同时确保队列任务的资源最终析构。证据 `/tmp/parrot-queued-poll-before.log` 与 `/tmp/parrot-queued-poll-after.log`。


### 第四批追加：公开停止入口与专用线程配额

- ThreadActorRef::stop 现在请求并等待系统拥有的同一停止操作；引用携带弱系统句柄，并在注册表锁内核对 mailbox 身份，旧代引用不能停止同路径的新实例。注册表、上下文和返回的引用改为共享同一个 mailbox 状态单元。
- ThreadContext::stop 同步封住本代 mailbox 并请求停止，不在当前 handler 内等待需要同一 actor/context 锁的 before_stop。处理批次在封口后不再取下一条消息。子 actor 的完整生命周期和跨引擎停止契约仍需继续完善，不能把本项当作整个父子树停机验收。
- 三种模式的外部 stop 和 self-stop 回归旧实现 **6 fail / 1 pass**（额外一项旧引用保护通过），入口修复后六项均通过。断言 before_stop 仅执行一次、registry 真正移除、自停止当前请求可返回、后续排队消息不处理，不只断言 is_alive。
- 专用线程停止超时后仍保留 worker 和 JoinHandle，继续计入 max_threads；真实退出后才 rejoin 和移除。shutdown 超时不再标记成已关闭，后续 shutdown/admission 可回收退出的线程。容量回归旧实现失败，修复后通过，公开入口和容量共 **8/8**。另补齐 dedicated admission 在 worker map 锁内复查 shutdown 标志，防止新线程越过关闭快照。
- 旧行为证据归档 `stop-entrypoints-before.txt`、`mailbox-destructor-before.txt`。截至环境变量修复开始前，workspace/all-targets strict Clippy 通过（`/tmp/parrot-fourth-batch-final-clippy.log`）。最终停止专项日志 `/tmp/parrot-stop-entrypoints-final.log` 已结束，**48/48** 通过。

## 第五批：配置展开终止性（2026-10-09，进行中）

- 环境变量不再递归重扫替换值；自引用、互引用保留为字面内容，一次展开工作量与输入及实际输出长度相关。`${VAR:-default}` 对缺失和空值均使用默认值；没有默认值时仍替换为空字符串，保留既有兼容语义。残缺表达式保持原样。
- 增加 Unicode、相邻占位符、空值/缺失值/非空值、字面美元符号、残缺表达式、自引用/互引用测试。原 e1 配置加载测试不再在多线程 harness 中修改进程全局环境，改用父进程给子进程设置 env，在真实 load_file 流程验证自引用不会卡死和空值默认值生效。
- 插值仍发生在 TOML 解析前，不支持嵌套默认表达式；插入内容必须符合所在位置的 TOML 语法，已明确写入配置设计文档。配置的数值与未知键验证、全链路参数接线仍未完成。
- `cargo test -p parrot-config`：**16 单元 + 1 doctest 通过**；其中真实环境加载回归内部另运行一个子进程，不重复计入测试总数。日志 `/tmp/parrot-config-expansion-final.log`。`cargo clippy -p parrot-config --all-targets -- -D warnings` 通过，日志 `/tmp/parrot-config-expansion-clippy.log`；fmt/diff check 通过。


### 最新回归结果补记

- `cargo test -p parrot --lib`：**153/153** 通过，日志 `/tmp/parrot-fourth-batch-lib-final.log`，覆盖队列、调度、邮箱、上下文、ActorRef、系统与取消排队 poll 等单元用例。
- 第四批最终源码专项已通过注册 **3**、调度进度 **7**、消息契约 **7**、单 actor deadline **14**、监督 **3**、system deadline **5**、公开 stop/配额 **8**；watcher **1** 也已通过，本组共 **48/48**。与前面的 mailbox **12/12** 合并考虑，但不重复累加其它已跑过的用例。
- 公开 stop 行为改变后的既有跨引擎 **1**、远程语义 **10**、M1–M6 **6** 与 thread 场景 **6** 均通过，共 **23/23**，日志 `/tmp/parrot-public-stop-compatibility.log`。
- `/tmp/parrot-third-batch-workspace-final.log` 仍在运行；这是明确标注过的混合阶段运行，不能作为当前全部修改的源一致最终验收。


### 继续发现：满邮箱淘汰/拒绝消息时的析构重入

- 新增 5 个回归在消息 Drop 中请求读取邮箱长度。旧实现 MPSC/SPSC/ring 的 DropOldest、SPSC 的 DropNewest/Full 拒绝路径全部失败：用户析构运行时持有 high-lane/receiver/producer 等锁，重入会死锁。测试用有截止时间的辅助线程观测锁是否释放，避免旧行为使测试进程永久挂起；旧实现 **0 pass / 5 fail**，证据 `mailbox-reentrant-drop-before.txt`。
- 现将淘汰的消息移出锁作用域保存，在释放锁后析构；SPSC 返回的拒绝消息也在 receiver 解锁后处理。没有修改 DropOldest/DropNewest 的容量和返回值契约。
- 追加最终复验 `/tmp/parrot-drop-and-lifecycle-final.log` 已结束、退出码 0：runtime lib **153/153**、mailbox **17/17**、individual stop **14/14**、公开 stop/配额 **8/8** 全通过，本组 **192/192**。新的完整 strict Clippy 通过，日志 `/tmp/parrot-drop-config-final-clippy.log`；fmt/diff check 通过。


配置回归的终止性也已补强：纯函数自引用测试限制 lookup 调用次数以立即捕获重扫；真实 load_file 子进程由父进程提供 180 秒外层兜底（包括调试宿主的进程启动时间），超时 kill/wait，不允许整套测试永久挂起。修订后 **16 单元 + 1 doctest** 再次通过（`/tmp/parrot-config-expansion-bounded-final.log`），配置 strict Clippy 通过（`/tmp/parrot-config-bounded-test-clippy.log`）。


本轮收尾边界：第四/第五批的上述修复及专项、strict Clippy、fmt/diff 均通过；先前完整 workspace 运行仍保留其阶段混合边界，未作为最新源码的完整验收。总体目标继续进行，尤其 B02 typed 生命周期、B04 监督代际/重启预算、B05 Actix 生命周期、B06 钩子线程亲和性与父子关闭，以及 C/D/E 的配置接线、安全、应用和发布项均未宣称完成。


## 第六批：生命周期钩子的线程亲和性（2026-10-09）

- 六个新增回归记录 init、handler、before_stop 的 future 构造、初次 poll 与 await 后恢复时的真实 ThreadId，覆盖 dedicated/sharded、单 actor stop/system shutdown、系统默认模式继承。旧实现 **0 pass / 6 fail**，证据 `lifecycle-affinity-before.txt`；修复后 **6/6**。
- before_stop 现在由实际所属调度器执行。dedicated/sharded 各增加独立的停止控制通道和在途 hook future 集合，保留原 handler 进度；共享池仍按许可隔离 poll。控制请求共用 StopHookRequest，统一 deadline、取消与 panic 处理，避免过期请求晚启动用户钩子。
- 单独停止和系统关闭均使用 actor 配置或系统默认值计算实际调度模式；系统关闭不再把钩子 Timeout 字符串化成普通 ShutdownError。dedicated 控制请求验证 processor 身份，避免向替换 worker 发送旧代请求。
- 新增取消 future 析构 ThreadId 和同步阻塞 before_stop（250ms）不拖住 40ms 系统 deadline 的真实回归，四项通过。加上六项基础 affinity 和原有 individual/system/entrypoint 测试，本轮 **37/37** 通过（`/tmp/parrot-lifecycle-affinity-final.log`）。
- 两个真实 actor 的控制请求边界单元测试验证过期或取消的请求既不构造钩子，也不改变 actor 的可服务状态；随后真正 shutdown 才调用一次钩子。runtime lib **155/155** 通过（`/tmp/parrot-affinity-lib.log`）。钩子 panic 隔离、调度注册与进度回归追加复验中（`/tmp/parrot-affinity-scheduler-regressions.log`）。
- 修正设计文档中公开 stop/并发 shutdown 仍未接线的旧叙述；更新 shard 模块文档为当前异步复用的实际行为，移除未经本轮性能测量支持的“自旋、零唤醒延迟”等保证。线程亲和性保证覆盖生命周期 future；普通 actor/消息值最终析构线程、父子关闭与监督代际仍未完成，不宣称 B06 整项完成。

### 先前全量运行结果与边界

`/tmp/parrot-third-batch-workspace-final.log` 已结束，退出码 0，123 suites：**1247 passed / 0 failed / 34 ignored**。该运行跨越第三至第六批的源码变化（早期二进制、被专项重编译的二进制及末尾 doctest 取不同阶段来源），因此仅为兼容性阶段证据，不作为最新源码的源一致整体验收。接下来需对固定快照执行全量验证。


第六批追加验证结束：`/tmp/parrot-affinity-scheduler-regressions.log` 注册 **3**、调度进度 **7**、生命周期 affinity **12**（含两个钩子 panic 隔离回归）、监督 **3**，共 **25 passed / 0 failed / 3 ignored**。三个 ignored 为原有显式 release 性能实验，未作为吞吐与延迟验收。最终 strict Clippy 日志 `/tmp/parrot-affinity-final-clippy.log`；源一致的全 workspace 验证将转入单独快照工作树，后续代码变更不覆盖其测试二进制或 doctest 源码。


### 第六批固定快照全量验证（运行中）

- 独立 managed worktree：`/Users/biluochun/.codex/worktrees/parrot-validation/parrot`，由 `31f7b2c` 创建后复制当前 491 个源码/文档/测试文件，保存逐文件 SHA-256 和执行权限。主工作区可继续修复，验证工作树及其独立 build 目录不再跟随变更。
- 清单归档 `validation-snapshot-2026-10-09.json`，原始清单 SHA-256：`8b8169b7f7c84ac284e388e9fb4824334ade7c937aca3232218c53e28f91c2e3`。运行器在开始与结束时核对该快照源码哈希。
- JVM fixture 在快照中用 `mvn ... package dependency:build-classpath -DskipTests` 重建；该命令只准备制品，不计为 JVM 测试通过。新 jar 启动预检取得真实监听端口后 kill/wait 回收进程；Rust↔JVM 集成测试使用这个新 jar 和明确 classpath。
- 验证命令：`cargo test --workspace --locked --no-fail-fast -- --nocapture`。日志 `/tmp/parrot-validation-workspace.log`，驱动状态/最终结果 `/tmp/parrot-validation-workspace-result.json`，驱动日志 `/tmp/parrot-validation-driver.log`。首次启动状态已确认 `phase=testing`，并非仅创建了计划。
- 复核发现现有 MQTT 测试缺 broker 时直接 return，JVM setup 缺依赖时也会返回 None；这些在 Rust harness 中可能被计为 passed，而不是 ignored。新验证会保存 SKIP 行并单独记录。MQTT 用例还有仅测试 MQTT 客户端模拟消息、未执行 Parrot 桥/WAL 的语义缺口；JVM 启动失败后潜在 silent return 与子进程回收缺口亦归入 E07，不能据全量 harness passed 声称已验证这些能力。
- 当前目标保持 active；后续优先继续监督代际/重启与 typed 生命周期，并修复上述测试证据缺口。固定快照验证完成之前不得标为最终全量通过。

## 第七批：监督重启的逻辑身份、预算与取消（2026-10-09）

- 首先增加三个可证伪的回归：原 typed/erased 引用恢复、两次预算准确产生三代、旧处理器不能影响手工替代实例。旧实现全部失败（`supervision-before.txt`），修复后全部通过。原 M3 测试重新查注册表隐藏旧引用失效，且用 `generations <= 3` 隐藏提前熔断；已改成原引用恢复、准确代数和真实 panic 原文断言。
- 监督状态并入 ActorRegistryEntry，与首次调度同步发布，修复初始化 panic 早于监督登记的竞态。Processor 独立保存 system weak 与原 mailbox 身份，故障通知不再依赖 context.try_lock；系统在注册锁内过滤旧代及重复通知，然后只预留一次重启预算。
- 增加 `spawn_supervised_with_config`，完整保留 mailbox、调度模式/affinity、背压、timeout 和策略。监督引用共享逻辑 mailbox cell，成功重启后原 typed/erased 引用可继续发送，watch 保留到最终终止；手工复用路径不继承旧逻辑引用。系统继承 DedicatedThread 时也按实际模式选择 mailbox，而非仅看 actor 是否显式填写模式。
- 失败清理期间保留 registry 占位，阻止同名创建；新代的注册、逻辑引用更新和调度在同步临界区发布，调度失败恢复旧占位及引用。清理会先 seal/cancel/deschedule，并隔离执行排队消息析构；未完成退休或超过截止时间时不会继续重启。
- 重启工厂使用可取消的隔离 poll；同步 factory panic、阻塞、晚完成都不会拖住 runtime。stop/shutdown 加入同一失败操作并撤销重启，发布时再次核对原邮箱和取消标记，阻止晚到 factory 覆盖手工替代实例。整个清理与重启共用 shutdown_timeout。已开始的同步用户代码仍不可强制终止；其结果不会再发布。
- 新专项 `test_supervision_contract` **14/14**，涵盖三模式初始化故障、配置/容量/背压/timeout 保留、重复和旧代回调、context 锁竞争、watch 最终一次真实死因、factory panic/阻塞/超时、stop/shutdown 竞争、清理析构阻塞及调度失败回滚。原 M3 **3/3**，日志 `/tmp/parrot-supervision-final.log`。
- 运行时兼容回归 `/tmp/parrot-supervision-lifecycle.log`：lib **155**、注册 **3**、affinity **12**、停止入口 **8** 全部通过。该命令也跑了旧版 M3 和已更新 A2/A3，用于兼容性；M3 最终断言以 `/tmp/parrot-supervision-final.log` 为准，重复执行不累加为独立覆盖。
- B04 仍为部分修复：失败代未读消息的保留/重放、真实父子树、父级策略执行、OneForAll 和业务错误 Resume、静态 typed 路径监督未完成。当前 Escalate 仍只按路径向直接父级投递 ChildFailure，不能等同完整层级监督。普通 actor/factory 捕获对象的所有析构线程与资源上限也未在本批全面解决。

### 本批发现并纠正的背压测试错误

固定快照全量测试实际报告 `test_thread_advantages::a2_backpressure_strategies` 失败；该测试在消费者尚未进入时用固定 sleep 灌消息，并在 backlog 尚未处理完时读取 ops，且根本未检查所谓“最新消息”的值。没有修改快照或放宽断言以掩盖失败。

主工作区重写 A2 为三调度模式 × 四背压策略：通过进入/释放双向信号确保消费者暂停，准确验证 Error 拒收、Block 挂起/恢复、DropOldest 留下 6/7/8/9、DropNewest 留下 0/1/2/3，并在 drain 后查询完整序列。A3 原先运行 20G CPU 循环约 110s 后以进程 RSS 作宽松判据；现用真实 actor 暂停、200000 个 1KiB 带析构计数的负载检查：接收恰好 256、拒收立即释放、保留数始终不超过容量、排空后全部 200000 个负载释放。该测试验证逻辑保留与回收，不声称测量了进程 RSS 或性能优势。

最终 `test_thread_advantages` **3 passed / 2 ignored**，约 0.25s；两项既有 release 性能测试仍未跑，不算通过。A2/A3 正式源码中已移除无界 Actix 对比和 CPU/RSS 误导性表述。strict Clippy 收尾日志 `/tmp/parrot-supervision-clippy.log`；截至本段写入尚待其最终结束状态，不能提前记为通过。

### 第六批固定快照的最终结果

运行已于 2026-10-09 09:36:05 UTC 结束，退出码 **101**，**127 suites / 1295 passed / 1 failed / 34 ignored**。开始与结束的逐文件哈希完全一致（`source_mismatches_after_test: []`）。唯一失败为上述旧 A2 背压测试；其主工作区修复后的专项通过不能追溯改写该固定快照的失败结果。最终结构化证据归档 `validation-result-2026-10-09-batch6.json`，详细原始日志仍在 `/tmp/parrot-validation-workspace.log`。

该运行的三个 MQTT 用例输出 `SKIP: mosquitto not running at 127.0.0.1:18831`，却被 harness 计入 passed；因此 1295 是 harness 数字，不代表 1295 个实际业务检查全部执行。JVM 使用重建并预检的 jar，测试正常执行；没有 JVM SKIP。未把 ignored、MQTT 缺依赖、Linux/容器与 release 性能算作通过。该结果覆盖第六批固定快照，不包含第七批监督实现。

第七批收尾复核又发现外部引用 stop 与重启发布之间的窗口：按一次读取到的旧 mailbox 匹配，会在逻辑 actor 仍存在时误报 ActorNotFound。现已为注册项保留逻辑 address cell，外部引用在同一注册临界区匹配逻辑身份；Context 自停继续匹配原 mailbox，隔离旧代动作。追加 64 轮 stop/工厂发布竞争回归，验收日志 `/tmp/parrot-supervision-identity-final.log`。先前 strict Clippy 已通过；这项追加修改后将再执行一次作为本批最终检查。

第七批最终收尾：`/tmp/parrot-supervision-identity-final.log` **15 个监督契约 + 8 个停止入口 + 3 个注册回归全部通过**；其中新增竞争测试执行 64 轮。`cargo clippy --workspace --all-targets -- -D warnings` 最终退出码 **0**（`/tmp/parrot-supervision-final-clippy.log`）；`cargo fmt --all --check` 与 `git diff --check` 通过。以上是第七批专项与静态检查；第六批快照的全量失败和 MQTT 隐式跳过仍如实保留。下一阶段继续 B02 静态 typed 注册/取消与 E07 外部依赖测试的真实执行，再推进完整监督、Actix 能力、安全控制面及 C/D/E 其它未完成项。总体目标保持 active，未声明世界级验收完成。

## 第八批：thread 静态 typed actor 的托管生命周期（2026-10-09）

### 修复与正式行为

- 五个旧版回归全部失败：typed actor 不计入注册表、同名重复、shutdown 后引用仍服务、动态桥 stop 假成功、关机后仍能 spawn。旧证据 `typed-lifecycle-before.txt`；基础修复后 **5/5** 通过。
- ActorRegistryEntry 改为 Dynamic/Typed 两种实际运行实体，共用原子路径注册、查询、actor_count 和路径枚举。动态与静态并发创建同名 actor 只允许一个成功；shutdown 的准入标记在同一注册临界区复核。typed 原引用、协议视图和 `into_dyn` 共享同一生命周期控制；旧引用再次 stop 不会影响之后手工复用该路径的新 actor。
- 系统持有托管 actor 的生命周期。单 actor stop、系统 stop/shutdown 及系统对象释放均封闭通道并启动独立拥有的清理；取消 stop 调用方不会取消清理。注册项保留到终止通知和完成结果确定。`get_actor_ref` 返回 spawn 时入口协议的动态桥；其它协议仍通过 typed `ref_for` 使用，并未谎称注册表动态桥支持全部协议。
- TypedChannelState 将入队和 seal 放在同一锁边界；满通道等待真实容量通知，seal 唤醒等待发送方。未接纳的消息、已排队的消息及在途回复都能结束，拒收返回 Stopped。显式 drain 掉 flume receiver 内的排队信封，防止幸存 sender 让负载一直留在队列。拒收负载的析构在准入锁外，新增重入检测单元测试。
- ParrotTypedDispatch 增加默认 init/before_stop。初始化成功后才 dispatch；stop 取消正在挂起的 dispatch，再执行 before_stop；初始化取消/失败、panic 不运行不安全的停止钩子。手写分派可覆盖生命周期方法；现有 derive 使用默认钩子。hook 错误和超时向调用方报告，成功完成只在实际 actor 析构后确认。
- typed 用户 future 的每次 poll（含 init、handler、hook 和析构）在有界许可下隔离，Pending 释放容量；使用系统 shared_pool_size 作为 typed 执行许可上限。持续 ready 的 backlog 按默认 SharedPool max_messages_per_run 主动 yield，其它默认模式暂用 10 条批次；未宣称已为 typed 接通 dedicated/sharded 模式。运行中的同步 poll 无法强制杀死，deadline 限制等待并返回 Timeout，未完成的清理由独立任务继续拥有。
- typed watch 可接收真实 Terminated，panic 原文被保留；它使用自己的类型化通道，尚无高优先级车道。动态 watcher 仍走原 High+Block 车道，既有截止时间/并行通知回归通过。
- 原始 TypedActorRef::new 通道没有引擎生命周期，stop 现在返回明确错误，移除“成功但不停止”的假承诺和相应错误测试。托管引用默认 ask timeout 采用系统配置；原始通道仍默认 5 秒。

### 实跑证据

- `test_typed_lifecycle` 最终 **19/19**，日志 `/tmp/parrot-typed-fairness-final.log`。涵盖真实注册/引用恢复、32 次混合类型路径竞争、32 次 spawn/shutdown 竞争、挂起 handler/满队列/负载析构、取消 stop 调用方、钩子错误/超时、初始化取消/panic、普通 panic、watch 死因、系统释放、默认 ask timeout、单许可 Pending 进度，以及另一 actor 必须在 backlog 耗尽前执行的公平性判据。
- API typed channel **17/17**，日志 `/tmp/parrot-typed-api-final.log`，包括拒收析构锁重入、原始通道 stop 明确失败和 Stopped 错误分类。末尾仅改了两个测试名字，使名字与 Stopped 判据一致，未改变测试体。
- `/tmp/parrot-typed-runtime-regressions.log`：runtime lib **155**、注册 **3**、亲和性 **12**、shutdown deadline **5**、停止入口 **8**、watch deadline **1** 全部通过。该命令还跑了当时的 17 个 typed 回归；不与最终 19 个重复累加。
- `/tmp/parrot-typed-final.log`：thread M4 **6**、Actix M4 **5**、当时的 typed 生命周期 **18** 全部通过。M4 的 debug 小样本计时只说明测试执行，不能据此声称性能领先；本次 managed typed ask 小样本比动态路径慢，正确性与完整生命周期接线之后还需正式 release 基准。
- 此前包含本批主体的全 workspace strict Clippy 已通过；最后公平性补丁后的最终 Clippy `/tmp/parrot-typed-final-clippy-2.log`、Node admin 执行器兼容回归 `/tmp/parrot-typed-node-regressions.log` 在收尾运行，不提前记为成功。

### 仍须继续，未缩小目标

本批没有补齐 Actix typed 生命周期、typed 工厂重启/父子监督、所有协议的动态分派、优先级以及完整调度/drain 能力。Node drain 遇到没有动态 mailbox 的 typed 实例当前返回 aborted（保留实例），不是成功排空；动态 drain 对 in-flight 的判断和 Stop 回执错误吞并也仍在 C 类清单内。Actix typed 目前未安装生命周期 owner，桥接 stop 因而明确返回错误；必须继续落实实际管理，不能把“报不支持”视为最终修复。整体目标继续 active，安全控制面、持久化/恢复、跨语言恶意帧、应用装配和发布门禁仍按原 A–E 清单推进。

第八批最终收尾：包含公平性修复的 `cargo clippy --workspace --all-targets -- -D warnings` 退出码 **0**（`/tmp/parrot-typed-final-clippy-2.log`）；Node admin 执行器 **25/25** 通过（`/tmp/parrot-typed-node-regressions.log`）；格式与 diff 检查通过。没有把第六批固定快照结果当作本批全量验收。下一阶段继续 Actix 静态与动态生命周期、typed 监督及已列出的 E07 测试证据缺口，并按原清单推进安全/应用/持久化等项；目标继续 active。


## 第九批：Actix 托管生命周期与宿主隔离（2026-10-09）

### 修复内容

- typed 的注册/重复路径/幸存引用 shutdown/动态桥 stop/关机后 spawn 五个回归在旧实现全部失败，证据 `actix-typed-lifecycle-before.txt`；动态 init/停止钩子与析构/挂起 handler 取消/Context 自停/Context 注册路径五个回归同样全部失败，证据 `actix-dynamic-lifecycle-before.txt`。先以正确行为建立回归，没有把旧版假成功当作规范。
- ActixActorSystem 合并重复 registry；typed 和 dynamic 共用同一准入锁与生命周期 owner。typed 保留准确请求路径，dynamic 继续生成独立实例路径；禁止关机后注册。原引用、typed 协议视图与动态桥共同收到关闭。stop_actor/path 查询都使用真实注册实例。
- 新 ActixSystemSettings 接通 mailbox_capacity、ask_timeout、shutdown_timeout、arbiter_count。线程池在系统创建的宿主线程上建立，后续从普通 Tokio 线程创建 actor 不依赖调用方的 Actix TLS。原懒创建方式在这种路径会 panic。arbiter 池保留线程 owner，显式关闭会等待所属线程退出；最后释放系统也触发生命周期清理。
- 系统 shutdown 关闭准入、并行停止 actor，再停止并 join 自己的 arbiter 池；移除固定 100ms sleep 与全局 System::current().stop。克隆系统及重复关闭共享完成回执，取消调用方不取消清理。deadline 返回明确 Timeout；阻塞中的用户同步代码不能被强制杀死，清理和线程 join 由独立任务继续拥有。测试验证另一 Parrot 系统与宿主 runtime 在关闭后仍服务。
- typed init、dispatch、before_stop、future 创建/恢复与正常析构留在所属 arbiter。取消当前 dispatch 后运行 stop hook；init 取消/失败和 panic 跳过 hook。排队 Flume 信封显式 drain，单个负载析构 panic 不会阻止继续回收后续信封。持续 ready 的消费循环每 10 条主动 yield。停止过期后不再构造晚到钩子。
- 动态适配器 init 成功后才开放 dispatch；同步和异步入口捕获 panic，普通 handler 错误允许继续服务。异步 future 继续拥有 actor/context，保持先前内存安全修复。Context 自停发起带外取消，不等待自身完成。外部 stop 等待整个 ContextFut（actor、handler 与邮箱）析构；不以收到 StopMessage 作为停止成功。
- 扩展回归发现 Actix 可在 hook wait future 合并前重复调用 stopping。现仅在钩子产生结果后结束，避免取消监听器或队列中的消息绕过钩子。动态 deliver 由无条件 do_send 改成有界 try_send，满队列明确返回错误。broadcast 等待真正投递并传播失败，不再生成无限后台 ask 任务后立即返回成功。
- 测试修正：手工构造的 Context 绑定真实 self 引用，并在最后 stop 后断言地址关闭；移除“stop 成功但继续服务”的错误前提。未初始化适配器的防御测试要求真正返回诊断，不再将 reply channel 丢失算通过。保留原有低层 Actix 取消时 handler 先于 actor 析构的测试。

### 验证状态

基础 typed **5/5**、扩展 typed **19/19** 已通过；动态扩展 **12/12** 已通过（含 init/hook 错误与超时、队列负载实际释放、阻塞 poll 的截止时间）。最后新增动态初始化/停止钩子的 future 构造 panic、默认 ask timeout 与完整 lifecycle 线程亲和性后，最终专项及兼容回归在运行：`/tmp/parrot-actix-lifecycle-final-tests.log`。全 workspace strict Clippy `/tmp/parrot-actix-lifecycle-clippy-2.log` 亦在收尾，未提前宣称最终通过。

### 保留的边界与下一阶段

- 原生 Addr、do_send 以及手工 ActixActorRef::new 为低层逃生路径，没有系统 owner。raw stop 已从立即假成功改为限时等待地址断开，但其停止请求仍经过原生队列；无法保证带外取消或返回钩子错误。后续须统一可托管低层引用的入口与契约。
- 动态 Actix 邮箱中用户析构 panic 的完整隔离、强制宿主停止时的资源清理、注册失败捕获对象析构线程等仍需审计；本批未据正常析构回归宣称覆盖所有异常析构组合。
- Context timer/watch/stream/spawn、真实父子/OneForAll/Resume 与 typed 自动重启继续按原 B 类清单推进。CPU/内存资源指标仍为旧占位，不能据 uptime 与真实 worker 数更新声称 C06 完成。
- MQTT/JVM 隐式跳过、默认 TLS/admin 授权、WAL/Raft 恢复接线、应用编排、跨语言恶意帧、Linux/容器/feature/release CI 等 A–E 目标未取消。没有运行新的固定源码全 workspace 测试；第六批全量失败记录保持不变。总体目标保持 active。


### 收尾验证新增的两个真实缺陷

1. 兼容测试虽显示 passed，日志却出现宿主析构阶段的 `there is no reactor running`。Actix ContextFut::drop 会在 Tokio 已退出后再轮询 stopping，原实现此时创建了停止计时器。新增隔离子进程测试安装 panic 计数器，旧实现明确失败（计数 1，期望 0；`actix-runtime-teardown-before.txt`）。现在检测 executor 已退出的强制 teardown，不再构造 timer/user hook；释放已拥有状态并保存非优雅关闭错误。正常托管 stop 仍运行完整 hook。
2. 将 typed 队列析构 panic 测试同步用于 thread 后，发现首个信封析构 panic 会提前结束 drain，使后续消息在幸存 Sender 中滞留。旧回归失败，负载计数只有 1/2（`typed-cleanup-panic-before.txt`）。thread 与 Actix 现在都逐个隔离排队信封析构并继续 drain，随后处理 actor 析构，保留首个失败及真实死因。该补丁还需最终专项结果确认。

收尾新验证：`/tmp/parrot-actix-cleanup-final.log`（动态 15 项、Actix typed 19 项、owned dispatch、adapter、thread typed 19 项与新增队列析构回归），以及最终 strict Clippy `/tmp/parrot-actix-cleanup-clippy.log`。较早 `/tmp/parrot-actix-lifecycle-final-tests.log` 保留未修复 teardown 的日志证据；它作为阶段兼容性运行，不可替代最后修复的验证，更不是固定源码全仓库验收。


第九批最终收尾：`/tmp/parrot-actix-cleanup-final.log` 退出码 **0**，六个 suite 共 **74 passed / 0 failed / 0 ignored**：Actix adapter **18**、动态生命周期 **15**、owned dispatch **2**、Actix typed 生命周期 **19**、thread typed 析构 panic **1**、thread typed 生命周期 **19**。最终日志没有再出现 `no reactor`；故意注入的 init/handler/hook/payload panic 仍保留日志，其回归断言全部通过。两个收尾缺陷均有旧版失败与修复后通过的正式测试证据。

最终 `cargo clippy --workspace --all-targets -- -D warnings` 退出码 **0**（`/tmp/parrot-actix-cleanup-clippy.log`），格式与 diff 检查通过。较早的扩大兼容运行 `/tmp/parrot-actix-lifecycle-final-tests.log` 也已完成，退出码 **0**、10 suites / **248 passed**，包括 lib155、跨引擎 context7、derive3、thread M4六项、Actix M4五项等；该运行早期存在被捕获的 teardown panic，已由后续专项补证，不能单据其 green harness 宣称当时无缺陷。重复测试未累加为独立覆盖，未执行新的固定快照全 workspace / Linux / release 验收。

下一阶段继续 Actix timer/watch/stream/spawn 与低层引用契约、完整监督及 E07 外部依赖测试的真实接线，并推进原定安全控制面、持久化与应用组装等 A–E 项。总体目标继续 active。
