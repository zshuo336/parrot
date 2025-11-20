# 09 开发扩展与正确验证

本章从使用者过渡到开发者：修改搜索质量、增加组件或实现新业务时，如何让 Parrot 的消息和生命周期真正接到端到端流程。

## 1. 先建立可重复基线

保留第一次成功运行的源码版本、构建模式、依赖版本、语料、参数、数据目录和结果。新行为使用新的数据目录验证，避免旧索引让错误代码看似成功。

第一章语料同时由 `run_tutorial_integration.py` 启动独占 HTTP 服务进行真实回归：检查精确三页、中文与 BM25 查询、robots 禁止页零请求，以及整个 JVM 重启后的结果和分数。网站首页统一链接到 `/`；HTTP 服务也能提供 `/index.html`，但它是另一个 URL，不能依靠页面内容相同获得 URL 去重。其它业务回归应使用每次唯一的搜索标识，并保存实际请求记录。

验证问题要具体：想证明“新词能搜索”就比较精确 URL；想证明“重试不重复累加”就比较 TF 或 BM25 分数；想证明“停止能清理”就检查实际组件和进程，而不是只看退出码。

## 2. 修改分词和停用词

入口为 [tokenizer.py](../apps/websearch/python/tokenizer.py) 的 `_tokenize` 与 `_STOP`。修改过滤规则会改变 TF、文档长度、词项数和搜索排名，因此不是只改变显示效果。

开发步骤：准备包含旧词、新词、停用词和零有效词的文档；记录预期 term 集；验证 Terms；验证完整文档提交；检查 JVM 查询分词是否兼容；重启 JVM 后再次比较。

只改 Python 停用词不能保证 JVM 查询端同步变化。词典版本也应作为发布输入记录。已有文档不会因为代码升级自动重新抓取和分词，必须规划重新索引或明确更新流程。

## 3. 扩展 HTML 抽取

先定位 Rust 抽取函数与 [HTML 测试](../apps/websearch/src/html_tests.rs)。如果支持新的标签、正文提取或编码，至少准备畸形标签、注释、script/style、相对链接、Unicode 与超大输入。

保持职责清晰：抽取器输出纯文本和规范化链接；tokenizer 不应被迫重新解析 HTML。若改成浏览器渲染抓取，需要重新设计执行隔离、资源限制、重定向和目标访问策略，不能仅在函数名里写 browser。

## 4. 新增一个业务消息

例子：希望 search 提供按文档 ID 查询元数据的功能。先定义 `GetDocumentV1` 与结果协议，明确不存在时的结果、字符串上限和读一致性。

实现要跨越 Rust 类型注册、请求编码、JVM handler、回复编码、Rust 解码、业务调用与 UI/接口。若只写了 JVM handler，使用者无法调用，就没有完成接线。

测试覆盖成功、不存在、错误类型、截断、尾部字节、无效 UTF-8 和超过上限。最后通过真实 Wire 与实际 HTTP 使用入口验证。遵循第五章的版本化规则。

## 5. 新增一个独立组件

例如在分词前增加语言检测：

当前 Websearch 专用清单宿主要求输入恰好四个组件，并严格解析三个远端角色，因此新增第五组件首先需要扩展 `application.rs` 的拓扑校验、角色解析和 workflow 引用结构。以下是需要开发的完整步骤，不是仅编辑现有 TOML 就能启用的功能。

1. 定义检测输入、结果、预算和失败策略。
2. 选择运行时与已有网关方言，提供真实入口和制品。
3. 扩展宿主拓扑与角色校验，在清单声明组件与依赖，增加必要 wiring。
4. 在宿主/业务中取得引用，实际调用并使用结果。
5. 定义超时后停止、跳过还是重试，说明对任务完成记录的影响。
6. 将启动失败、部署中断和停止回收纳入生命周期。
7. 用真实语料证明检测结果改变后续行为，并在重启恢复后保持一致。

“清单多了一段”只能证明声明存在；没有真实调用和结果影响就不能算功能完成。

## 6. 把 Websearch 的模式用于自己的应用

可以复用的模式是：Actor 封装状态，消息定义操作边界，网关屏蔽运行时差异，清单描述拓扑，宿主提供业务工厂，持久化确认定义恢复顺序。

应重新设计的部分包括 URL 身份、页面预算、词项布局、索引快照和抓取日志。比如支付或邮件发送不能通过“完整替换文档”获得相同幂等性质，需要各自业务键、去重记录和外部系统确认。

从最小本地 Actor 开始，再用两个进程跑一条真实远程请求，最后加入清单与恢复。不要在第一步同时启用全部框架 feature，导致无法判断错误来自哪里。

## 7. 分层测试入口

Rust 业务解析与局部逻辑：

```bash
CARGO_BUILD_JOBS=2 cargo test --locked -p websearch
```

Python 启动工具、部署与测试辅助契约：

```bash
python3 -m unittest discover -s apps/websearch/tests -p 'test_*.py' -v
```

真实 JVM Actor 的索引、存储与检索契约：

```bash
bash apps/websearch/jvm/test.sh
```

Websearch 主机多语言端到端：

```bash
apps/websearch/tests/run_all.sh
```

最后一个入口默认执行构建与真实 Rust/Erlang/Ray/JVM 业务场景，要求所有依赖齐备。`--no-build` 明确测试已有 debug 制品，不应把它记录为新源码构建成功。`--fast` 是兼容入口，不是可以省略所有真实组件的替代方案。

具体范围与前提见 [测试说明](../apps/websearch/tests/README.md)。其中可能保留历史批次结论；你自己的验收结论应以本次结果为准。

### 网络受限环境

真实 TCP/UDP 合同需要允许监听回环端口。`crates/parrot/tests/support/network.rs`
的能力探测默认在 bind 失败时报错；仅当错误为权限拒绝，并显式设置
`PARROT_ALLOW_NETWORK_TEST_SKIP=1` 时，采用该 helper 的测试才会提前返回并打印原因。
此开关仅适用于受限环境中的局部检查，不能用于网络或发布验收。Rust 测试 harness
会将这种提前返回显示为通过，因此记录结果时必须另外列出未执行的协议场景。
其它 bind 错误继续失败，不能通过该开关隐藏。

完整 workspace 还需要先准备实际业务制品及语言工具链，例如
`make build-crawler-business build-crawler-components build-ts`，并先安装 TypeScript
项目锁定的 npm 依赖。源码冻结包不自动包含 `node_modules`、JAR、BEAM 或业务构建回执。
大量真实运行时并行测试会消耗文件描述符；Linux 完整回归可显式使用
`ulimit -n 65536` 与 `RUST_TEST_THREADS=4`，同时保留默认环境下的首次失败记录。
此配置不证明低资源条件下的正确性；线程耗尽合同由独立受限门禁验证。

## 8. 按目标增加真实入口门禁

```bash
python3 apps/websearch/tests/run_launcher_integration.py
python3 apps/websearch/tests/run_tutorial_integration.py
python3 apps/websearch/tests/run_manifest_integration.py
python3 apps/websearch/tests/run_distribution_integration.py
python3 scripts/verify_websearch_images.py
```

分别覆盖监督启动入口、本书实际语料与服务重启、清单装配、可迁移分发和容器矩阵。它们依赖不同工具和产物，应按各脚本与测试说明准备。教程门禁默认重建，显式 `--no-build --binary /absolute/path/to/websearch` 可复用指定产物，报告会记录跳过构建及其摘要。分发集成使用本地 shell 替代 SSH 传输，因此不证明真实 SSH 认证与异构机器兼容。

组件生命周期还可以运行：

```bash
CARGO_BUILD_JOBS=2 cargo test --locked -p websearch \
  --test component_lifecycle_process
```

其可控 TCP 对端有利于精确注入管理失败，但不能替代真实 Erlang/Ray/JVM 组件测试。不同夹具提供不同证据，不应混成“完整系统通过”。

## 9. 什么样的断言能证伪缺陷

文档替换：先写含 A/B 的文档，再用同 ID 写只含 C 的文档。断言 A/B 搜不到，C 只关联预期文档，重复写 C 的分数不变。

零词文档：先写有词文档，再替换为空词集合。断言旧 postings 删除，但元数据仍存在，重启后保持相同状态。

持久化：观察确认后杀死整个 JVM，使用相同数据根重新启动，比较精确结果。仅重建 Rust 客户端不能证明磁盘恢复。

任务重放：在远端检查点成功、本地完成尚未记录的窗口中断，恢复后检查 URL 与分数不变，未完成任务没有丢失。

安全抓取：公网 URL 重定向到私网目标应被拒绝，且目标服务实际请求计数为零。只断言抓取器返回错误，无法排除请求已经发出。

robots：提供明确拒绝路径，记录真实 HTTP 请求，断言禁止页面没有被请求和索引。不要只看搜索不到，因为抽取或分词错误也会造成相同结果。

生命周期：部署回执丢失但远端组件实际存在时，停止流程仍应请求精确实例回收。只断言 socket EOF 不能证明 Actor 已销毁。

租约与单例：注入时间和 `quorum_ok` 的本地测试应检查过期／确认丢失的实际降级事件、同代次不能复活、续约与旧观察、单调时间和期限边界；它不等于kill进程、分区、持久fencing或真实接管耗时。执行端唯一性必须观测旧／新owner的真实副作用和旧命令拒绝，不能只看两份内存状态。SWIM Alive不提供互斥多数授予；当前实现边界见 [Singleton租约](../docs/SINGLETON_LEASE.md)。

分片：保存成员移除前的真实owner映射，断言精确迁移键集、未受影响记录及旧宿主保留；空成员集也须请求退休。spawn前先预留代次，所有回调携带原lease；同key同node替代后，陈旧回调不得激活、退休或删除新记录。验证取消构造、成员恢复和真实退休后才能重建。实际本地状态交接须使用不同实例、原实例退休回执及明确的checkpoint和效果ID；对同一实例累计发送不能证明drain或迁移。新增／移除节点的最小扰动、无效配置原子拒绝及地址字节边界另有公开契约，见[本地分片契约](../docs/SHARDING.md)。独立进程kill／分区／重启、网络重试／去重和接收端fencing仍需真实端到端测试。

生命周期追踪：分别核对构造器的创建身份、实际初始化/停止钩子的独立子身份、future 清理和记录终态。挂起不应提前完成，未调用的钩子不应有记录；首次停止请求的等待者取消后，后续等待者不能替换它的父身份。工厂单次失败测试应显式选择停止策略，重启测试则单独验证替代代次及触发失败的真实消息。动态工厂在代次交替时可能拒绝投递，应等待替代初始化的真实进入门再验证业务，不能误套类型化工厂的保留邮箱契约。见[生命周期钩子追踪](../docs/ACTOR_LIFECYCLE_TRACE.md)。

Native生命周期：用真实Rust／C动态库验证load／construct／drain／retire，等待独立C构造／dlclose进入门后再检查记录仍在途，取消等待不能提前完成。记录必须与实际hook调用一一对应，缓存drain回执不产生新调用，失败加载不伪造构造／drain，ABI panic与普通错误分开检查；同时核对业务回复、destroy次数和真实退休。首次stop显式None不能被后续身份覆盖。HTTP按实际kind和父子身份核对，测试枚举所有kind时按种类数量分配容量并检查零淘汰。宿主记录不证明guest收到生命周期追踪能力，见[Native worker生命周期](../docs/NATIVE_WORKER_LIFECYCLE.md)。

Native guest生命周期另用可选独立ABI验证：将guest实际观察身份与host阶段记录比较，检查嵌套span及成功／错误／panic的未结束终态。构造、消息、drain和销毁间过期句柄不得复用；每次真实调用分别有256额度，不能因完成早就无限录制。失败drain仍销毁，多个实例必须先全部drain再开始销毁，每次destroy无论返回码都实际释放且不重试。兼容性同时验证旧入口不隐式启用扩展、旧guest回退及依赖库外来descriptor拒绝；取消等待和关闭recorder不能提前结束正在执行的hook。实际HTTP与Node本地管理测试不冒称独立网络或跨平台认证，见[Native guest生命周期](../docs/NATIVE_GUEST_LIFECYCLE.md)。

Wasm录制适配器：使用实际component测试宿主返回零ID、跨trace、根span复用、活跃／已完成子span复用时的拒绝。既要检查返回原因，也要检查拒绝项没有发布句柄或占用额度、所有owner恰好析构一次、已完成状态保持、未结束状态为Error，以及下一调用从新实例执行；拒绝项析构panic不能替换原错误。正常256项额度和同步／异步ASK、TELL、启动、drain应单独作为正控，不能只检查错误代码编译成功。身份历史按调用有界，不把本次去重声称为跨调用全局唯一性或认证，见[Wasm追踪](../docs/WASM_TRACE_CONTEXT.md)。

Wasm恢复启动：对实际毒化Store的恢复on_start与随后业务／drain分别核对根身份、guest子记录、256+256预算及跨trap句柄的精确接续。新增`recovery_recording`契约和App录制测试检查健康／未启动／显式启动／重建失败不伪造恢复、默认拒绝、显式None、identity-only与关闭recorder、失败恢复不执行后续阶段、scope工厂／回调／析构错误和取消。真实宿主消息、HTTP及Node Stop测试核对完整父链；构造消息请求future应置于trace作用域内，不能构造后才安装上下文。异步挂起测试检查import构造、每次poll、正常及取消Drop的上下文、尚未结束的真实recording owner，以及作用域返回后的TLS隔离；完整future析构失败传播仍单列F1/F2，不从本批局部通过外推。命令与限制见[恢复启动](../docs/WASM_TRACE_CONTEXT.md#recovery-startup)。

Wasm异步回归要分别覆盖host future构造、poll、完成后Drop和取消Drop。构造／poll
失败且清理正常时允许毒化Store重建；future、订阅或恢复scope清理失败则保存首次
错误，`cleanup_error()`可在取消后查询，`is_alive()`为false，不得重建后继续执行。
已接纳recording的显式finish、未结束span退休及root析构／finish失败遵守同一契约，
覆盖同步、异步和恢复取消；同步恢复scope析构失败也不得丢失。较早future错误不被
后续recording失败覆盖，后续兄弟span收到Panicked，已发出的终态不倒改；准入前拒绝
或未poll的recording清理不得改变组件状态。使用实际component的
`cargo test -p parrot-wasm --all-features --test recording_cleanup`验证这些边界。
服务订阅还须验证同一次调用的后续副作用：`next`因EOF、Closed或非法事件退休接收器时，
析构失败必须在返回guest前trap，不能等最终退出时才报告错误。使用实际component的
`cargo test -p parrot-wasm --all-features --test service_next_cleanup`，让guest在next后
紧接注册及TELL，分别检查ASK／TELL／启动／drain／恢复启动没有越过清理失败，兄弟
接收器仍逐项退休且每个owner只析构一次；正常退休、4096字节UTF-8事件、超时及
普通非终结错误应作为独立正控，保持正常继续执行和handle存活语义。
检查实际guest／恢复／App lifecycle记录为Panicked，并验证drain重复查询保存真实
清理错误且不重调guest。还须直接丢弃尚未poll的初始化／ASK／TELL／start／drain
future，检查传入capability析构受保护、未读取parent／运行guest或改变状态。
`async_cleanup`的三个ignored入口是由父测试实际执行的进程内故障worker，父测试
必须断言子进程成功及确实执行一项测试，不能只看父进程退出码。独立源码变体的
编译失败、signal／abort、零测试和过期二进制都不能算行为检出；冻结tar要保留
有效修改时间，并从编译日志和新增用例核实确实执行变化后的源码。

追踪导出：按真实 kind 和因果身份查找记录，同一路径可能同时有初始化和消息记录。
初始化成功、后续启动消息失败时不能把初始化也判为失败。HTTP 可将初始化与应用
启动根拆成不同批次；失败回执须核对已确认、待确认和未导出余项的总账，重试保持
原批次，环形缓冲丢失不能被 HTTP 成功掩盖。

并发退休：先用明确的进入门确认构造器或析构器正在执行，再检查停止结果、路径预留和真实资源归还。不要把机器调度耗时当作业务截止时间。Tokio 的阻塞任务可能抑制虚拟时间自动推进，`sleep` 又有毫秒取整；测试应显式推进到配置期限之后，再等待对应回执，不能只推进恰好相同的时长便假定定时器已就绪。推进测试时钟不改变生产期限，也不替代退休语义断言。

夹具看门狗：它只限制失控测试，不证明生产行为正确。报告超时前先释放夹具锁，释放守卫应能处理锁中毒，避免首个失败变成析构期间再次 panic 和进程 abort。用受控调度延迟验证夹具仍能检查原契约，同时保留原失败。工厂退休测试的真实修正与对照见 [native 服务批次记录](../docs/review/independent-2026-10-08/native-services-20261110/README.md)。

端口所有权：启动自己的监听进程占用候选端口，验证应用选择实际端口或明确失败，且无关进程仍存活。避免用“连接某端口成功”证明本次应用就绪。

宿主委托 future：用独立子进程覆盖构造、poll、取消和完成后的 Drop，分别注入
future 与 panic payload 析构异常。创建输出必须用真实 actor 验证：检查兜底清理
之前已经实际退休、失败组件未执行 START、依赖最后停止、原引用未调用用户克隆
或路径回调，并检查 context 可重新装配。组合业务错误、future 析构失败和依赖
回滚失败时，既要检查所有诊断，也要检查 `primary_error()` 仍保留原类别。
部分创建错误另验证其返回的原身份全部被接管，不能只检查错误文本。
入口为 `cargo test -p parrot-app --features host --test host_future_ownership`。

还要检查外层 async 块内部直接 await 的委托。`resolver_future_disposal` 除原有
timeout/取消合同外，新增本地/网关查找、接线、派发和本地存活查询的构造异常、
poll 与 Drop 连续异常及返回后析构失败合同。七种 poll/Drop 组合在只保护外层时
实际 SIGABRT；修复直接委托边界后，31 项全部通过。父进程必须同时检查子进程
状态和实际执行计数，检查兜底前真实退休，不能用 runtime 退出掩盖跳过的回滚。

原生路径用例必须先构造当前文件系统支持的路径。Linux 使用非法 UTF-8 字节，
macOS 等平台使用受支持的非 ASCII 名称；分别保留直接路径和符号链接读取断言。
创建文件时返回 EILSEQ 只说明夹具前提不成立，不能当作加载器已经拒绝路径。
FIFO 拒绝用例在独立子进程执行实际 loader，父进程在五秒内检查类型化错误、
实际执行一项测试和成功退出，超时则终止并回收子进程。仅在同一异步 runtime
外套 timeout 无法阻止阻塞 open 卡住测试。

`dylib_assembly` 的独立合同使用受限子进程调度，每次只运行一个合同进程，
避免不同测试的动态库加载/卸载消耗彼此的生命周期期限。单项合同内仍运行原有
并发、32 worker 负载及原截止时间，父进程检查实际执行且负责超时回收。
此调整不证明生产中的 OS loader 竞争消失；需要独立的集成负载测试评估该风险。

## 10. 故障注入要改变真实业务边界

有效例子包括修改一条真实 Terms 回复中的 doc ID、把 DocumentsAck 数量改错、截断 Checkpoint 回复、让落盘同步失败、在指定窗口终止进程。

预期应同时检查：非零退出或明确错误、没有错误完成标记、本地日志没有提前完成、远端状态符合可恢复规则、拥有的资源得到回收。

只在一个模拟函数里返回 Err，而真实调用根本没有经过该函数，不能验证端到端失败处理。测试夹具也可能有错误，应记录实际线上 type key、请求和状态变化。

## 11. 项目级检查与证据边界

完整 workspace 需要 Node/npm、C++ 编译器、Wasm 工具和额外业务产物，准备过程见[根 README](../README.md)。`cargo test -p websearch` 不是全项目门禁；主机端到端不是容器门禁；debug 正确性不是 release 性能结论。

建议每次验收记录：源码 revision、dirty 状态、构建 profile、锁文件与依赖、平台、运行命令、成功/失败/未运行、日志路径和未覆盖范围。

缺依赖应记为未执行或失败，不能写通过；被 ignore 的测试没有自动获得覆盖；只看到 CI 配置存在也不代表远端 CI 已成功。

## 12. 推荐练习

先在本地语料新增一页和链接，使用新目录冷启动，验证新 URL；再通过 keep 新增未发现 URL，观察追加与去重；然后运行 serve-only，确认不再请求语料站。

接着在独立实验分支实现词项规则调整，写出精确 Terms 测试和 JVM 重启搜索测试。最后尝试新增只读消息，完成协议、handler、调用和 HTTP 展示全链。

练习完成标准是可观察的真实业务行为与相应失败验证，不是修改行数、测试数量或漂亮的架构图。
