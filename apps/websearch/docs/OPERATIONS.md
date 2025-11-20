# Websearch 运维手册

> 源码对齐日期：2026-11-06。所有命令默认从仓库根执行，占位路径和 APP_IP 必须替换。本文描述当前代码行为，不宣称本次已执行全部集成或部署验收。

## 1. 环境与构建

源码开发准备 Rust/Cargo、CPython 3.11、JDK 21、Maven、Erlang/OTP、make、unzip。根 README 记录当前 Rust 验证工具链为 1.98.1；镜像使用 OTP 27、Java 21、Python 3.11。不要将旧文档的 Java 11/OTP 24/Python 任意版本当作当前统一验证矩阵。

```bash
git submodule update --init --recursive
rustc --version
cargo --version
python3 --version
java -version
mvn -version
erl -noshell -eval 'io:format("~s~n",[erlang:system_info(otp_release)]),halt().'
jar --version
```

开发环境可用 venv 安装 Python 协议包、Ray 与 jieba，具体步骤见[入门指南](../../../documents/01-getting-started.md)。需要固定部署依赖时，在 CPython 3.11 专用环境中使用仓库锁定安装器：

```bash
source .venv/bin/activate
PYTHON="$PWD/.venv/bin/python" sh deploy/images/install-ray.sh
python3 -m pip install --no-deps -e ./interop/python
python3 -c 'import ray, jieba'
```

先创建 .venv。安装器使用工具与运行依赖锁、哈希和 pip check；版本与平台范围见[镜像说明](../../../deploy/images/README.md)。开发 extras 的版本范围不等于固定依赖验收。

构建顺序：

```bash
make build-jvm build-erl
CARGO_BUILD_JOBS=2 apps/websearch/build.sh
```

build.sh 默认执行锁定依赖的 release Rust 构建、Scala 业务编译、frontier erlc 和 Python 导入自检。使用 `build.sh --profile dev` 则生成开发制品。它不会自动安装系统工具，JVM 业务编译依赖此前准备的通用网关 jar 与 Maven 库。

主要制品为 Rust websearch、通用 JVM 网关、websearch-jvm-1.0.0.jar、frontier.beam 和 tokenizer.py。业务 jar 打包 jieba 及词典、配置解析依赖和许可证；仍依赖 Java、Akka/Scala 与通用网关宿主，不是完整自包含运行时。

JVM build.sh 校验配置解析依赖清单/SHA256，并使用固定 jar 时间戳；整个项目构建是否字节可复现还依赖编译器、环境和其它输入，不能仅凭这一项就声称全链可复现。

## 2. 本机 direct 与 registry

建议先按[本地语料实验](../../../documents/01-getting-started.md)运行，无需依赖公网网页。以下给出一次受预算限制的公网开发运行：

```bash
apps/websearch/run.sh --insecure-development \
  --data "$PWD/.local-websearch" --pages 100 --port 0 https://example.org/
```

反拨入口：

```bash
apps/websearch/deploy/run-registry.sh --insecure-development \
  --data "$PWD/.local-websearch-registry" --pages 100 --port 0 https://example.org/
```

两个入口均执行 launcher.py。默认先构建当前 release；失败立即退出，不回退旧产物。WS_PROFILE=dev 选择 dev；WS_SKIP_BUILD=1 明确测试已有产物，只有此时允许 WS_BIN。

启动器为每次运行创建独占 WS_RUN_DIR，拒绝已有目录；保存轮转日志和 result.json。未指定 data 时使用本次目录下 data。keep/serve-only 要求显式 data 或 WS_DATA，不能悄悄选择新空目录。

未显式 index-data 时，启动器把同数据根传给 JVM WS_DATA；显式远端索引选择由应用配置和持久意图协调。原始二进制的 data 默认 ./data，不应与启动器规则混淆。

网关与 registry 使用端口 0。以 `WEBSEARCH_READY=http://127.0.0.1:实际端口` 为访问地址：就绪必须同时满足拥有的进程存活与实际 /stats 有效响应。单个日志标记或某个旧监听者都不够。

原始 HTTP listener 绑定 0.0.0.0，READY 的 loopback URL 不表示仅本机监听。受控网络/防火墙决定暴露面。启动器不自动增加 insecure-development；此选项显式选择未认证开发连接，不提供生产安全。

### 环境变量

- WS_PROFILE：dev/release 构建选择。
- WS_SKIP_BUILD、WS_BIN：明确使用已有制品。
- WS_RUN_DIR：尚不存在的独占运行目录。
- WS_DATA：明确持久化数据位置。
- WS_PORT、WS_SITES、WS_DEPTH：CLI 默认值，显式 CLI 优先。
- WS_STARTUP_TIMEOUT：默认 180 秒就绪期限。
- WS_BUILD_TIMEOUT：默认每项 1200 秒构建期限。
- WS_TIMEOUT：可选整个运行期限。
- WS_RAY_HTTP_PROXY：只影响 Ray 环境，不改变 Rust 爬虫禁用环境代理的策略。

日志单文件约 2 MiB 轮转、保留两个备份；Ray 临时目录使用短私有路径。检查本次实际文件名与 result，不把共享固定 app.log 作为证据来源。

## 3. 数据保留与查询重启

```bash
apps/websearch/run.sh --insecure-development \
  --data "$PWD/.local-websearch" --serve-only --port 0
```

serve-only 隐含保留、不接受 seed，只启动 JVM；恢复相同索引后提供 HTTP。普通无 keep 抓取会重置索引，不是查看旧数据的安全替代。

keep 恢复未完成任务并保留已发现集合：

```bash
apps/websearch/run.sh --insecure-development \
  --data "$PWD/.local-websearch" --keep --pages 100 --port 0 https://example.org/
```

若原来限定 crawl-origin，续跑应保留对应范围及足够 maxdepth，否则拒绝不在范围内的待处理任务。已完成 URL 不保证重新抓取，keep 不是定时 freshness 更新器。

## 4. 原始外部网关模式

以下假设外部网关已经由其管理者启动，业务制品位于目标根。应用不会替这些手工外部网关进程提供完整监督。

```bash
./target/release/websearch --insecure-development \
  erl=10.0.0.11:19871 ray=10.0.0.12:19873 jvm=10.0.0.13:19872 \
  --node-root /opt/parrot/websearch-v1 \
  --data /absolute/local-journal --index-data /var/lib/websearch-v1 \
  --pages 100 --port 0 https://example.org/
```

direct 要具体 IP 和非零端口，IPv6 使用 `[::1]:PORT`。node-root 是各目标网关看到的业务制品根；index-data 是 JVM 机器的绝对数据路径；应用 data 是本地日志/身份，不应建议直接通过共享盘并发运行。

serve-only 可只提供 jvm 地址，并沿用同 data 和已绑定索引。不要直接拼接构建机 cp.txt 的绝对路径作为另一台机器的 classpath，使用下面的分发包。

## 5. 跨机器 registry 与分发

先选择各目标都尚不存在的新发布目录：

```bash
apps/websearch/deploy/distribute.sh user@host-a,user@host-b,user@host-c \
  --root /opt/parrot/websearch-v1
```

默认 release，--profile dev 可选。包包含 JVM 网关及完整依赖/相对 classpath、Erlang 源码、Python 协议和业务、目标启动脚本；不依赖目标 Maven 缓存。Erlang 在目标 OTP 编译，失败不能启动旧产物。

目标通过 SSH stdin 收包，校验归档与文件 SHA256，拒绝链接、路径穿越、重复/缺失成员。归档最多 512 MiB、内容 1 GiB、最多 10000 文件，现有目标不覆盖。目标目录归安装用户私有，需要明确服务账号权限。

SSH 发布后断连不证明安装未发生，应检查目标 MANIFEST。多机分别发布，不提供跨机器事务、自动回滚、签名或任意断电持久性。WS_DISTRIBUTE_TIMEOUT 默认每主机 300 秒，范围 1..86400。

应用端：

```bash
./target/release/websearch --insecure-development \
  --bind 0.0.0.0:19870 --wait 60 --port 0 \
  --node-root /opt/parrot/websearch-v1 \
  --data /absolute/local-journal --index-data /var/lib/websearch-v1 \
  --pages 100 https://example.org/
```

各目标分别前台执行自己的角色，替换 APP_IP：

```bash
/opt/parrot/websearch-v1/start-remote.sh erl APP_IP:19870 --root /opt/parrot/websearch-v1
/opt/parrot/websearch-v1/start-remote.sh ray APP_IP:19870 --root /opt/parrot/websearch-v1
/opt/parrot/websearch-v1/start-remote.sh jvm APP_IP:19870 --root /opt/parrot/websearch-v1 --data /var/lib/websearch-v1
```

由明确的服务管理器托管这些入口。它们不是自动重启及完整进程树监督系统。注册等待只认所需身份的活跃已握手直连，serve-only 只需 jvm-search-1；地址已知或其它节点数量足够不能替代。

跨机未认证 TCP 仅供显式开发演练。本文不提供已完成认证/TLS的生产发布方案。

## 6. Compose 容器

先阅读[镜像说明](../../../deploy/images/README.md)，准备 Docker/BuildKit、cargo-zigbuild/Zig 与匹配 Rust musl target。包装器默认 linux/arm64；AMD64 必须明确：

```bash
WS_PLATFORM=linux/amd64 apps/websearch/deploy/compose.sh up \
  --insecure-development --pages 100 https://example.org/
apps/websearch/deploy/compose.sh ps
apps/websearch/deploy/compose.sh stats
apps/websearch/deploy/compose.sh search Parrot
apps/websearch/deploy/compose.sh logs app
apps/websearch/deploy/compose.sh down
```

同一平台选择控制 Rust 二进制与 app 镜像，ELF 检查验证实际架构；其它网关镜像按其平台规则构建。跨架构执行仍需模拟器或宿主支持。

默认宿主 loopback 8188、容器内 8080；以实际发布地址为准。up --port N 同步监听与映射目标。基镜像默认官方源，仅显式 WS_REGISTRY_PREFIX 选择其它源。

Compose 要求容器监听 --port 为 1..65535，不接受原始 CLI 的 --port 0；需要随机宿主发布端口时使用 WS_WEB_PORT=0。基础 Compose 已显式包含 insecure-development，包装器合并单次覆盖选项，不应重复提供用户选项。

框架镜像提供运行时，业务文件挂载，Rust 二进制 COPY。file URI 必须是容器内路径。Ray 使用 init/subreaper 回收孤儿进程，不替代业务实例租约与停止流程。

up 验证真实目标平台 CLI，之后启动并等待应用健康；失败保留容器供诊断。down 先停应用 75 秒宽限，再停网关；down -v 会删除持久卷，不能作为例行无损停止。

容器 127.0.0.1 是容器本身，不是宿主教学网站。主机业务测试不能证明容器部署；独立门禁为 `python3 scripts/verify_websearch_images.py`。

## 7. 监控与故障定位

记录版本/dirty 状态、实际制品摘要、数据绑定、运行模式、端口、进程、退出码及本次证据目录。使用打印的实际 URL 访问 /stats，检查 docs/terms/postings/queries/segments。

/docs 按站点浏览，/terms 显示 df；/search 是 HTML，不是 JSON API。搜索 p 从 1 开始且限制十页；浏览页 p 从 0 开始。/stats 正常不证明所有抓取任务已完成，物理 segments 也不等于逻辑增量段数。

Ray 只统计本次实例处理的文档，JVM 可恢复旧库；keep/serve-only 后两者统计不要求相等。查询正确性要检查精确 URL 与分数，不只看 docs>0。

### 常见故障

- 旧端口 404：先看本次 READY 与进程，不推断一定是 Websearch 的回复。显式端口可自动顺延，别终止无关监听者。
- 网关连接拒绝：查本次网关日志、端口和身份；Web 端口不是 Wire 端口。
- jieba/Ray 导入失败：在启动所用 shell/venv 检查解释器、包和平台支持。
- JVM 类/词典缺失：重建通用网关与业务 jar，核对实际 URI、包内资源、classpath；不能只修改源码后继续用旧 jar。
- Erlang badfile：让目标 OTP 编译源文件，核对工具与镜像版本。
- 容器 exec format：检查 WS_PLATFORM、实际 ELF 与宿主执行支持。
- 最新快照损坏/锁冲突：保留文件，修复数据或等待真正持有者退出，不能删锁抢占。
- 搜不到词：按抓取请求、robots、抽取、Terms、索引、查询分词逐段检查，不盲目延时。
- 本机 URL 被拒绝：教学用显式 IP crawl-origin，保证协议/端口匹配，不要移除全部安全校验。
- 403：当前目标可能拒绝爬虫，使用受控语料定位；不保证某个公共站点永远可抓。
- 索引空：检查冷启动、不同 WS_RUN_DIR/data/index-data 或已删除卷，不只检查 WS_DATA。
- ACK 数量/类型错误：按真实协议和版本排查，不把错误转空列表伪装成功。

## 检索存储与任务恢复

Rust data 保存 journal、identity、intent，JVM index-data 保存完整快照。原始 CLI 默认 data 不自动修改远端根；启动器默认协调 WS_DATA；manifest 首次缺省将索引绑定本地 data。持久显式索引选择后，keep/serve-only 不接受不同路径。

完整文档替换的 DocumentsAck 只确认内存。当前应用按约 60 秒和收尾发送带 epoch/generation/SHA256 的 CheckpointV2，收到并验证确认后同步任务完成。不为每八页额外写完整快照。

Discover 在 Push 前同步；未完成、HTTP 失败、出队未完成和已写未确认的任务在下次 keep 重放。已完成任务保留去重，不保证重新抓取。epoch/代际/历史摘要不符时拒绝恢复，不能编辑编号绕过。

最新 v5 为完整索引及当前 epoch 摘要历史；新头同步后后台清理旧文件，segments 可暂时大于 1。旧 v4 首次迁移须保留并验证完整父链，v5 恢复后按实现继续清理。不要手工删除仍需校验的祖先。

快照提交执行临时文件、file force、原子改名、目录 force，存储错误后组件拒绝继续服务直到修复/重部署。最新损坏不自动回退；同步失败可能已发布，不能保证无效果。

续爬先 PrepareIndex：每 100ms 轮询，总期限 600 秒，单 RPC 5 秒，可响应停止；serve-only 不触发旧链准备，冷重置/未完成 Reset 走 Clear。准备与回收 worker 退出后才释放 search 锁。

限额：快照 256 MiB、字符串 16 MiB、文档/词项各百万、posting 1600 万、历史摘要十万；日志 256 MiB、单记录 2 MiB、任务十万。它们不是推荐容量或无限长期运行承诺。

### 备份与升级

正常停掉全部写入者，成套备份 journal、identity、intent 和对应 JVM 索引，记录版本/路径/摘要。在独立恢复目录用 serve-only 验证精确 URL/分数，再考虑 keep。

活跃时随意复制两侧不是一致备份协议。新 Rust、Ray 业务与 JVM jar 应配套升级；旧 JVM 不支持新业务消息时明确失败，不回退旧增量路径。旧读取器不支持 v5 快照/格式 2 日志，降级需升级前成套离线备份。

摘要不是签名，文件锁不是分布式租约。不承诺 NFS、多机克隆身份 fencing、跨存储原子事务或任意掉电保证。

## 停止时的数据与连接边界

INT/TERM 停止新派发，完成当前批次、尾批索引、最终检查点和 HTTP 排空，再退休组件和关闭连接。应用总收尾期限 60 秒；确认失败或超时非零，不能输出 shutdown complete。

HTTP 监听停止，已有请求各有 15 秒期限，最多 128 连接。启动器给应用 65 秒，Compose 给 75 秒，再停网关。启动器因信号可按 128+signal 退出，其 result 中应用子进程仍应独立记录最终状态。

父进程 SIGKILL 可由 guardian EOF 回收拥有进程组，但不能保证业务最终提交；按 journal 恢复。不要用全局 pkill/ray stop 清理本次任务。

## 独立应用的组件所有权与退出

组件名称为 ws-identity-role-run。应用持 `.crawl.lock` 读取 remote.components，逐角色停止确切旧地址，确认后同步新意图再 Deploy，并核对回执路径。新身份不会主动抢占旧固定 /user/search。

正常/失败退出都对本次尝试部署地址发 Stop，即使部署回执丢失；各节点并行回收，COMPONENT_NOT_FOUND 幂等接受，其它错误保留。网关进程仍归外部所有者，不退休无关组件。

remote.components v2 最多三个角色、可选索引路径，上限 32768 字节，兼容 v1。serve-only 保留未参与角色意图。不要删除或重造 identity 绕过损坏，这会丢失恢复地址。

网络分区、强杀和迟到 Deploy 仍可能留残余，需依据身份与意图人工核对；不是自动失联租约或事务回滚。数据锁保留到组件和传输退场后。

## 本地 manifest 宿主

```bash
./target/release/websearch --insecure-development \
  --manifest apps/websearch/websearch.app.toml \
  --data /absolute/manifest-data --pages 100 --port 0 https://example.org/
```

这是链接业务 factory 的宿主，不通过 run.sh 再包一层。CLI 显式参数覆盖清单 settings；制品与清单内相对路径按清单目录解析，CLI 路径按当前目录。

serve-only 只装配 crawler/search，使用保存的索引选择；保留模式不能变更路径声明。旧外部网关不会被私有清单宿主自动接管，旧 search 持锁需先由其管理者退休。

validate-only 校验 schema/配置/路径，不绑定服务或修改数据，不证明制品成功加载。真实装配必须收到对应成功回执并验证实际业务，不将部分网关就绪当整体成功。

## 抓取日志压缩与格式升级

格式 PRWSJ2 的 Snapshot/RestoreTasks/Seal 保留任务顺序、深度、完成和索引证明。普通追加增长约 4 MiB 或临近文件限额时压缩，完成任务仍占去重额度，不会丢任务来突破十万限制。

临时 .crawl.journal.next 同步、rename、父目录同步，锁与文件替换独立。失败必须重开，不能继续写旧句柄。只有完整 Seal/校验链才接受压缩结果；普通追加尾部截断可退到最后完整记录，完整坏记录不能跳过。

新冷抓取先原子发布 Reset，预留 Begin 预算，再执行远端 Clear，确认后 Begin。旧无摘要日志保守全重放；dedupe 迁移为深度 0。版本 1 可读取并迁移，旧程序不读格式 2。

## 可迁移的原生 manifest 宿主

```bash
CARGO_BUILD_JOBS=2 python3 apps/websearch/deploy/package_host.py \
  --profile release --output /absolute/new-output/websearch-host.tar.gz
```

输出必须是新文件，支持 CARGO_TARGET_DIR，构建失败不使用旧二进制。包包含 bin/websearch、manifest、业务文件、网关与完整相对 JVM 依赖、MANIFEST/RELEASE 元数据和安装接收器。

从可信渠道获得接收器及归档 SHA256，然后安装到新目录：

```bash
python3 /trusted/install_bundle.py /absolute/new-release ARCHIVE_SHA256 \
  < /absolute/new-output/websearch-host.tar.gz
/absolute/new-release/bin/websearch --insecure-development \
  --manifest /absolute/new-release/websearch.app.toml \
  --data /absolute/writable-data --pages 100 https://example.org/
```

发布可移动/只读，data 必须外置可写。runtime-root 显式配置优先，否则使用安装 executable 相邻 runtime；安装树损坏失败，不回退开发源码。开发目录才使用构建时源码路径。JVM classpath 来自所选 runtime/cp.txt，本宿主不采用环境 PARROT_JVM_CLASSPATH。

网关 Erlang 编译到实例私有临时目录。业务包已有 app.beam 时保留包及依赖，否则编译选定 app.erl，不自动发现完整多模块工程。编译失败/超时禁用旧产物，目录保留到进程真正退休。

仍需兼容系统/CPU/库、Python/Ray/jieba、Java、Erlang/erlc；不是跨平台自包含镜像。带组退休验证的本机进程所有权支持 Linux/macOS，其它平台启动前拒绝。SIGKILL 后 guardian 可回收进程，但临时编译目录可能因无法执行析构而残留供诊断。

### 原生宿主的网关进程组配额

manifest 的 --gateway-process-limit 默认 64。完整抓取同时拥有三个网关组，需要至少三个额度；serve-only 只需一个。编译器也计费，真正退休后额度才复用。

启动取消/退出中继续计费，无法证明退休则保守隔离名额。此为共享宿主的 guardian 组额度，不是总子进程、CPU、内存或磁盘限制。见[应用资源准入](../../../docs/APPLICATION_ADMISSION.md#实际网关进程组准入)。

## 8. 参数与测试参考

原始 CLI 的权威帮助在 cli.rs/--help，完整中文参数解释见[使用者参考](../../../documents/10-reference-and-troubleshooting.md)。覆盖 manifest、runtime-root、gateway-process-limit、pages、sites、maxdepth、port、data、index-data、keep、serve-only、bind、wait、node-root、crawl-origin、insecure-development、validate-only、help 与 --。

参数错误先于数据修改返回 2，帮助返回 0；Web 绑定或注册启动失败返回 3。data 默认 ./data，pages 50000、sites 100、maxdepth 10、port 8080、wait 60 秒。具体组合限制以代码检查为准。

本机门禁为 tests/run_all.sh；Python 发现测试、jvm/test.sh、run_launcher_integration.py、run_manifest_integration.py、run_distribution_integration.py、run_shutdown_integration.py 和组件恢复测试提供不同范围证据。

完整前提与边界见[测试说明](../tests/README.md)。主机成功不代表容器/真实 SSH/异构平台/性能已通过；本次文档对齐没有执行这些全链门禁。

## 9. 清理

先正常停止并检查本次 result/退出码，确认全部持有者已退出，保留成套备份后再清理明确目录。外部网关交原服务管理器，构建期间不要清理产物。不要删除活跃锁、手工拼接不同历史快照或批量终止同名无关进程。
