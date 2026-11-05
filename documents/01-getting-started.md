# 01 环境准备与第一次运行

目标：在本机真实启动 Rust 应用、Erlang frontier、Ray tokenizer 和 JVM search，抓取随本书提供的三页网站，并从浏览器搜索到结果。这个实验使用真实 HTTP 和真实跨语言组件，不需要访问公网网页。

## 1. 准备工作目录

进入包含 `Cargo.toml` 的仓库根目录。获取代码时建议同时获取子模块：

```bash
git submodule update --init --recursive
git status --short
```

子模块中包括协议向量等资料。不要为了运行教程重置已有修改，也不要在不了解数据用途时执行清理命令。

首次下载 Rust crates、Maven 依赖和 Python 包仍需要网络。这里的“本地实验”指被抓取的网站和应用进程在本机，不代表首次安装可以离线完成。

## 2. 安装并检查依赖

准备 Rust/Cargo、CPython 3.11、Java 21/JDK、Maven、Erlang/OTP、`make`、`unzip`。仓库 README 记录的当前 Rust 验证工具链为 1.98.1；这不是仓库自动安装或锁定工具链的承诺。Erlang 可优先使用 OTP 27，并让编译与运行使用同一个 OTP 系列。不要把旧文档中的最低版本当成所有当前测试的支持矩阵。

```bash
rustc --version
cargo --version
python3 --version
java -version
mvn -version
erl -noshell -eval 'io:format("~s~n",[erlang:system_info(otp_release)]),halt().'
erlc -help
jar --version
unzip -v
```

`erlc -help` 的退出码取决于工具实现，用它观察命令是否存在即可。JDK 必须同时提供 `java` 和 `jar`。只安装 JRE 不足以完成源码构建。

创建独立 Python 环境：

```bash
python3 -m venv .venv
source .venv/bin/activate
python3 -m pip install --upgrade pip
python3 -m pip install -e './interop/python[ray,test]'
python3 -m pip install jieba
python3 -c 'import ray, jieba; print("Python dependencies OK")'
```

选择 Ray 支持的 Python、操作系统和 CPU 架构组合。若 pip 找不到 Ray wheel，先检查版本兼容性，不要直接跳过 tokenizer。`jieba` 是业务依赖，需要单独安装；Parrot Python 协议包可导入并不说明分词器已经可运行。

这些命令适合类 Unix 的源码开发环境。Windows 原生运行、所有 Linux 发行版和所有架构没有因本教程而得到统一验证。容器有独立入口，见第八章。

## 3. 先运行最小 Actor 示例

```bash
cargo run --locked -p parrot --example quickstart
```

预期业务输出包含：

```text
Actix: count=5
Thread: count=5
```

它证明当前机器能构建并运行两个本地引擎的最小示例。它不测试 Erlang、Ray、JVM 或 Websearch。下一章解释这段程序。

## 4. 构建 Websearch 与网关

```bash
make build-jvm build-erl
CARGO_BUILD_JOBS=2 apps/websearch/build.sh
```

先构建通用网关，再构建业务组件，是因为 JVM 业务编译需要通用网关提供的桥接接口与 Maven 依赖。`build.sh` 默认构建 release Rust 应用，编译 Scala 搜索组件、预编译 frontier，并检查 Python 依赖。构建脚本不会替你安装系统运行时。

主要产物：

- `target/release/websearch`：Rust 抓取、编排与 Web 服务。
- `interop/jvm/target/parrot-protocol-jvm-0.1.0.jar`：通用 JVM 网关。
- `apps/websearch/jvm/target/websearch-jvm-1.0.0.jar`：搜索业务及其打包依赖。
- `apps/websearch/erlang/frontier.beam`：Erlang 业务模块。
- `apps/websearch/python/tokenizer.py`：Python 业务源文件，由宿主加载。

自定义 Cargo target 目录会改变 Rust 产物位置。启动器会使用 Cargo 报告的可执行文件；手工启动时要使用你实际生成的路径。

开发迭代可执行 `apps/websearch/build.sh --profile dev`，后续启动器配合 `WS_PROFILE=dev`。不要把旧 release 文件当成当前源码的构建结果。

## 5. 终端 A：启动本地测试网站

本书已提供静态语料：入口页链接两个子页面，两个页面交叉链接，并包含一个 robots 禁止路径。

```bash
python3 -m http.server 18080 --bind 127.0.0.1 \
  --directory documents/examples/websearch-site
```

浏览器打开 `http://127.0.0.1:18080/`，确认能看到“Parrot 本地实验站”。这个 HTTP 服务只是被抓取目标，不是 Parrot 网关，也不是搜索 UI。

实验使用固定端口是为了容易理解。若 18080 被占用，换一个空闲端口，并同时修改后续 origin 和 seed。不要终止不属于你的进程。

## 6. 终端 B：启动完整应用

先激活虚拟环境，再从仓库根目录执行：

```bash
source .venv/bin/activate
apps/websearch/run.sh \
  --insecure-development \
  --data "$PWD/.local-websearch-guide" \
  --crawl-origin http://127.0.0.1:18080 \
  --pages 10 --sites 10 --maxdepth 2 --port 0 \
  http://127.0.0.1:18080/
```

这条命令的含义：

- `run.sh` 通过受监督启动器构建并启动真实本机网关，应用主动连接它们。
- `--data` 选择一个专属、明确的持久化目录。首次运行请使用新的目录。
- `--crawl-origin` 把抓取限定为这个协议、IP 和端口，并关闭内置公网种子回填。
- 显式 IP origin 允许本次受控实验访问本机地址；普通公网模式默认拒绝本机和私网目标。
- `--pages 10` 是尝试预算，包括失败和 robots 拒绝，不是保证生成十篇文档。
- `--sites 10` 让这个只有一个站点的小语料在队列耗尽时结束抓取；无需达到十个站点才可检索。
- `--maxdepth 2` 允许遍历入口和子页；去重防止循环链接导致无限抓取。
- `--port 0` 请求操作系统分配搜索 UI 端口，避免预先猜测端口是否空闲。

启动器默认会构建 release 当前源码，因此首次运行可能较慢。已经完成构建、明确要使用已有产物时才能加环境变量 `WS_SKIP_BUILD=1`。它不能作为“这次源码重新构建成功”的证据。

查找启动器输出：

```text
WEBSEARCH_READY=http://127.0.0.1:实际端口
```

同时记录 `WS_RUN_DIR`。它包含本次拥有的日志和最终结果。不要把文档中的“实际端口”原样粘贴到浏览器。

## 7. 在浏览器中观察结果

打开 READY 给出的地址，依次观察：

1. 搜索 `鹦鹉实验`：应能命中实验内容。
2. 搜索 `消息可靠性`：检查结果是否指向消息教程页。
3. 打开 `/docs`：观察被索引站点及其页面统计。
4. 打开 `/terms`：观察真实切出的词项和文档频率。
5. 打开 `/stats`：确认 JVM 搜索组件返回结构化状态。

可用 curl 访问真实端口，例如：

```bash
curl -f http://127.0.0.1:实际端口/stats
curl -f -G --data-urlencode 'q=消息可靠性' \
  http://127.0.0.1:实际端口/search
```

`/search` 返回 HTML，不是 JSON 搜索 API。中文分词可能受词典版本影响，不应仅凭某个自然语言短语的命中数量建立严格回归。正式验收使用独立固定语料与精确断言，见第九章。

被禁止的 `private.html` 不应出现在索引中。终端 A 的访问日志可以帮助区分抓取请求与浏览器手工访问；手工打开该页本身并不说明爬虫违反 robots。

抓取结束后搜索服务仍然运行。看到“索引段落盘”不是整个应用退出，也不是可以强杀所有进程的信号。

## 8. 停止与只读式服务重启

在终端 B 按 Ctrl-C，等待启动器完成排空和进程回收。不要立刻用 SIGKILL 代替正常停止。

保持相同数据目录，只提供查询服务：

```bash
apps/websearch/run.sh --insecure-development \
  --data "$PWD/.local-websearch-guide" --serve-only --port 0
```

此模式不接受 seed，不继续抓取，启动器只需 JVM 业务网关。再次搜索原内容，观察重启后结果是否保留。

继续抓取使用 `--keep`，并重新提供限定 origin 和 seed。**省略 `--keep` 的普通抓取是冷启动，会重置原索引。** 不要用普通首次启动命令随意检查已有数据。

```bash
apps/websearch/run.sh --insecure-development \
  --data "$PWD/.local-websearch-guide" --keep \
  --crawl-origin http://127.0.0.1:18080 \
  --pages 10 --sites 10 --maxdepth 2 --port 0 \
  http://127.0.0.1:18080/
```

已经发现或完成的 URL 可能被去重，不保证重新下载已修改页面。`--keep` 是恢复和保留机制，不是定时全量重抓功能。要比较不同网页版本，使用独立数据目录做冷启动实验，或按第九章为更新策略实现并验证完整流程。

## 9. 这次实验到底验证了什么

你观察到了本机四种运行时的真实业务链、HTTP 抓取、分词、索引和重启查询。它没有证明公网抓取覆盖率、跨机器网络安全、高并发性能、断电恢复或所有语言 SDK 的正确性。

如果失败，保留 `WS_RUN_DIR` 与数据目录，先按第十章定位。不要通过跳过 Ray、伪造健康响应或返回固定搜索结果来“完成”教程。
