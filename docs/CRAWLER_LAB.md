# Crawler-Lab：五运行时全链集成场景

> 大规模爬虫 + 索引构建 + 用户 Web 检索——异构 actor 运行时按技术强项分工，
> 通过 Wire 1.0 协议两两互通，端到端可验证、可监控、可回归。

## 1. 场景架构

```
                ┌──────────────────────────────────────────┐
                │        TS Lite（用户终端/边缘）            │
                │  Node tcp:// 直连 ←→ 浏览器 WS 兜底两栖    │
                └──────────────────┬───────────────────────┘
                                   │ bin:crawl/Search（用户查询）
                                   ▼
┌──────────────────┐    IndexTerms  ┌──────────────────┐
│  Ray (Python)    │◄───────────────│  Akka (JVM)      │
│  索引构建         │                │  搜索 API         │
│  CPU 密集：       │   (Rust 分词)  │  倒排聚合 top-k   │
│  分词/词频统计    │                │  高并发短查询      │
└────────▲─────────┘                └────────▲─────────┘
         │ IndexPage（HTML 整页）              │ IndexTerms / Search
         │                                    │
┌────────┴────────────────────────────────────┴─────────┐
│                Parrot / Rust（爬取 worker + hub）       │
│   tokio 并发合成页面 · 出链收集 · 词频计算 · 编排路由     │
└───────┬────────────────────────────────────────────────┘
        │ FrontierPush / FrontierNext（URL 调度）
        ▼
┌──────────────────┐
│ Erlang (OTP/ETS) │
│ URL Frontier     │
│ IO 密集：         │
│ ETS 去重+有序队列  │
└──────────────────┘
```

**分工原则**（每类运行时用在其并发模型最强处）：

| 运行时 | 角色 | 理由 |
|---|---|---|
| Erlang/OTP | URL Frontier 调度器 | 海量轻量进程 + ETS 有序表——调度/去重的天生形态 |
| Parrot/Rust | 爬取 worker 集群 + 系统 hub | tokio 高并发 IO + 零拷贝路由，中枢核心 |
| Ray/Python | 索引构建（分词+词频） | CPU 密集并行计算（ray worker 池） |
| Akka/JVM | 搜索 API（倒排+top-k） | 高并发短查询请求处理，面向用户 Web 访问 |
| TS Lite | 用户终端 | 边缘轻客户端（Node tcp:// 直连 / 浏览器 WS） |

## 2. 数据流（两两互动全覆盖）

```
Rust ──FrontierPush/Next──▶ Erlang   （URL 调度：出链回注 / 批量取批）
Rust ──IndexPage──────────▶ Ray      （HTML 整页 → CPU 分词统计）
Rust ──IndexTerms──────────▶ Akka    （词项条目 → 倒插入库）
Rust ──Search/Healthz─────▶ Akka     （查询 top-k / 存活探针）
TS  ──Search──────────────▶ Akka     （真实用户查询跳）
```

Rust hub 与三个被动网关辐射相连——星型拓扑下四条 wire 边全覆盖，
TS 终端经 JVM 网关消费索引（第五运行时出口）。

## 3. 线上消息契约（bin: 裸 LE 方言）

| type_key | 方向 | 载荷布局（LE） |
|---|---|---|
| `bin:crawl/FrontierPush` | Rust→Erlang | `[n u32]{id u64 \| len u32 \| url \| depth u16}` |
| `bin:crawl/FrontierNext` | Rust→Erlang | `[n u32]` → 回 `FrontierBatch` 同 push 布局 |
| `bin:crawl/IndexPage` | Rust→Ray | `[n u32]{doc u64 \| len u32 \| html}` |
| `bin:crawl/IndexStats` | Rust→Ray | 空 → `[terms u32 \| postings u64]` |
| `bin:crawl/IndexTerms` | Rust→Akka | `[n u32]{len u32 \| term \| doc u64 \| tf u32}` |
| `bin:crawl/Search` | Rust/TS→Akka | `[k u32]{len u32 \| term}` → JSON top-k |
| `bin:crawl/Healthz` | Rust/TS→Akka | 空 → `{"terms","postings","queries"}` |

## 4. 一键运行

```bash
# 依赖：erl / java(已 mvn package) / python3+ray / node / cargo
make test-lab                    # 200 页标准回归（Makefile 集成）
./deploy/crawler-lab/run-lab.sh --pages 20000 --depth 4 --fanout 5 --batch 256
```

场景驱动器（Rust）：`crates/parrot-node/src/bin/parrot-crawler-lab.rs`
- **阶段 0** 网关方言 sanity（三网关探活）
- **阶段 1** 种子注入 Erlang frontier
- **阶段 2** 爬取循环（合成页面 → 出链回注 → 双路索引缓冲 flush）
- **阶段 3** 索引对账（**三方收敛断言**：Rust 本地词表 == ray terms == jvm terms；ray postings == jvm postings）
- **阶段 4** 搜索验证（4 组查询 top-k 非空断言）+ healthz 终态
- **阶段 5**（run-lab.sh）TS Lite 用户终端真实查询

站点模拟：无外网依赖——xorshift64* 确定性合成页面（33 词表采样），
页面间链接由 seed 派生（可复现爬取图）。

## 5. 运行状态监控

- 驱动器内置指标打点（每 2s）：`pushed / fetched / ray_pages / jvm_terms`
- 网关侧健康：`bin:crawl/Healthz`（akka terms/postings/queries 计数）
- 索引对账 = 分布式一致性证明（三方独立计数必须逐位相等）
- 网关日志：`/tmp/lab_{erl,jvm,ray}.out`（端口行 + stderr 事件）

## 6. 实测规模（M1 macOS，2026-10-05）

| 规模 | fetched | postings | 爬取耗时 | 全程 |
|---|---|---|---|---|
| 200 页 | 191 | 5,578 | 44ms | ~2.6s（含网关拉起） |
| 5,000 | 4,901 | 140,350 | 348ms | 2.9s |
| 20,000 | 19,128 | 548,773 | 1.6s | 4.7s |
| 50,000 | 48,834 | 1,401,441 | 5.5s | 7.7s |
| 100,000 | 97,730 | 2,802,007 | 15.4s | 18.3s |

吞吐：~6.3k pages/s（合成页 IO+双路索引全链）；线性扩展无性能悬崖；
三方对账在所有规模下 100% 收敛。

## 7. 测试集成

- `make test-lab`：依赖齐备跑 200 页回归；缺工具链自动跳过
- `make test-everything`：全量测试 + 多语言 + 五运行时场景
- TS 终端测试独立跑自动 skip（需活网关），run-lab.sh 内驱动真跑
