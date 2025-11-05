# Crawler 应用契约

`make test-crawler-app`从当前源码构建Rust应用、通用JVM网关及crawler的JVM／Erlang组件，再运行真实业务。依赖Ray、Java／Maven、Erlang与Rust工具链，缺失即失败。现有CI在安装这些工具后调用该目标；它与test-app-polyglot共用组件构建前置。

`run_integration.py`使用正式网关owner启动三网关，全部监听动态端口，实际运行crawler-lab CLI。正常用例要求四运行时业务对账和退出成功；18个场景分别使用独立进程／状态。除正常业务和合法空白JSON外，自有TCP代理注入FrontierBatch、FrontierAck、两种IndexAck、IndexStats、Healthz和Search回复故障；包含截断、合法但错误的方言/计数/type-key，以及假空搜索结果。异常必须非零退出、有应用错误、没有panic和PASS标记，代理必须确实命中一次；判据不会因Python -O消失。日志保留于打印的/tmp/crawler-contract-*目录。

作用域结束时只关闭本次CLI、代理socket／线程与正式owner持有的网关。超时、SIGINT和SIGTERM走同一清理路径；SIGKILL不能运行finally。没有复用固定端口、全局pkill、共享日志或应用默认数据目录。已有制品可直接运行python3 apps/crawler-lab/tests/run_integration.py，不能据此代替新源码构建。

Rust协议回归运行 `cargo test --locked -p crawler-lab --bin crawler-lab protocol_tests`。包括所有截断位置、计数／尾部／UTF-8、条目预算、合法字节互锁、回复类型错误和真实TCP损坏后下一条有效回复；并非仅测试本地encode/decode自洽。

数字回复要求精确消费：FrontierAck为4字节确认标志1，IndexStats为u32词项/u64 posting计数。IndexAck沿用共享key的两种合法形状，Ray为8字节(pages,terms)，JVM为4字节accepted；codec接受两种合法形状，具体调用方必须核对方言及本次实际计数。只有匹配成功才清理本地批次并增加成功指标。

Healthz JSON要求terms/postings/queries三个u64字段，Search要求doc(i64)/score(u64)数组；拒绝重复字段、错误类型、溢出、UTF-8错误、尾部数据、重复doc及非确定性排序。JSON允许合法空白/键顺序，Search允许合法空数组。字节受Wire上限约束、搜索数组在解析时最多65536项。应用以本地合成页面的独立索引检查词项/postings、四个查询的完整top-5排序和最终查询次数，因此格式正确的假空结果也不能骗过PASS。

错误会通过Result执行remote shutdown并返回非零；不保证撤销已经成功的远端写入，两路索引尚无原子事务、可靠重试或持久工作账本。CLI参数、网关部署自动回滚、整体资源额度及其它开放项目仍需继续改进。运行本门禁不等于发布性能/真实断电验收。


CLI先完整解析、后创建runtime/连接。未知/重复/缺参、非法数值或endpoint、direct/registry混用、重复目的端点、缺失显式明文开发选项均exit2，不进行DNS/连接。--help/-h单独使用exit0。pages允许0–1000000（0为合法空工作负载），depth为u16，fanout允许0–256，batch为1–256，registry wait为1–3600秒；这些工作负载边界不等于进程总内存保证。registry监听允许port0，目的端口必须非零。

启动/绑定/连接/registry等待失败正常返回exit1，已创建client先shutdown。DNS解析最多等待5秒，按解析器顺序尝试所有去重地址；每网关连接尝试共用10秒期限。OS解析器底层调用不能由此证明强制取消。registry在同一等待期限内要求三个node登记与实际link均就绪，取消原额外固定睡眠。真实应用新增localhost、空工作负载和三网关主动注册场景，Rust子进程契约检查参数失败前无网络副作用、监听占用及等待失败无panic。

run-lab.sh 是 Python launcher 的薄入口。启动器使用动态端口、私有日志、受控进程组和 EOF guardian；--skip-ts 由启动器消费，明确记为跳过。`make check-crawler-launcher` 覆盖真实子进程的并发、构建／业务失败、伪就绪、超时、TERM/INT、SIGKILL、忽略 TERM 的后代端口和已有目录保护。夹具只替换组件命令，不承担多语言业务证明；真实业务仍须运行 make test-lab、registry 场景及 run_integration.py。Ray 在 macOS 的进程组行为须由真实 Ray 进程树复验，普通 Python 夹具不够。

run_regression.sh 使用同一所有权机制，从源码构建并精确比对固定 golden 的计数、查询顺序／分数和完成标记。缺失、重复、数字前缀或 JSON 数值类型变化均失败；不比较耗时／JSON 对象键序。--keep-output 保留为兼容选项，日志始终保存。历史 golden 保持原样；匹配它不代表 manifest 迁移或 cid 轨迹已实现。

Frontier 另由 `make check-erl` 运行真实容量契约。默认每实例 Queue 为 65,536 项／16 MiB，Seen 为 262,144 项／16 MiB；CLI 的 pages 上限不是组件容量预留或可完成页数保证。过载拒绝整批且不污染去重，消费后只释放 Queue，Seen 直到实例退休。测试实际达到默认上限，检查 32 个并发 legacy 写入、二进制引用大小及 pool 独立额度；具体配置与计费见 [Erlang 组件契约](../../../docs/ERLANG_COMPONENTS.md)。

`make test-crawler-app` now also builds TypeScript (run `npm ci` in `interop/typescript-lite` first). The valid crawl scenario queries the actual populated JVM search component with the TS client using explicit bin negotiation. Its `typescript.log` and `CRAWLER_TS_QUERY_OK` record the result; a Rust crawl alone cannot satisfy this scenario.

`make test-crawler-business` 是新增的三形态真实业务门禁，先构建 Dylib／Wasm 与全部网关制品。`component_business.rs` 通过公共宿主实际执行两类制品，比较固定向量、最大批次、并存实例的状态／配置隔离及错误 digest 拒绝；不以 Echo 或 embedded 代替组件业务。`run_component_business.py` 再执行 direct 的 embedded／dylib／wasm 及 registry 的 dylib／wasm 五组真实链路，逐组比对未修改 golden；direct 必须完成 TS 查询，registry 明确记录 TS 不可用。每组记录采样后代身份、无活进程遗留和已知网关端口释放；驱动也采样可观察的后代 TCP listener 并复查可重新绑定；这不是连续、无遗漏的内核事件追踪。协议与预算见 [组件内核](../core/README.md)。

CLI 子进程测试持续消费 stdout/stderr，启动和实际监听后的 registry 等待分别保持3秒期限。大型 Wasm 宿主二进制在 macOS 首次装载的耗时不会被误当成注册等待超时，也没有取消真实注册超时与端口释放断言。manifest_migration.rs 仍仅验证静态模板，完整 golden 字节 hash 用于防止基线被修改；真正的 manifest 迁移和 CID 轨迹并未由这些测试完成。

`make test-crawler-manifest` 是标准装配迁移门禁：Props／Dylib／Wasm 三形态保留原 golden，改名组件／版本／工作负载、显式零覆盖、结构合法但业务错误的接线都进入真实执行。TS 必须在 teardown 前完成并比对独立完整排名。额外场景覆盖查询超时、错误回执、Rust 宿主 SIGTERM／SIGKILL，以及真实 TS oracle 断言失败。驱动记录进程 PID+creation time、嵌套 listener、进程终态和端口重新绑定，端口探针先以实际占用／释放反例自验。历史 CID 轨迹不在此门禁的证明范围。
