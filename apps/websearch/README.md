# websearch —— 真实搜索引擎（五运行时全分工）

与 `apps/crawler-lab`（合成页回归基线）同族架构，面向**真实互联网**：

| 运行时 | 组件 | 职责 |
|---|---|---|
| Erlang/OTP | `frontier` | 真实 URL 去重（ETS）+ BFS 调度 |
| Rust/Parrot | `crawler` | reqwest 真实 HTTPS 漫爬 + HTML 抽链/抽文 + 编排 + 百度式 Web 页 |
| Ray/Python | `tokenizer` | **jieba 中文分词**（开源）+ 词频统计 |
| Akka/JVM | `search` | **jieba-analysis 查询切词** + 倒排 + BM25 + 段文件落盘（重启回放） |
| TS Lite | 浏览器 | 零依赖访问 `http://localhost:8080` |

四节点可分布任意机器（组网/部署跨网络——见「跨网络部署」节；本 README 快速
上手只演示单机 direct 形态）。

## 📚 文档

| 文档 | 内容 |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 技术详细设计——架构图/模块图/数据流/全部时序图/wire 协议/设计决策 |
| [docs/PARROT_USAGE.md](docs/PARROT_USAGE.md) | parrot 能力全景——用了什么/为什么用/引擎内部原理（组网·握手·codec·RPC·部署·心跳·寻址） |
| [docs/OPERATIONS.md](docs/OPERATIONS.md) | 运维手册——编译/部署/parrot 部署机制沙盘推演/监控/故障排查 |

本 README 只保留快速上手；技术细节见上述三文档。

## 快速上手

```bash
# 构建（rust + jvm fat jar + erl beam + 依赖自检）
apps/websearch/build.sh

# 一键全链（默认构建后起三网关 + 漫爬 + Web 服务）——direct
# 不给种子自动启用内置 47 条多样化种子集；默认目标 100 站 × 每站深度 10
apps/websearch/run.sh --port 8080

# 也可以指定种子与目标
apps/websearch/run.sh https://www.runoob.com --sites 100 --maxdepth 10

# 仅检索（独立运行——回放已落盘索引）
apps/websearch/run.sh --serve-only --port 8080 --data /absolute/path/to/saved/index
```

## 本地清单宿主

在仓库根目录构建业务和通用网关后，可由应用自身按清单装配：

```bash
CARGO_BUILD_JOBS=2 apps/websearch/build.sh
make build-jvm build-erl
./target/release/websearch --insecure-development \
  --manifest apps/websearch/websearch.app.toml \
  --data /absolute/websearch-data --pages 100 https://example.org/
```

该入口启动本次独占的 Erlang、Ray、JVM 网关，并把真正的抓取/HTTP 工作交给
`websearch.crawler` Props actor；三个业务引用来自清单 wiring。清单的组件名称、
版本、依赖及 settings 生效，显式 CLI 参数覆盖清单；相对制品和 settings 路径
相对于清单文件。缺省索引选择绑定到本地数据目录并持久保存；可用
`--index-data /absolute/index` 分离索引目录，或用 search 的 `config.data_dir` 声明。
两个声明冲突时报错，显式 `--index-data` 可统一覆盖。

重启加 `--serve-only` 时只启动 JVM 与 crawler，并回放相同索引。
不要同时传入 direct 网关地址或 `--node-root`；这些参数属于外部网关运行模式。
`--validate-only` 检查清单/schema/配置和制品路径，不绑定端口或创建数据，
也不代表制品内容已成功加载。真实业务与资源回收门禁为
`python3 apps/websearch/tests/run_manifest_integration.py`（使用已构建 debug 应用）。

这是链接了业务 factory 的 `websearch` 宿主，不是通用 `parrot-app` CLI 插件，
也不是 Dylib/Wasm 适配。`run.sh`、registry 和容器入口继续采用原有部署方式。
清单模式开发运行需要明确传入 `--insecure-development`；它不提供生产认证部署。

## 本机启动与索引所有权

两个本机入口`run.sh`（direct）和`deploy/run-registry.sh`（反拨）现在共用受监督的启动器。默认先构建release应用和网关制品；任一步失败立即退出。`WS_PROFILE=dev`选择开发构建，成功后只使用Cargo实际报告的可执行文件，支持Cargo自定义target目录，不回退到旧release。`WS_SKIP_BUILD=1`明确测试已有制品；只有此时可用`WS_BIN`指定应用路径。

每次打印独立`WS_RUN_DIR`，保存有界轮转日志和终态`result.json`；已存在的目录会被拒绝。默认数据放在该运行目录的`data`中。要复用索引，显式传`--data /absolute/index`或`WS_DATA`；`--keep`与`--serve-only`要求明确本地数据路径；未选择`--index-data`时，启动器把同一路径传给JVM的WS_DATA。serve-only仅启动JVM。原始Rust CLI的默认数据目录语义不变。

网关和registry监听端口由OS分配。Web默认8080，可用`--port 0`分配端口；以`WEBSEARCH_READY=http://127.0.0.1:实际端口`为准。就绪要求拥有的应用和网关仍存活、实际`/stats`返回合法结构，日志或绑定标记本身不算就绪；之后继续监督网关并等待确切的应用进程。

参数值按字面传递（例如`--data jvm=index`）；混合参数中的`--help`由真实Rust CLI处理，成功后退出而不启动网关或应用服务。

默认保留应用的Local传输策略，不自动添加`--insecure-development`；调用方明确提供该选项时才透传。

`WS_STARTUP_TIMEOUT`默认180秒；`WS_BUILD_TIMEOUT`默认每项1200秒；可选`WS_TIMEOUT`限制整个运行。`WS_PORT`、`WS_SITES`、`WS_DEPTH`为对应CLI选项提供默认值，显式CLI优先。`WS_RAY_HTTP_PROXY`可单独设置Ray的HTTP(S)代理；Rust 爬虫禁用环境代理并使用自己的目标地址安全策略。

Ctrl-C或TERM先请求应用结束当前抓取批次并确认落盘，最长等待65秒，再关闭网关进程组并回收guardian；失败保留日志与数据，不使用全局pkill或共享/tmp日志。父进程遭SIGKILL时，由控制连接EOF触发guardian回收。现有crawler-lab复用同一OwnedChild实现，原类方法未改变。这是本机开发运行入口，不替代远程服务管理或完整生产发布验收。

## 运行模式总表（四模式）

| 模式 | 入口 | 形态 | 适用 |
|---|---|---|---|
| ① 单机调试 | `run.sh` | 本机三网关进程 + direct 拨号 | 开发调试 |
| ② 单机 registry | `deploy/run-registry.sh` | 网关反拨注册（生产组网形态） | 组网演练 |
| ③ 多节点模拟 | `deploy/compose.sh up` | docker 4 容器真实跨网（项目独立 bridge） | 单机模拟多物理机 |
| ④ 真实多机 | `deploy/distribute.sh` + `deploy/start-remote.sh` | ssh 分发 + 物理机网关 | 显式开发演练；生产验收未完成 |

## 跨网络部署（多节点形态）

parrot 组网与部署命令（admin-v2 Deploy）本就是跨网络的——`erl=/ray=/jvm=`
可填任意机器地址；run.sh 只是单机演示形态。三种远程形态：

### 形态 1：registry 反拨注册

应用零网关地址知识——三网关（任意机器）主动反拨注册到应用：

registry启动只接受所需身份的已握手、当前未关闭直连。静态种子、gossip地址、hub路由及其它身份的连接数量均不能代替注册；已关闭但尚未异步移除的sender也不算就绪。应用等待共用一个`--wait`期限，不再在地址入表后额外等待五秒并无条件继续；超时列出缺少直连的身份并失败退出。websearch的serve-only只需jvm-search-1，其余模式及crawler-lab需erl-gw-1、ray-gw-1和jvm-search-1。

这是启动时的本地连接观察，不是组件健康检查或持续可用租约；连接随后仍可能断开，后续部署和业务操作必须分别处理失败。没有新增自动拨号、权限授予、业务重放或网关重启能力。


```bash
# 应用侧（本机或任意机器）
./target/release/websearch --insecure-development --bind 0.0.0.0:19870 --wait 60 --port 8080 --node-root /opt/parrot/websearch

# 各网关节点（制品先经 apps/websearch/deploy/distribute.sh 分发）
apps/websearch/deploy/start-remote.sh erl  <应用IP>:19870   # Erlang 机器
apps/websearch/deploy/start-remote.sh ray  <应用IP>:19870   # Python 机器
apps/websearch/deploy/start-remote.sh jvm  <应用IP>:19870 --data /var/lib/websearch
```

### 形态 2：direct 显式跨机地址

```bash
./target/release/websearch \
  --insecure-development erl=10.0.0.11:19871 ray=10.0.0.12:19873 jvm=10.0.0.13:19872 \
  --node-root /opt/parrot/websearch --port 8080
```

`--node-root` = 各节点上制品目录（file:// uri 指向目标网关本地路径——
节点侧预置同构制品即可，与运行机解耦）。

### 形态 3：docker 多容器跨网模拟（apps/websearch/deploy 自包含）

```bash
apps/websearch/deploy/compose.sh up        # 4 容器真实 TCP 跨"机"（bridge 网）
apps/websearch/deploy/compose.sh logs      # 跟踪应用日志
apps/websearch/deploy/compose.sh stats     # /stats 快照
apps/websearch/deploy/compose.sh down      # 销毁（down -v 连数据卷）
```

app 的 compose 在 `apps/websearch/deploy/docker-compose.yml` **完备自包含**；
镜像引用框架通用运行时（`deploy/images/gw-{erl,jvm,ray,app}.Dockerfile`——
单一真源不复制）；app 制品（beam/jar/py/二进制）volume 挂载进容器，
重建 app 不重建镜像。基镜像默认来自官方源；仅显式设置 `WS_REGISTRY_PREFIX` 才使用所选代理，空值也使用官方源。Web 端口发布到宿主 `127.0.0.1:8188`，容器内默认 `8080`；`up --port N` 会同时更新容器监听和发布目标。

### 远程制品的完整性与版本目录

`distribute.sh user@host1,user@host2 --root /opt/parrot/websearch-v2`默认构建release应用及JVM网关；`--profile dev`可选择开发构建。完整包包含网关JVM jar、全部依赖jar及相对classpath、Erlang源码、Python协议包和业务组件、目标机启动脚本。重复jar文件名按classpath顺序分别保存，不依赖打包机的Maven缓存；Erlang在目标OTP上编译，任一编译失败不启动服务。

每个目标通过SSH标准输入接收独立包，校验整个压缩包和每个文件的SHA-256，再在同一父目录下发布新目录。目标机需要Python 3.9+、对应角色运行时及安装目录权限。包上限512MiB，展开内容1GiB，清单最多10000文件；拒绝归档链接、路径穿越、重复／缺失成员及校验不符。安装目录归安装用户私有，服务应以该用户运行，或由运维显式管理服务账户权限。

**现有目标目录不会被覆盖**，升级时选择新的版本目录，并将应用`--node-root`及各网关`--root`指向相同新路径。不要把不确定的失败直接当作未安装：SSH可能在发布后断开，应检查目标MANIFEST.json。安装锁仅由创建者清理，遭强杀留下的锁须确认没有安装进程后人工处理。多个主机分别安装，没有跨主机事务／自动回滚，也没有承诺断电持久性或制品签名；SHA校验通过SSH传输的预期摘要建立完整性。

目标机使用包内`start-remote.sh erl|ray|jvm HOST:PORT --root /opt/parrot/websearch-v2`前台运行，交由明确的服务管理器托管；JVM可另设`--data /var/lib/websearch`。Ray启动独立本地runtime，TERM时关闭；本入口不是自动重启或完整服务进程树监督器。`WS_DISTRIBUTE_TIMEOUT`限制每个SSH安装，默认300秒，范围1..86400。

应用制品路径现在通过合法file URI编码，空格、中文、#、?和%不会被误当作URI结构。JVM按URI解码后加载，显式非法／非本地file URI和不存在jar返回ArtifactFetch，不再静默回退父classpath；未给URI的父classpath模式保留。Maven坐标自动下载仍未实现。

当前示例中的非回环TCP组网须由调用者明确选择`--insecure-development`，仅用于显式开发演练；本批没有完成生产传输认证和真实跨机器发布验收。


分词三语言同族：Python `jieba`（原版）/ JVM `jieba-analysis`（huaban 移植）/ Rust 侧经
Ray 通道复用 Python 分词（词条布局 `len|term|docid|tf` 四方言同构）。

## 站点目标制爬取（用户裁定 3）

- `--sites N`（默认 **100**）：爬够 N 个**不同 host** 且每站深度达标才允许停
- `--maxdepth D`（默认 **10**）：每站爬取深度 ≥ D（链接逐层展开，depth+1，D+1 截断）
- `--pages P`（默认 50000，必须大于0）：抓取尝试次数硬上限；成功、失败和robots拒绝均计数，重定向链仍算一次尝试。达到额度立即收尾，不等待已排队种子全部处理，也不继续回填。
- 内置 47 条多样化种子（中文门户/技术社区/高校/国际文档站）——不给种子自动启用；
  **队列耗尽且目标未达时自动回填内置种子**（单种子死路/JS 渲染页自愈——404 也能续爬）
- Web 服务**边爬边开**（爬取启动即监听）——站点/分词表页实时可见爬取进度
- 数据目录：默认**全新爬取**（提交空索引快照后重建去重表——历次残留不混入 /docs 统计）；
  `--keep` 续爬（保留旧索引与去重表——增量模式）
- **周期段落盘**（60s 一次）：爬取中途崩溃/中断后 `--serve-only` 可回放已爬部分——
  段为全量快照式（回放幂等，docid 覆盖）
- **deploy 幂等**：网关进程长存（重启应用不重启网关）——同名组件已在位时自动
  drain + 重部署（serve-only/应用重启不再 name 冲突 panic）

参数在创建数据目录和启动网关连接前完整校验。缺值、未知/重复参数、越界数字、非HTTP(S)或带凭据种子返回退出码2；`--help`显示完整用法。`--node-root`须为目标网关上的绝对目录，网关地址使用具体IP和非零端口（IPv6写为`[::1]:PORT`）。`--wait`范围为1–86400秒。

Web监听socket在数据修改和组件部署前实际绑定，并保留到服务运行；请求端口占用时最多向后尝试20个端口，超出65535不回绕。全部不可用时退出码3，数据目录不变。`--port 0`由操作系统分配端口，以日志`Web listening on ...`为准。

预算停止不是可靠续爬事务：现有dedupe.tsv记录的是发现过的URL，未完成/失败URL的持久队列和可靠重试仍待实现，`--keep`不能保证补回所有未抓取内容。

## 确定性主机集成测试

```bash
apps/websearch/tests/run_all.sh
python3 -m unittest discover -s apps/websearch/tests -p 'test_*.py' -v
bash apps/websearch/jvm/test.sh
```

默认入口构建当前 Rust、JVM 和 Erlang 制品，要求真实 Ray/jieba 依赖，然后运行四进程业务链路。网页来自本地固定语料；本次独有标记、精确 URL 集合、抓取次数及 BM25 分数用于验收。日志、数据、结果和制品哈希保留在每次独立的 `/tmp/ws-e2e-*` 目录，退出时只清理本次启动的进程，不删除用户数据。

覆盖 direct / registry、真实404及robots拒绝、`--keep`增量、JVM进程彻底重启后的磁盘回放、冷启动清理、相同种子重爬、实际末页内容、零/极大分页参数、空库、端口占用和网关迟到。网关回放必须保持业务结果及分数一致；只检查“docs>0”或日志无panic不算通过。JVM契约测试另验证独立BM25公式、重复词频更新及多快照回放。

**此入口只验收主机进程，不包含容器矩阵。** 旧脚本的compose分支使用固定容器名、共享数据及公网抓取，已停止由本入口调用；独立容器入口为 `scripts/verify_websearch_images.py`，验证范围和准备步骤见 [镜像说明](../../deploy/images/README.md)。`--fast`保留为主机范围兼容别名；`--no-build`仅用于显式验证现有debug制品，结果会记录跳过构建。完整边界和排错见 [tests/README.md](tests/README.md)。

## 站点与分词表浏览（用户裁定 1/2）

- **站点页 `/docs`**：全部已索引站点（host 聚合、页面数降序、样例 URL）——分页 100/页
- **分词表页 `/terms`**：jieba 切出的全部词条（df 降序，点词条直接搜索）——分页 300/页
- 入口：页头导航「站点」「分词表」+ 首页快捷链接

## 真实化四诉求对照

1. **真实漫爬**：seed URL → robots.txt 礼貌策略 → 同域 200ms 节流 → depth 剪枝 →
   BFS 出链回注 frontier（百科类站点 403 反爬时换 runoob/MDN/w3school 等可爬站）。
2. **索引落盘**：`data/index/seg-*.segment`（版本5、epoch、祖先摘要、UTF-8长度字段和CRC32的完整快照）——重启恢复最新已提交快照；未Checkpoint确认的任务在恢复时重放。
3. **四服务独立**：搜集（rust crawler）/ 调度（erl frontier）/ 分词索引（ray）/
   检索查询（akka）各为独立 actor 服务；`--serve-only` 单独跑检索。
4. **浏览器访问**：内置 HTTP 服务——百度式首页 + 结果页（标题/URL/摘要/score +
   关键词高亮 + 分页 + 5s 轮询 `/stats` 索引规模）+ 站点页 `/docs` + 分词表页 `/terms`。

## 中文支持

- 页面编码：GBK/GB2312/GB18030 使用内存解码器转 UTF-8，不依赖 iconv 子进程
- 索引分词：ray 侧 jieba（停用词表 + 单字过滤）
- 查询分词：jvm 侧 jieba-analysis SEARCH 模式（同停用词表）

## 数据目录

本地与JVM可以使用不同数据根；本机启动器在未显式选择远端路径时共用同一根。

```text
Rust --data/
├── .crawl.lock
├── crawl.journal       # 带校验的发现/完成/索引确认日志
├── dedupe.tsv          # 兼容投影，不是恢复依据
├── remote.identity     # 持久应用身份
└── remote.components   # 角色部署意图与显式索引路径
JVM --index-data（未绑定时WS_DATA）/
├── .search.lock        # JVM组件持有的独占锁；不要删除或替换
└── index/
    └── seg-00000000000000000000.segment  # 完整快照及历史链
```


### 页面读取预算与本地回归

页面响应在读取过程中执行 1 MiB 上限，按 HTTP 传输解码及 gzip 解压后的字节计数。Content-Length 已声明超限时，在读取正文前拒绝；未知长度及压缩响应逐块校验，超限即返回错误并停止读取，不再将截断页面当作成功页面送入索引。恰好 1 MiB 的完整响应允许。此上限位于字符集转换之前，UTF-8 转码后的字符串、HTTP 内部缓冲与并发请求仍各有内存开销，不代表进程 RSS 限制。

`cargo test --locked -p websearch` 使用本地 HTTP 服务器验证声明超限、未知长度不停流、完整超大页、精确上限及 gzip 解压预算，另检查大 GBK 内容和 GB18030 四字节字符。无需公网、网关或搜索数据目录。这些回归不等同运行 `tests/run_all.sh` 的完整爬取/恢复场景。

robots 策略、目标地址安全和跳转边界见下节。抓取/索引恢复、网关全链路和入口调度仍需独立验收；本地 HTTP 回归不代表这些流程已经完成。


### robots、重定向与 URL 语义

爬虫使用 `ParrotWebSearch/1.0` 标识，按 `ParrotWebSearch` 产品名匹配 robots 分组；不再使用 Chrome 标识。规则地址取当前 URL 的 scheme、host、port 下 `/robots.txt`，不把 HTTP 强制改成 HTTPS。缓存按完整 origin 隔离，默认端口与主机名大小写按 URL 解析器规范化。

按 RFC 9309 合并同产品分组，只有不存在匹配产品分组时才使用 `*`；支持 Allow/Disallow 最长匹配、等长时 Allow 优先、`*`/`$`、注释、UTF-8 与百分号编码路径及查询。每份解压后规则最多 500 KiB，超限或非 UTF-8 响应暂时拒绝抓取，不把规则截断成放行结果。该预算范围内的可解析规则全部处理，没有更小的任意规则条数截断。

成功规则缓存至多24小时。除401/403/429外的4xx按文件不可用处理，允许抓取并缓存1小时；认证拒绝、限流、5xx、网络错误、超时或非法跳转暂时拒绝，30秒后下一次请求可重取。缓存最多256个origin，规则向量和字符串容量预算8 MiB，最多16个并发下载；同origin共用一个下载，取消调用会释放锁和名额。全局缓存锁不跨网络等待。没有可回收空间或下载名额时本次拒绝，不创建无界后台任务；当前调用将失败计入尝试预算，失败任务保留在持久日志，下次`--keep`重试；同一次调用不无限重试。

robots 文件允许最多5次跳转，包括跨origin跳转，所得规则只归最初origin。页面关闭reqwest自动跳转，在每一跳发送前检查目标robots，跳转循环、超过5次、非HTTP(S)或含凭据的目标均失败。robots下载全链期限5秒；一次页面抓取的12秒总期限包含规则等待、跳转和正文读取。生产模式拒绝 loopback、RFC1918、link-local、共享/保留地址和 IPv4-mapped 私网地址；每个 redirect 都重新校验，DNS resolver 会检查一次解析返回的全部地址，环境 HTTP(S) 代理被禁用。测试如需访问本地 HTTP server 必须显式使用 test-only network policy。

索引URL与相对链接基址使用实际获取的最终URL。相对路径交给URL解析器，保留尾斜杠、查询原始编码及顺序；去掉fragment和显式的utm_/fbclid/gclid跟踪键。`/dir/page`与`/dir/page/`解析`child`的结果不同，不能合并为同一资源。现有入口的200ms调度间隔还不等于每一跳HTTP请求的严格间隔。


### 检索快照的确认与恢复

JVM独占数据目录，停止actor或进程退出后释放文件锁。Flush即使只有文档元数据或索引为空也会发布完整快照：写临时文件、同步文件、原子改名、同步目录后才确认。Clear先提交空快照再清内存；持久发布后后台回收旧完整快照，恢复只采用编号最大的一个，因此旧词项不会重新出现。历史版本1–4的合法快照可读取，下一次提交迁移到含爬取epoch、当前编号及祖先SHA256摘要的版本5；坏的最新快照会报错，不自动回退到旧数据。

每个快照最多256 MiB、100万文档、100万词项、1600万posting，每个字符串最多16 MiB；旧目录最多10万个完整快照，迁移期间允许再发布一个新头；后台回收完成后保留一个完整快照。每个epoch最多保留10万个祖先摘要，达到上限后继续增长会明确失败，显式新爬取仍可Clear开启新epoch。这不是无限历史或无限规模索引；磁盘容量应持续检查。CRC32用于检测损坏，不是来源认证。写入/发布/同步失败后，actor返回错误并拒绝后续业务请求，须修复存储并重新部署恢复；同步失败可能发生在改名之后，失败回执不代表操作一定没有生效。

冷启动在网关连接、部署和Clear确认前不删除索引或去重记录；只读serve-only也不重置去重记录。任务日志只在索引Checkpoint确认后记录完成，两次写入之间中断采用至少一次完整文档重放；这不是跨存储的原子事务。测试覆盖真实JVM进程SIGKILL后的恢复，不代表已验证断电、存储控制器缓存或所有文件系统的持久性。不要在实例运行时删除、改名或替换数据目录。

### Frontier 层级顺序与升级

Frontier 以深度和单调入队序号排序，同层保留首次接受顺序；`Next` 每次只返回最浅已排队层。Rust 爬取驱动器完成该层 pending 和在途请求后才推进深层，域名节流期间也不抢跑。`PushAck` 是本次新增 URL 数，`Size` 才是队列现存数量。

生产 Frontier 每部署运行在独立 BEAM VM，每实例使用私有 ETS；两套应用的同名 frontier 可以在一个网关共存，部署连接断开不丢状态；Stop 回收状态，成功重部署创建空队列。它仍不提供跨消费者的在途完成屏障或持久队列。升级旧网关需重启，不自动迁移旧表；零参数本地兼容接口仍拒绝旧非空布局。宿主状态、准入与退出契约见 [Erlang 组件文档](../../docs/ERLANG_COMPONENTS.md)。真实回归入口为 `python3 apps/test_support/run_frontier_bfs.py`，须先构建 Rust 及三个网关。

### 限定抓取范围

使用 `--crawl-origin http://127.0.0.1:9000` 可以只抓取一个明确授权的站点，适合本地语料和内部部署。可重复指定不同 origin，例如再加 `--crawl-origin http://127.0.0.2:9001`，允许在这些站点之间发现链接和重定向；规范化后重复的 origin 会被拒绝。范围按 URL 的协议、主机、有效端口匹配；页面、robots 与它们的重定向都不能离开该范围。指定 IP origin 时允许该地址，即使它属于私网；指定域名时仍保留私网 DNS 地址拒绝策略，列表中同时包含私网 IP 也不会放宽域名解析。范围外种子在启动网络与存储前拒绝。

该模式没有内置种子回填；未给出种子时按参数顺序从每个指定 origin 的 `/` 开始。默认不指定该选项时仍使用原公开网络策略与内置种子能力。环境 HTTP 代理不能作为抓取器的访问范围限制，因为抓取器主动忽略环境代理。

独立运行的应用退出时，在业务收尾与 HTTP 排空后回收本次尝试部署的远端
组件，然后关闭客户端。`--serve-only` 只回收 search；网关进程和其它名称
组件继续存活，已提交索引快照保留。部分部署或业务失败也会尝试回收，
回收失败必须非零退出。组件使用持久应用身份及本次运行独立地址；
断连、强杀及整体停止超时不能保证无残留。详见[组件所有权与退出](docs/OPERATIONS.md#独立应用的组件所有权与退出)
和[真实生命周期验收](tests/README.md#独立应用退出后的组件生命周期)。


爬虫现以完整文档批次提交元数据和词项，替换时删除已消失的旧词；同文档重复
提交不累加词频，空词页面也会清空该文档旧posting。Ray统计按相同替换语义
更新。`DocumentsAck`是内存接受确认，当前爬虫的`CheckpointV2Ack`才确认持久化，
随后记录任务完成；旧`FlushAck`保留持久语义。升级需同时更新Rust应用、Ray业务
模块及JVM业务jar，旧JVM不支持`IndexDocuments`/`CrawlStateV2`/`CheckpointV2`时明确失败。旧增量接口仍可供原客户端使用，不能当作完整
文档替换。详见[写入与回放契约](tests/README.md#完整文档替换与安全重放)。


### 待爬任务的持久恢复

`--keep --data DIR`恢复`crawl.journal`中的未完成任务，包括原始URL、发现深度
和首次发现次序；重定向最终URL只用于索引身份，完成记录仍指向原始任务。
发现意图在Push之前同步落盘，成功完整提交且收到JVM的`CheckpointV2Ack`后才
同步记录完成。HTTP失败、已出队未抓取、已索引但未确认持久化的任务会在下次
调用重放；已持久完成的任务不重复抓取。`--pages`仍是每次调用的尝试上限。

JVM快照版本5携带16字节epoch、当前编号及有序祖先编号/SHA256摘要；Clear更换epoch。
日志保存已确认快照的完整SHA256。恢复时核对当前快照中的历史摘要，旧版本4首次迁移先校验完整父链，拒绝外来epoch、
编号倒退，以及同epoch旧备份再写到相同/更高编号形成的分叉；远端已提交而本地
完成日志尚未写入时，仍允许沿同一历史重放。日志记录SHA256链，末尾不完整追加可截断恢复，
完整记录的损坏/乱序/重复则明确失败。Rust持有`.crawl.lock`，包括serve-only，
在任何远端Stop/Deploy前拒绝同目录的并发应用；不要删除运行中的锁文件。

冷启动先写重置意图再执行Clear；中断后以相同origin范围`--keep`会完成该次
明确请求的重置。恢复任务超出本次origin或maxdepth时失败并保留任务，需使用
原范围/足够深度或明确开始新爬取。旧`dedupe.tsv`只作兼容投影；首次迁移不能
推断哪些URL完成，因而将旧URL作为深度0根任务重放，并打印迁移提示。
没有快照SHA256的旧crawl.journal会保留URL、深度和首次次序，将所有任务（包括
旧完成记录）转为待重放；迁移记录先同步落盘，不把旧编号当作内容证明。

日志上限256MiB、单记录2MiB、单代任务100000；预算耗尽明确失败，不丢弃
任务。日志支持下文的原子压缩，索引支持下文的后台回收。当前验证覆盖macOS本地文件系统与
Linux容器中的真实进程中断与同epoch备份分叉拒绝；未认证掉电、NFS或跨机器锁。不同应用的组件地址互相隔离；目录锁和运行地址不替代分布式租约/fencing。
恢复日志与索引必须成套备份。升级须一起部署当前Rust应用和JVM业务jar，
旧JVM可能不支持PrepareIndex或CrawlStateV2/CheckpointV2，旧快照读取器不能读取版本5。降级必须恢复升级前成套离线备份。
服务保留旧CrawlState/Checkpoint的24字节回执接口；当前应用只使用带历史证明的V2接口。
旧历史校验以64KiB缓冲逐文件计算SHA256，完整验证可达的同epoch父链后才能压缩。
Rust续爬先轮询PrepareIndex（每100ms、总期限600秒，单次RPC仍用原5秒期限）；
收到停止信号时取消等待，退休组件会取消并等待校验线程退出。新爬取/未完成Reset直接Clear，
serve-only只恢复检索，不触发旧历史准备。服务端准备未完成时拒绝历史相关写入，
不会阻塞actor扫描整条链；直接协议调用方也须先轮询准备完成。
新完整快照同步发布后才异步删除旧文件；确认不代表物理删除已经结束，Healthz.segments
报告当前物理数量。进程中断后重新验证最新快照并继续清理，停止时等待校验和回收线程
退出后才释放目录锁。禁止手工删除祖先文件；摘要不是恶意存储重写的签名，也不能替代远端所有权。


正文提取在title/body、块级元素及编号标题之间保留空白，内联标记不会把单词
拆开；script/style/noscript只有匹配的结束标签才退出忽略状态，`</scripture>`
不再使脚本文本泄入索引。真实恢复语料检出标题和正文粘连导致搜不到词的缺陷，
回归使用原无额外空格的页面。当前提取器仍是轻量实现，不宣称完整HTML5解析。

### 组件身份与恢复清理

数据目录新增`remote.identity`（操作系统随机源生成的256位应用身份）和
`remote.components`（带SHA256校验的角色→运行标识记录）。组件名为
`ws-<应用身份>-<角色>-<运行标识>`，角色仍是frontier/tokenizer/search。
每次运行生成新标识；启动停止上一条持久记录中的确切地址，确认退休后先同步
新部署意图，再发送Deploy。丢失部署回执也能定位下一次恢复需要清理的地址。
退出只停止本次尝试部署的确切地址，服务端回执必须精确匹配请求路径。

`--serve-only`只更新search的记录，保留其它两个角色，随后完整爬虫可继续清理。
这两个文件需与crawl.journal成套备份；不要删除、手改或同时运行复制了相同身份
的数据目录。文件损坏、外来身份和缺失身份会明确失败，不生成新身份掩盖已有意图。
历史固定名称组件不属于新身份；升级前由其管理者明确停止或重启专用网关，应用
不会自动抢占`/user/search`等旧名称。`--index-data`显式选择并记住JVM上的索引数据
目录；原始Rust的`--data`只选择本地日志/身份目录，不自动改变远端索引路径。
两个独立身份争用同一索引时，后者被文件锁拒绝且不停止前者。运行地址隔离已经验证，跨机器克隆身份、分区和晚到Deploy
的租约/fencing、自动回收仍未完成。

### 共享网关上的独立索引

同一组三网关可以服务多个应用。每个应用使用不同的`--data`保存本地任务日志及
身份，并使用不同的`--index-data`选择JVM机器上的绝对数据目录（快照位于其`index/`
子目录）。例如，应用A使用`--data ./journal-A --index-data /srv/search/A`，应用B使用
`--data ./journal-B --index-data /srv/search/B`，两者可连接相同`erl=/ray=/jvm=`端点。
这不会在Rust机器上创建或检查远端目录，也不会上传索引；JVM运行账户须有相应权限。

显式路径以TOML组件配置发送，优先于WS_DATA，并与角色部署意图一起带校验持久保存。
随后`--keep`或`--serve-only`省略该选项时沿用保存的路径；明确给出不同路径会在
连接网关前失败。首次从旧版迁移可以显式指定现有索引目录；显式冷启动可以重新
选择目录并重建索引。路径按原始字符串比较，不跨机器canonicalize；备份/迁移必须
同时保留本地身份、意图、任务日志与对应远端完整索引历史。不要手改持久记录。
从未选择过该选项的旧目录继续使用网关WS_DATA（未设则./data），该隐式环境路径
尚未由Rust绑定；要消除环境变化歧义，首次迁移时显式指定`--index-data`。

组件配置使用Tomlj解析完整TOML；只接受可选字符串data_dir，显式路径必须绝对、
无NUL且不超过4096个UTF-8字节。配置上限32768字节；重复键、未知字段、错误类型、
非法UTF-8或语法错误均拒绝部署，不能回退共享目录。省略配置或空表兼容WS_DATA。
解析器及其依赖按固定版本和SHA256构建进业务jar，远端不需额外安装这些jar。
此能力是数据与生命周期隔离，不是租户授权、远端租约或分布式fencing。

## 抓取日志压缩与格式升级

`crawl.journal` 的生产限制仍为256 MiB，单条记录2 MiB、任务总数100,000；
压缩不会删除已完成任务的去重身份，也不会提高这些限额。普通追加从上次压缩
结果之后增长约4 MiB时，自动把当前状态写成快照；接近文件上限时同样先压缩。
实际活跃状态仍不能放入预算时明确失败，不声称可以无限增长。

压缩保留URL发现顺序、深度、完成状态、epoch/generation及索引历史摘要。
追加记录按新校验链重新编码后，再检查单条和总文件上限，避免校验字段的编码
长度变化导致写出无法读取的超限记录。
`--keep`继续使用原有历史验证；旧无摘要记录仍保守重放。显式新爬取先原子发布
完整重置意图，再清空远端索引；新Reset还预留后续Begin检查点所需的最大编码空间；旧重置意图续跑也在Clear前
检查同一容量，防止先清空索引再因Begin超大而失败。已确认Clear后的Begin也原子替换，因此重复
检查点或历史重置填满日志，不会阻止一轮合法的新爬取。

新文件以`PRWSJ2`行标识格式2，快照分块写入并以校验链中的Seal记录结束。
写入保留名`.crawl.journal.next`，依次执行临时文件sync、rename与父目录sync；
目录所有权锁独立保留。任何发布IO失败要求重新打开日志，不允许继续写旧的
已解除链接的文件。进程退出前未发布的临时文件，在持有目录锁且验证主日志
成功后丢弃；非普通文件路径拒绝。缺少Seal的压缩文件报错，不恢复成空队列。
普通追加尾部截断仍只回退到最后一条完整记录。

新程序可以读取原无头部的版本1日志，并在压缩或新爬取时迁移。
旧程序不能读取格式2；升级前应保存停机状态下相匹配的日志与索引备份。
不得只截掉新格式头部或只恢复不匹配的日志来回滚。此机制回收本地日志的
冗余历史，不压缩JVM索引段，也不提供NFS/分布式fencing或磁盘空间无限扩展。

原生 manifest 宿主可用 `deploy/package_host.py` 构建含网关依赖的可迁移包，运行时不依赖源码 checkout 或 Maven 缓存。安装步骤、系统前提和只读目录要求见[运维说明](docs/OPERATIONS.md#可迁移的原生-manifest-宿主)。
