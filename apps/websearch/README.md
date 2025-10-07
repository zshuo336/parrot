# websearch —— 真实搜索引擎（五运行时全分工）

与 `apps/crawler-lab`（合成页回归基线）同族架构，面向**真实互联网**：

| 运行时 | 组件 | 职责 |
|---|---|---|
| Erlang/OTP | `frontier` | 真实 URL 去重（ETS）+ BFS 调度 |
| Rust/Parrot | `crawler` | reqwest 真实 HTTPS 漫爬 + HTML 抽链/抽文 + 编排 + 百度式 Web 页 |
| Ray/Python | `tokenizer` | **jieba 中文分词**（开源）+ 词频统计 |
| Akka/JVM | `search` | **jieba-analysis 查询切词** + 倒排 + BM25 + 段文件落盘（重启回放） |
| TS Lite | 浏览器 | 零依赖访问 `http://localhost:8080` |

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

# 一键全链（起三网关 + 漫爬 + Web 服务）
apps/websearch/run.sh https://www.runoob.com --pages 100 --port 8080
# 浏览器打开 http://localhost:8080 —— 中文搜索即用

# 仅检索（独立运行——回放已落盘索引）
./target/release/websearch --serve-only --port 8080 --data apps/websearch/data
```

分词三语言同族：Python `jieba`（原版）/ JVM `jieba-analysis`（huaban 移植）/ Rust 侧经
Ray 通道复用 Python 分词（词条布局 `len|term|docid|tf` 四方言同构）。

## 真实化四诉求对照

1. **真实漫爬**：seed URL → robots.txt 礼貌策略 → 同域 200ms 节流 → depth 剪枝 →
   BFS 出链回注 frontier（百科类站点 403 反爬时换 runoob/MDN/w3school 等可爬站）。
2. **索引落盘**：`data/index/seg-*.segment`（DataOutputStream 原生格式：docs 表 +
   postings 表）——检索服务重启回放，不丢。
3. **四服务独立**：搜集（rust crawler）/ 调度（erl frontier）/ 分词索引（ray）/
   检索查询（akka）各为独立 actor 服务；`--serve-only` 单独跑检索。
4. **浏览器访问**：内置 HTTP 服务——百度式首页 + 结果页（标题/URL/摘要/score +
   关键词高亮 + 分页 + 5s 轮询 `/stats` 索引规模）。

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
