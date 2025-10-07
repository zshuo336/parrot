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

# 一键全链（起三网关 + 漫爬 + Web 服务）——本机直连形态（direct）
# 不给种子自动启用内置 47 条多样化种子集；默认目标 100 站 × 每站深度 10
apps/websearch/run.sh --port 8080

# 也可以指定种子与目标
apps/websearch/run.sh https://www.runoob.com --sites 100 --maxdepth 10

# 仅检索（独立运行——回放已落盘索引）
./target/release/websearch --serve-only --port 8080 --data apps/websearch/data
```

## 运行模式总表（四模式）

| 模式 | 入口 | 形态 | 适用 |
|---|---|---|---|
| ① 单机调试 | `run.sh` | 本机三网关进程 + direct 拨号 | 开发调试 |
| ② 单机 registry | `deploy/run-registry.sh` | 网关反拨注册（生产组网形态） | 组网演练 |
| ③ 多节点模拟 | `deploy/compose.sh up` | docker 4 容器真实跨网（172.30.0.0/16） | 单机模拟多物理机 |
| ④ 真实多机 | `deploy/distribute.sh` + `deploy/start-remote.sh` | ssh 分发 + 物理机网关 | 生产部署 |

## 跨网络部署（多节点形态）

parrot 组网与部署命令（admin-v2 Deploy）本就是跨网络的——`erl=/ray=/jvm=`
可填任意机器地址；run.sh 只是单机演示形态。三种远程形态：

### 形态 1：registry 反拨注册（生产推荐）

应用零网关地址知识——三网关（任意机器）主动反拨注册到应用：

```bash
# 应用侧（本机或任意机器）
./target/release/websearch --bind 0.0.0.0:19870 --wait 60 --port 8080

# 各网关节点（制品先经 apps/websearch/deploy/distribute.sh 分发）
apps/websearch/deploy/start-remote.sh erl  <应用IP>:19870   # Erlang 机器
apps/websearch/deploy/start-remote.sh ray  <应用IP>:19870   # Python 机器
apps/websearch/deploy/start-remote.sh jvm  <应用IP>:19870 --data /var/lib/websearch
```

### 形态 2：direct 显式跨机地址

```bash
./target/release/websearch \
  erl=10.0.0.11:19871 ray=10.0.0.12:19873 jvm=10.0.0.13:19872 \
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
重建 app 不重建镜像。基镜像经 `WS_REGISTRY_PREFIX` 走加速器（国内网络）。

制品分发（形态 4 用）：`deploy/distribute.sh user@host1[,host2...]`
——jar/beam/py + 三方言网关宿主打包推送 + 远端校验。

分词三语言同族：Python `jieba`（原版）/ JVM `jieba-analysis`（huaban 移植）/ Rust 侧经
Ray 通道复用 Python 分词（词条布局 `len|term|docid|tf` 四方言同构）。

## 站点目标制爬取（用户裁定 3）

- `--sites N`（默认 **100**）：爬够 N 个**不同 host** 且每站深度达标才允许停
- `--maxdepth D`（默认 **10**）：每站爬取深度 ≥ D（链接逐层展开，depth+1，D+1 截断）
- `--pages P`（默认 50000）：安全页数上限（防死循环兜底——队列耗尽自然收尾）
- 内置 47 条多样化种子（中文门户/技术社区/高校/国际文档站）——不给种子自动启用；
  **队列耗尽且目标未达时自动回填内置种子**（单种子死路/JS 渲染页自愈——404 也能续爬）
- Web 服务**边爬边开**（爬取启动即监听）——站点/分词表页实时可见爬取进度
- 数据目录：默认**全新爬取**（清旧索引段+去重表——历次残留不混入 /docs 统计）；
  `--keep` 续爬（保留旧索引与去重表——增量模式）
- **周期段落盘**（60s 一次）：爬取中途崩溃/中断后 `--serve-only` 可回放已爬部分——
  段为全量快照式（回放幂等，docid 覆盖）
- **deploy 幂等**：网关进程长存（重启应用不重启网关）——同名组件已在位时自动
  drain + 重部署（serve-only/应用重启不再 name 冲突 panic）

## 全场景测试（tests/run_all.sh）

```bash
apps/websearch/tests/run_all.sh          # 全量（含 docker compose 多容器）
apps/websearch/tests/run_all.sh --fast   # 快速（跳过 compose——本机四进程形态）
```

| 组 | 场景 | 覆盖点 |
|---|---|---|
| A 组网 | A1 direct 拨号 / A2 registry 反拨 / A3 compose 4 容器 | 三种组网拓扑真实 TCP 全链 |
| B 生命周期 | B1 冷启动 / B2 `--keep` 续爬 / B3 serve-only 回放 | 数据目录三策略 + 段回放 |
| C 协议 | `/` 搜索 / `/docs` / `/terms` / `/stats` | 四 HTTP 端点 + BM25 高亮 |
| D 健壮性 | D1 死种子自愈 / D2 残留清空 / D3 端口顺延 / D4 网关迟到 | 回填种子 / 8330 占用顺延 / 30s 重试窗 |
| E 边界 | E1 分页越界钳制 / E2 空库浏览 | p=999 渲染末页 / 空索引不 panic |


## 站点与分词表浏览（用户裁定 1/2）

- **站点页 `/docs`**：全部已索引站点（host 聚合、页面数降序、样例 URL）——分页 100/页
- **分词表页 `/terms`**：jieba 切出的全部词条（df 降序，点词条直接搜索）——分页 300/页
- 入口：页头导航「站点」「分词表」+ 首页快捷链接

## 真实化四诉求对照

1. **真实漫爬**：seed URL → robots.txt 礼貌策略 → 同域 200ms 节流 → depth 剪枝 →
   BFS 出链回注 frontier（百科类站点 403 反爬时换 runoob/MDN/w3school 等可爬站）。
2. **索引落盘**：`data/index/seg-*.segment`（DataOutputStream 原生格式：docs 表 +
   postings 表）——检索服务重启回放，不丢。
3. **四服务独立**：搜集（rust crawler）/ 调度（erl frontier）/ 分词索引（ray）/
   检索查询（akka）各为独立 actor 服务；`--serve-only` 单独跑检索。
4. **浏览器访问**：内置 HTTP 服务——百度式首页 + 结果页（标题/URL/摘要/score +
   关键词高亮 + 分页 + 5s 轮询 `/stats` 索引规模）+ 站点页 `/docs` + 分词表页 `/terms`。

## 中文支持

- 页面编码：GBK/GB2312 自动探测转 UTF-8（iconv 兜底）
- 索引分词：ray 侧 jieba（停用词表 + 单字过滤）
- 查询分词：jvm 侧 jieba-analysis SEARCH 模式（同停用词表）

## 数据目录

```
apps/websearch/data/
├── dedupe.tsv          # URL 去重持久化（重爬幂等）
└── index/
    └── seg-00000.segment   # 倒排段（docs + postings）
```
