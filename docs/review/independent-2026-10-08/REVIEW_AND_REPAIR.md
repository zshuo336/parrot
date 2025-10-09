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
- B01 待修复：shared worker / same shard 等待 actor 内 ask 占调度槽；1 worker A→B 250ms 超时可复现。
- B02 待修复：typed actor 不在系统 registry；同名重复创建，actor_count=0，shutdown 后两实例仍回 42/43；缺监督/取消/统一终止。
- B03 待修复：EngineContextHandle 无生命周期，可用安全代码逃逸；Actix async transmute 自引用生命周期/别名设计需要移除或严格重构。
- B04 待修复：supervision restart 预算双计、旧 ref/mailbox 失效、配置丢失、模式/子树语义不完整。
- B05 待修复：ActixContext stop/timer/watch/stream/spawn 未完整执行。
- B06 待修复：系统 shutdown 无完整 deadline；sharded scheduler 未被 shutdown_internal 停止；panic 路径未统一通知监督。
- B07 待修复：Wasm 内存 limiter / epoch 生产驱动 / WIT 主动通信能力未接齐。
- B08 待核查修复：dylib 加载信任、FFI 参数寿命、析构/显式卸载、平台扫描 fail-open 与实际 TLS 注册边界。

### C 应用、配置和可观测性
- C01 部分修复：组件在 start hook 前登记到逆序回滚栈，hook 失败会停止当前组件；hook_path 仍未真正路由到指定 actor，stop/drain 与回执契约待补齐。
- C02 待修复：assemble 克隆并 overlay cfg 后未交给执行器；component.config 在 host/supervisor 被丢弃；CLI 建默认引擎忽略文件参数。
- C03 待修复：stop 一个组件 kill 全网关；多实例部分成功失败的回滚、所有权与组件命名隔离不完整。
- C04 待修复：manifest wiring / placement / upgrade 声明与执行面缺口；Wasm/Dylib admin 部署，应用迁移与 reconcile/rollback 执行闭环。
- C05 待修复：MirrorRef 仅输出 skipped 日志，没有镜像投递；record/replay 与运行时接线、非法 hex 处理需补齐。
- C06 待修复：观测指标部分硬编码，需明确 unavailable 与真实计数，提供 trace 传播和实际 exporter。
- C07 待修复：expand_env 自引用死循环；默认值/未知键/数值边界/配置生效验证。

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
