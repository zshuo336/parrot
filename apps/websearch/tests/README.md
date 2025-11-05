# Websearch 验收入口

`run_all.sh`运行当前主机上的真实Rust应用、Erlang frontier、Ray/Python分词和JVM检索。默认执行锁定依赖的Rust构建、JVM网关/应用组件构建、Erlang编译及JVM契约测试；缺少cargo/java/erl/erlc/mvn/jar/unzip/Ray/jieba时失败。不会自动安装依赖。

```sh
apps/websearch/tests/run_all.sh
python3 -m unittest discover -s apps/websearch/tests -p 'test_*.py' -v
bash apps/websearch/jvm/test.sh
```

`--no-build`显式测试已有debug制品，适合先单独构建后的排查；不能据此称为新源码构建通过。`--fast`保留为旧调用方的兼容别名，两种方式均输出`Scope: host processes. Container matrix NOT RUN.`。未知参数失败。旧固定容器名的compose分支不再自动运行；独立容器入口为 `python3 scripts/verify_websearch_images.py`，准备步骤与范围见 [镜像说明](../../../deploy/images/README.md)。这里没有把Docker不可用记成通过，也不把主机测试计入容器结果。

## 判据

- 固定HTTP语料包含三页、真实404和robots禁止路径。每次语料有唯一搜索词，真实搜索结果必须是预期URL集合，不能靠已有端口、旧日志或残留索引通过。
- 命令行缺值、未知参数、非法数字及带凭据URL必须在数据目录创建前失败；帮助成功退出。测试占用完整的21个候选端口，验证Web无法启动时同样不会修改数据。
- direct和registry分别使用真实握手、部署、frontier、tokenizer、index和Web入口；registry使用系统分配的监听端口。
- `--keep`保留已有文档并加入一页；重启整个JVM进程后，URL和BM25分数必须与重启前完全一致，且serve-only不能再次抓取。
- 冷启动必须清除旧磁盘与内存结果，相同种子冷启动必须重新抓取。分页不仅看显示页码，还检查真正的末页行；零和极大分页参数不得导致运算溢出。另验证空库、晚到网关、由本测试主动占用的端口和端口0的实际服务地址。多个排队种子下的硬预算，以及失败消耗预算且不回填，分别通过业务与HTTP计数断言。
- 11项真实故障注入覆盖Clear/Push/DocumentsAck/Terms/Flush坏载荷、文档确认数量和回复类型，以及分词结果中外部docId、重复词、零词频和空词；要求非零退出、无panic、无完成标记，拒绝的Push不能污染去重文件。代理按type-key只改一条实际业务回复，不改握手或管理帧。`--faults-only`可单独执行完整故障组。
- 所有抓取次数最终精确对账。Rust 抓取器忽略环境代理；验收显式传入 `--crawl-origin`，将页面和 robots 请求（含重定向）限制到本次拥有的语料 origin，并关闭内置种子回填。域名仍不能解析到私网；显式 IP origin 只授权该协议/IP/端口。网关的环境代理与 Ray 元数据探测代理分别记录辅助请求。这不是完整网络沙箱。
- Rust JSON契约覆盖四种回复的schema、Unicode转义、截断与尾部、精确u64计数、HTML属性/链接方案、高亮重叠；真实TCP对端将坏JSON传到HTTP四入口，错误必须返回5xx，随后同一网关连接的合法回复必须恢复200。
- JVM契约测试新增全部32种控制字符保留转义与德语数字Locale，且恢复测试前Locale。
- JVM契约测试直接驱动真实actor，以独立计算的BM25值验证词频赋值、重复提交、重放多个全量快照、unsigned offset和稳定的同分排序。
- Python入口测试覆盖错误业务结果、死进程就绪误判、拥有的进程组清理、无关进程存活、代理拒绝及构建失败传播。断言不使用会被Python优化模式删除的`assert`语句。

## 所有权与结果

每次创建`/tmp/ws-e2e-*`，该目录内保留日志、数据、Ray runtime临时文件及`result.json`（构建状态、场景、制品SHA256和请求记录）。完成和失败都保留用于排查，用户可按路径自行清理。本测试不读写应用默认数据目录。

每个子进程由 `OwnedProcess` 在独立 session 中启动。`wait/poll` 只观察退出而不回收，最后一次进程组信号发送后才回收组长；清理失败保留身份以供重试，成功退休后不再发信号。先向 owner 发送 TERM，给予 Ray wrapper 调用 `ray.shutdown()` 的宽限期，再清理剩余组成员。不使用全局`pkill`、`ray stop`、固定日志文件或共享数据删除。HTTP fixture随作用域结束关闭。正常、错误、SIGINT和SIGTERM均经过清理；操作系统强杀测试主进程（SIGKILL）不可能执行finally。

网关优先绑定端口0并读取自己的启动日志。Suite中的Web应用默认也传入`--port 0`，读取`PARROT_WEBSEARCH_PORT`回执后访问实际由应用持有的socket，不再探测后释放端口。显式端口的占用与回退用例仍保留。回执前退出返回真实进程与退出码，业务就绪仍须通过实际HTTP请求；不会用日志替代就绪，也不会终止不相关占用者。独立runner测试通过真实socket连接验证端口回执，并覆盖回执前启动失败。

这些结果不证明公网质量、完整 TLS 认证、严格逐跳限速、崩溃一致性、可靠抓取重试、容器部署或高负载性能。Rust 单元测试已覆盖私网/保留地址和 DNS resolver 的 SSRF 反例，但不替代真实 DNS rebinding、证书轮换或公网网络门禁。

## Registry注册边界

`cargo test -p websearch -p crawler-lab --test registry_process`运行真实应用子进程，覆盖gossip已知地址＋其它身份连接不能伪造就绪、超时身份诊断、serve-only JVM单网关及三网关实际注册。共用夹具仅实现握手，不实现多语言业务；它不能替代上述真实网关验收。`cargo test -p parrot-remote --all-features --test direct_registration`补充真实TCP关闭但尚未退休的sender、同身份替代连接与地址／链路分离。

## 本机启动器所有权

`python3 -m unittest discover -s apps/websearch/tests -p test_launcher.py -v`驱动生产启动流程，使用真实子进程、HTTP和持有监听socket的后代。覆盖构建失败、死进程日志、慢响应总期限、错误健康结构、退出码、参数边界、serve-only、并发隔离、TERM／重复信号／SIGKILL与不相关进程保护。夹具替换网关命令，不证明多语言业务。新增Rust `cli_validation_process`验证`--validate-only`在真实端口被占用时仍无绑定／数据副作用，且坏参数仍拒绝。

`python3 apps/websearch/tests/run_launcher_integration.py`是常规真实入口门禁，构建当前dev制品，验证direct／registry业务和serve-only重启恢复，结果默认保存在打印的私有运行目录。CI已接入全部Python发现、优化模式所有权测试和这个真实入口门禁；本次只在本机执行，未冒称远程Linux CI已经通过。审计证据位于`docs/review/independent-2026-10-08/websearch-launcher-221`。该门禁独立核对精确索引、搜索URL／分数、HTTP请求及拥有的guardian回收，Ray元数据探测使用独立拒绝代理。它没有执行整个发布或容器矩阵。

## 可迁移分发与URI边界

`python3 -m unittest discover -s apps/websearch/tests -p test_distribution.py -v`验证完整依赖打包、同名jar顺序、归档完整性／边界、并发安装、已有目录／锁保护、字面SSH参数和编译失败禁止启动。

`python3 apps/websearch/tests/run_distribution_integration.py`执行真实构建及分发入口，只将SSH传输替换为本地shell。接收器、独立安装目录和包内启动脚本均实际执行，Erlang／Ray／JVM和Rust共同验证两文档索引与JVM重启后的相同URL／分数。目录包含空格、中文和URI保留字符；JVM classpath全部指向包内依赖。它不证明真实SSH认证、异构主机、Linux／容器发布或断电恢复。CI已接入该门禁，但本批未运行托管CI。

## 独立应用退出后的组件生命周期

`CARGO_BUILD_JOBS=2 cargo test --locked -p websearch --test component_lifecycle_process`
运行真实应用进程和可控 TCP 对端。覆盖正常 serve-only 停止、部署过程中收到
TERM、后续部署拒绝、部署回执不符合协议、启动前 Stop 拒绝、业务坏回复、
回收拒绝／30秒无回执，以及已不存在的组件。检查对端保留的实际状态，不把
socket EOF 当成组件已停止。无回执对端继续回复心跳；其它节点须在其等待期间
收到 Stop。原错误与回收错误都必须可见，回收失败不能退出0或打印成功标记。

`python3 apps/websearch/tests/run_component_lifecycle.py` 使用已准备的当前 debug
应用和网关／业务制品，应放在 `run_launcher_integration.py` 的源码构建之后；
CI 已按此前置关系接入。四个真实 Erlang/Ray/JVM 场景验证：三组件正常回收、
同一 JVM 内新 search 实例回放并再回收、Ray 制品缺失时回收先前 frontier 且
保留尚未尝试部署的旧 search、真实 Clear 回执损坏时回收全部组件。网关进程
一直存活，观察器通过新的 TCP 连接查询状态；另外一个不同名称的 frontier
全程保留。精确 URL、搜索分数和 HTTP 请求次数也必须一致。

该入口不自动构建，结果记录二进制 SHA-256，日志和失败数据保留在打印的
`/tmp/ws-components-*` 目录。它不替代网关自身的进程／文件锁回收测试，不证明
网络分区、克隆身份并发接管或强杀后自动无残留；存活网关上的恢复由下面的身份测试验证。


## 完整文档替换与安全重放

爬虫的索引路径使用`IndexDocuments`，先完整验证请求，再一起替换每个文档的
元数据、倒排与文档长度。`DocumentsAck`只确认内存接受；周期／最终`CheckpointV2Ack`（旧FlushAck兼容）
才确认完整快照持久化。原`DocMeta`/`IndexTerms`保留原来的增量语义，新客户端
不会在旧JVM不支持新接口时退回不完整写入。

Rust `indexing`测试独立构造线缆字节，覆盖无词页面、同URL重定向别名、不同URL
的docId碰撞拒绝、整文档分批，以及真实TCP传入非法分词语义时不能写入JVM。
分词请求与文档写入按15MiB有效载荷分组，单文档不拆开；为16MiB帧留出协议开销。

`bash apps/websearch/jvm/test.sh`新增真实actor契约：旧词消失、其它文档保留、
重复提交排名不变、完整截断／坏尾部／重复id和词／零词频全批拒绝、无Flush
重启恢复旧快照、Flush后重启恢复完整新文档、空词替换、unsigned词频，以及
替换旧客户端创建的增量条目。`--legacy-split`仅用于重现旧爬虫拆分更新的
失败反例，预期非零，不是可选通过门禁。

`apps/test_support/test_python_components.py`与真实Ray入口
`apps/test_support/run_python_components.py`同时检查重复分词删除旧词、空内容
清空旧词集、失败不改变先前统计和重复docId拒绝。完整文档替换是安全重放的前提；当前持久任务恢复另由下述独立入口验证。


`python3 apps/websearch/tests/run_document_replay.py`在真实网关上先通过旧接口
写入并Flush一份旧页面，再让当前爬虫以`--keep`更新同URL，要求旧词消失、
精确新URL可检索；最后重启整个JVM并验证结果和分数不变。它要求当前预构建
应用及组件制品，CI在实际启动器构建之后执行。完整业务入口另含9页语料，
覆盖满8页提交与1页尾批，避免只用小批次通过来代表整条批处理路径。


BFS 门禁 `python3 apps/test_support/run_frontier_bfs.py` 使用两个明确授权的
IP origin（127.0.0.1 与 127.0.0.2，各自绑定随机端口），保留跨host抢跑反例。
Suite 默认限制到自己的语料；BFS 显式传入自己的两个 origin，空范围不允许。
实际请求必须恰好60个不同页面、按20+20+20分层，20个根节点保持输入FIFO；
判据使用`require()`，普通Python和`-O`都有效。测试需要Linux的127/8回环支持、
当前应用及三个网关制品。它不依赖localhost私网DNS例外，也不开放全网抓取。

原错误范围导致启动退出2的失败证据保存在Batch429的`linux/bfs-before.log`。
Batch430增加多origin页面/robots重定向、未授权端口及混合IP/域名的DNS拒绝
回归；移除层级屏障的错误变体必须在抓完60页后被顺序断言检出。
BFS顺序不单独证明崩溃恢复；持久任务恢复由下述独立入口验证。


`python3 apps/websearch/tests/run_frontier_recovery.py`验证真实SIGKILL：已Next并开始
HTTP，以及Push/Documents/Checkpoint/Clear真实回执产生但尚未到达应用时中断。
重启全部网关后须恢复原任务；完成后的再次重启不得重复抓取。另验证同目录
serve-only不能抢占爬虫、失败重试、重定向原始任务身份、发现深度、范围缩小
拒绝、索引回滚/外来epoch拒绝及旧去重表迁移。`--only-semantics`仅运行后六项，
用于错误变体反证；CI运行完整入口。所有判据使用require，优化模式不跳过。

旧业务测试把失败URL永远跳过当成正确行为；现在keep场景保留原深度并给出
三次尝试预算，精确验证失败任务重试与新种子入库，已提交页面不重复获取。
错误回执注入点从FlushAck更新为CheckpointV2Ack，并补上CrawlStateV2与错误epoch。

`python3 apps/websearch/tests/run_index_history.py`用真实HTTP及网关构造旧备份，
通过真实Flush回执使同epoch编号追平/超过本地完成点，要求在HTTP之前拒绝恢复。
恢复匹配索引后不重抓；旧无SHA256日志迁移须完整重放且仅迁移一次。CI运行该入口。
JVM另检验把正确旧快照混放到错误分支不能伪装祖先，真实actor拒绝坏56字节协议，
并验证版本3迁移、回执丢失后合法祖先继续、版本4逐字节截断和CRC损坏。

`python3 apps/websearch/tests/run_component_identity.py`运行两个独立数据目录，
真实验证后者无法替换前者的组件/索引，serve-only同样不能抢占；实际SIGKILL
后保留网关，先serve-only再完整恢复，检查全部旧角色意图仍可退休。通过旧
业务地址发送Push/Tokenize/Clear，以及旧地址Stop，必须得到拒绝/不存在，
新组件继续运行且文档/分数不变。另检验旧固定名称search由独立管理者持有时
不会被新应用停止。观察器从实际部署回执读取路径；失败的部署则从持久意图
取得精确待检查地址。此入口接入CI，保留日志与结果，不跳过失败。

原生命周期TCP桩不再把固定名称当作正确行为；它核对持久身份、每次运行地址、
退出时精确地址Stop，新增错误Deployed路径回执拒绝。原注册握手身份判据保留。
旧文档回放夹具在写入旧格式数据后明确停止自己部署的search，再启动待测应用；
不能依赖应用擅自停止不属于它的夹具来完成迁移。

身份入口还会截住真实JVM Deployed回执，在客户端收到前SIGKILL应用。网关保持
运行，重启必须用已落盘的确切地址清理成功部署但未确认的实例，再正常爬取。

## 共享网关上的独立索引

`python3 apps/websearch/tests/run_index_isolation.py`使用当前debug应用、真实三网关
和本地HTTP语料。先由独立TOML生产者部署含引号/反斜线/中文的路径，核对真实
.search.lock和index目录；错误配置必须拒绝且不创建默认目录。随后两个应用使用
不同本地日志与远端索引，同网关同时服务，精确验证各自URL和BM25分数。另覆盖
同索引竞争拒绝、SIGKILL后存活网关上的serve-only/完整恢复、保留模式改路径拒绝、
整个JVM进程重启后两索引回放。请求计数、组件独立状态及磁盘位置同时作为判据。
失败数据保留，require断言在Python -O下仍有效；CI已接线，不等同托管CI已执行。
JVM SearchConfigContracts补充完整TOML字符串、UTF-8、schema和预算边界。

## 真实 manifest 宿主

`python3 apps/websearch/tests/run_manifest_integration.py` 使用当前 debug 应用和已经
构建的网关/业务制品。测试本身不启动共享网关，必须由应用的标准 host 创建。
覆盖默认清单、改名/版本/参数/接线、CLI 覆盖、独立索引位置、仅检索精确分数回放、
抓取中 TERM 持久提交、KILL 后 guardian 退场与 keep 恢复、装配中 TERM/KILL、
后续制品加载失败时回滚早期网关。业务只访问本次拥有的 HTTP corpus。

每个运行记录 PID+创建时间、真实后代与监听端口；应用退出后先检查资源已释放，
再执行夹具的兜底清理，不能用测试清理掩盖产品泄漏。搜索结果、分数、请求计数、
部署状态/版本及实际磁盘路径共同作为判据。失败日志与数据保存在独立
`/tmp/ws-manifest-*`；`--report` 可另保存结果。CI已接线，托管CI是否执行须另行确认。

Rust 的 application 测试验证清单配置合并和 factory 参数校验，CLI进程测试验证
无副作用预检；受控TCP对端延迟Stop，验证 crawl和serve-only两种路径在业务任务
结束后仍持有目录锁。它们不能替代真实跨语言链路测试。

## 生产容量下的日志压缩

`python3 apps/websearch/tests/run_journal_compaction.py --report /tmp/websearch-journal-result.json`
需要已构建的debug应用和真实三网关。独立夹具把真实检查点/任务状态编码为合法
旧版校验链，使用重复重置意图和合法JSON空白填到256 MiB上限前1字节，未修改
产品预算。实际验证新爬取、续爬及压缩后SIGKILL恢复；另构造能放入旧Reset但无法容纳
Begin的2 MiB边界，要求在Clear之前拒绝且已有URL/分数保持。使用精确URL、HTTP次数、
待爬深度/完成状态、索引历史与文件大小判据。保留旧日志硬链接，要求其SHA不变
且新日志inode改变，防止用原地截断伪装原子发布。失败日志和所有输入保留。

Rust契约覆盖分块状态/去重、正常4 MiB周期触发、未封口快照拒绝、合法checksum
但错误结构、压缩后重新编码跨越单条上限、IO失败不可续写、旧无摘要状态迁移，以及压缩/Reset/Begin发布各阶段
测试子进程无析构退出后的重开。它们验证进程中断，不冒充硬件断电或NFS测试。
原历史回放夹具显式去除格式2封装后生成真正的版本1记录，仍检查保守重放全部
完成URL；不能仅移除digest字段却把新快照结构称为旧格式。


索引版本5压缩：`python3 apps/websearch/tests/run_index_compaction.py --report /tmp/websearch-index-compaction.json`
使用真实Rust/Erlang/Ray/JVM和独立生成的合法10万个版本4文件（完整输入另存gzip归档），
分别验证续爬完整校验/迁移不重复抓取、显式新爬取、128次Checkpoint后的祖先证明和新JVM恢复。
在真实后台回收已删除首文件而末文件仍存在时SIGKILL JVM，要求残留数量严格介于0和10万；
恢复后最新头字节、搜索分数、目录外硬链接源均不变，物理文件最终剩1个。
`--only-legacy-migration`仅运行迁移定向探针，报告scope明确区分，不能算完整四场景通过。
JVM契约另覆盖摘要格式/每字节截断/完整CRC坏祖先/真实10万摘要上限、取消与worker join。
旧分支恢复测试在回收前保存独立快照备份；不能靠删除最新文件假定旧文件永远存在。

## 原生包与安装后运行

`test_host_package.py`验证完整打包清单、依赖顺序、相同输入的确定性输出、独占发布、接收容量限制，以及 Cargo 实际产物回执和构建失败传播。可在 Python `-O` 下运行。

`run_portable_host.py --archive FILE --report FILE`用生产接收器安装后改名，再运行真实业务；`--release DIR`测试已有只读发布目录。`--hidden-checkout /repo`明确要求指定构建目录不可见；隐藏 checkout/Maven 缓存和只读挂载由外层隔离容器提供，普通 `--archive` 本身不证明文件系统隔离。CI执行release打包和迁移后业务，不等于已执行完整OS/架构矩阵。

判据包括真实爬取和精确搜索分数、双实例独立索引/编译目录、全新JVM回放、SIGKILL后全部观察到的进程和监听退出、恢复不重抓已提交页面、显式runtime优先级及缺失runtime无存储副作用、真实业务编译失败的清理、安装文件摘要不变和无新增文件。含被robots拒绝链接的日志恢复时应重新读取robots，不能错误断言所有HTTP都为零；已全部完成的独立日志则要求零HTTP。测试保留失败报告，不将缺依赖记为跳过。
