# 08 运行、部署与日常运维

## 1. 先选运行方式

学习业务用本机 direct 启动器；学习反拨用 registry 启动器；学习声明式装配用 manifest。需要分机器运行时再使用远端网关，容器和宿主包则解决不同的交付边界。

所有未认证远端示例仅适合受控开发网络。隔离网络能减少暴露面，但不会把 plaintext 管理协议变成已认证的生产部署。

## 2. 本机启动器的工作

```bash
apps/websearch/run.sh --help
apps/websearch/deploy/run-registry.sh --help
```

两个入口共用 [launcher.py](../apps/websearch/deploy/launcher.py)。默认构建当前 release 制品，使用独立运行目录、OS 分配的网关端口、拥有的进程组与监督流程。

`WS_PROFILE=dev` 选择开发构建。`WS_SKIP_BUILD=1` 明确使用已有制品，此时才可用 `WS_BIN` 选择应用二进制。不要在构建失败后回退到旧文件并把它当成功。

启动器不会自动加 `--insecure-development`，需要由调用者明确决定。第一章命令已显式写出。

## 3. 运行目录与日志

每次启动输出 `WS_RUN_DIR`，包含本次有界轮转日志和最终 `result.json`。如设置这个变量，必须选择尚不存在的独立路径；启动器拒绝复用已有目录，避免把旧日志误当新结果。

没有显式数据参数时，启动器把数据放进本次运行目录。要跨次复用，使用 `--data` 绝对路径或 `WS_DATA`。`--keep` 与 `--serve-only` 要求明确数据位置，以免误选新建空目录。

`result.json` 应结合日志和业务检查解读；某份结果文件存在不能证明这次应用成功，更不能证明之后进程一直健康。

## 4. 就绪、健康、完成和退出

就绪意味着本次拥有的进程仍活着，并且实际 `/stats` 返回有效结构。READY 日志本身不是健康证明，某个旧进程在同端口响应也不能替代本次实例验证。

健康需要持续检查网关和业务响应。抓取完成是没有更多计划工作或预算/目标达成，并完成检查点。退出则需要服务停止和资源回收。

自动化脚本应分别记录这些状态。比如 `/stats` 返回 200 时，抓取仍可能在进行；任务完成后，Web 服务也可能仍正常运行。

## 5. 启动器环境变量

- `WS_PROFILE`：`dev` 或默认 release。
- `WS_SKIP_BUILD`：显式跳过本次源码构建。
- `WS_BIN`：跳过构建时选择已有二进制。
- `WS_RUN_DIR`：本次独占、尚不存在的证据目录。
- `WS_DATA`：明确的持久化数据位置。
- `WS_PORT`、`WS_SITES`、`WS_DEPTH`：对应 CLI 的默认值，显式 CLI 优先。
- `WS_STARTUP_TIMEOUT`：启动就绪期限，默认 180 秒。
- `WS_BUILD_TIMEOUT`：每项构建期限，默认 1200 秒。
- `WS_TIMEOUT`：可选整个运行期限。
- `WS_RAY_HTTP_PROXY`：仅 Ray 侧代理，不改变 Rust 抓取器禁用环境代理的行为。

这些变量属于启动工具，不等于原始 Rust CLI 都识别相同变量。自动化时保留最终命令、数据路径和运行目录。

## 6. 正常停止

INT/TERM 先给应用机会结束当前批次、持久化和回收组件，随后再停止网关进程组。启动器有有限宽限期，不能无限等待失联网关。

父启动进程遭 SIGKILL 时无法执行普通 finally，但 guardian 可通过控制通道 EOF 回收拥有的进程。它不替代业务检查点，也不意味着强杀与正常停止有相同数据保障。

不要使用全局 `pkill java`、`ray stop` 或删除共享 `/tmp` 的方式“清理教程”。这些命令可能破坏其他项目。清理应依据本次拥有的进程与目录。

## 7. 本机 registry 实验

保持第一章的测试网站运行，改用另一个数据目录：

```bash
apps/websearch/deploy/run-registry.sh --insecure-development \
  --data "$PWD/.local-websearch-registry" \
  --crawl-origin http://127.0.0.1:18080 \
  --pages 10 --sites 10 --maxdepth 2 --port 0 \
  http://127.0.0.1:18080/
```

主要差异是网关主动回连应用，而业务协议和持久化规则相同。注册是否完成要看真正活跃的连接与预期身份，不能只看“知道这个地址”。

## 8. 远端分发与 registry

先构建并分发到每台目标机器上的**新版本目录**：

```bash
apps/websearch/deploy/distribute.sh user@host-a,user@host-b,user@host-c \
  --root /opt/parrot/websearch-guide-v1
```

SSH 连接、目标权限、运行时和网络由你准备。目录不应覆盖已有发布。分发包含完整依赖并验证归档，但不提供跨多台机器的原子发布事务，也不能替代可信签名与发布来源管理。

应用机器执行，替换 APP_IP 和 seed：

```bash
./target/release/websearch --insecure-development \
  --bind 0.0.0.0:19870 --wait 60 \
  --node-root /opt/parrot/websearch-guide-v1 \
  --data /absolute/local-journal --index-data /var/lib/websearch-guide \
  --pages 100 https://example.org/
```

三个目标机器分别执行包内对应脚本，源码工作树内的形式如下：

```bash
/opt/parrot/websearch-guide-v1/start-remote.sh erl APP_IP:19870 \
  --root /opt/parrot/websearch-guide-v1
/opt/parrot/websearch-guide-v1/start-remote.sh ray APP_IP:19870 \
  --root /opt/parrot/websearch-guide-v1
/opt/parrot/websearch-guide-v1/start-remote.sh jvm APP_IP:19870 \
  --root /opt/parrot/websearch-guide-v1 --data /var/lib/websearch-guide
```

这三条命令应在各自目标机器执行，不是同时复制到应用终端。分发后的脚本就在发布根目录。`--node-root` 是远端业务制品根；应用的 `--data` 是本地 journal；`--index-data` 和 JVM 启动脚本的 `--data` 要协调为同一远端索引根。

当前角色期望的网关身份包括 `erl-gw-1`、`ray-gw-1`、`jvm-search-1`。serve-only 只需 JVM。注册超时先检查身份、路由、防火墙和握手，不要通过随机放宽预期身份绕过错误。

## 9. 手工 direct 连接

网关已独立启动并部署文件就位时，可使用：

```bash
./target/release/websearch --insecure-development \
  erl=10.0.0.11:19871 ray=10.0.0.12:19873 jvm=10.0.0.13:19872 \
  --node-root /opt/parrot/websearch-guide-v1 \
  --data /absolute/local-journal --index-data /var/lib/websearch-guide \
  --pages 100 https://example.org/
```

地址是示意，必须换成你实际运行的网关。direct 参数使用具体 IP 和非零端口；IPv6 使用 `[地址]:端口`。网关连接地址与 crawler 允许访问的网页 origin 是两类独立配置。

## 10. 容器运行

先阅读[镜像说明](../deploy/images/README.md)。需要 Docker Compose、BuildKit、匹配的 Rust musl target、cargo-zigbuild/Zig，以及对应 CPU 平台支持。不是安装 Docker 后就自动拥有所有跨编译依赖。

```bash
WS_PLATFORM=linux/amd64 apps/websearch/deploy/compose.sh up \
  --insecure-development --pages 20 https://example.org/
apps/websearch/deploy/compose.sh ps
apps/websearch/deploy/compose.sh stats
apps/websearch/deploy/compose.sh search Parrot
apps/websearch/deploy/compose.sh logs app
apps/websearch/deploy/compose.sh down
```

ARM64 使用 `WS_PLATFORM=linux/arm64`；包装器当前默认是 ARM64，不应在 AMD64 主机上忽略平台。以实际打印的 Web 发布地址为准，默认宿主映射为 loopback 8188。

镜像封装通用运行时和协议层，业务制品按部署配置挂载；`file://` 制品地址指容器内路径。宿主能看到某个 jar 不代表容器能看到同一路径。

`compose.sh down` 先停应用，保留网关给最终提交使用，再关闭服务。**`compose.sh down -v` 会删除卷中的持久化数据。** 不要把 `-v` 当例行停止参数。

Ray 容器需要 init/subreaper 处理孤儿进程，仓库 Compose 已有相应配置。容器内进程回收与业务 Actor 退休仍是两个层面。

本机语料的 `127.0.0.1` 在容器中指容器自身。不要原样把第一章命令搬进容器后期待访问宿主 HTTP 服务；需要在容器网络中部署语料或明确配置可达且被允许的目标。

## 11. 原生宿主包

```bash
python3 apps/websearch/deploy/package_host.py \
  --output /absolute/new-output/websearch-host.tar.gz
```

脚本构建宿主并收集运行资源、业务制品、依赖、MANIFEST 和 RELEASE 元数据，输出位置应为新文件。包内指南说明如何使用接收器校验和安装。

包可脱离源码与构建机 Maven 缓存运行，但仍需要兼容的操作系统/架构、Python/Ray/jieba、Java 与 Erlang。它不是包含所有系统运行时的独立静态程序。

安装后以包内 `bin/websearch --manifest ...` 运行，数据放到发布目录之外的绝对路径。默认清单相对 data 不适合只读发布目录。`--runtime-root` 可明确选择完整本机运行资源根。

## 12. 日常运维检查

至少记录：版本与制品摘要、运行模式、实际端口、数据目录绑定、各进程存活、索引文档/词项数、提交耗时、最新检查点、磁盘空间、失败与退出码。

查询超时先区分 HTTP 入口、Parrot 请求、JVM Actor 队列和磁盘提交。CPU 高先判断 tokenizer 分词、HTML 处理还是 JVM 快照；不要笼统增加所有线程池。

对外提供服务之前，独立设计认证、TLS、管理面网络隔离、反向代理限制、备份恢复、容量和告警。当前教程没有完成这些生产验收，不应把未认证跨机示例称为安全上线方案。
