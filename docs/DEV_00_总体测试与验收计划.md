# DEV_00 · 总体测试、模拟仿真、回归与验收计划（P1-P6 全程）

> 状态：**开发文档（实施合同）** · 2026-10-04
> 定位：**本文件是全部 DEV_01-06 的测试上位总纲**——各 DEV 的"测试义务"是模块级清单，本文件回答四个它们不回答的问题：① 全项目测试金字塔怎么分层、每层跑在哪、多久跑一次；② 模拟仿真环境怎么搭（拓扑/故障/规模三轴）；③ 回归怎么保证不退化（门禁/基线/冻结件）；④ 每个阶段的验收流程怎么走（谁跑什么命令、看什么数字、签什么字）。
> 上位依据：[07 §14](./TECH_DESIGN_07_异构联邦协议设计.md)（E1-E4 验收基线 + G1-G7 门禁定义——本文件是 G 系门的**执行计划**）；各阶段 DoD 数值以对应 DEV 文档为准，本文件不重复数值只编入流程。

---

## 1. 测试金字塔与执行矩阵（何时/何地/跑什么）

```
L4 验收基准（release 级）   remote-bench / federation-lab 规模仿真 / 混沌电信档         → G3/G4/G5
L3 混沌与故障注入（每夜）   kill -9 / 分区 / 丢包 / 慢盘 / 时钟跳变 / 双杀              → G4
L2 跨进程/跨语言集成（每 PR） RC1-RC8 / 集群矩阵 / 网关互操作 vectors 四语言矩阵        → G2
L1 单元与回环（每提交）     帧 golden / codec / 状态机 / mem transport 回环             → G1/G2
L0 编译期（每提交）         -D warnings / clippy pedantic / semver / 依赖白名单/unsafe  → G1/G7
```

**执行矩阵**（谁触发/在哪跑/时长预算——超时即红）：

| 层 | 触发 | 环境 | 预算 | 红线行为 |
|---|---|---|---|---|
| L0 编译期 | 每提交/每 PR | CI linux+macos | ≤5min | 阻塞合并 |
| L1 单元回环 | 每提交 | CI（mem transport，无端口） | ≤10min | 阻塞合并 |
| L2 集成 | 每 PR | CI docker（双节点/三节点 TCP+QUIC+TLS） | ≤20min | 阻塞合并 |
| L2 跨语言 vectors | 每 PR（涉及协议/网关时） | CI matrix: rust+jvm+py+erl+ts+cpp | ≤15min | 阻塞合并 |
| L3 混沌 | 每夜 02:00 + 手动 | 独立 CI runner（可 kill 容器） | ≤60min | 报警+标记 release 阻塞 |
| L4 基准 | 每 release + 每夜（缩减版） | **独占裸机**（性能数据有效性） | ≤30min（全量 2h） | 回退 >5% 阻塞发布 |
| L4 规模仿真 | P6 里程碑 + 季度 | 专用机器（50 集群 compose） | ≤6h | G5 门禁记录归档 |

**测试代码的位置规约**（防"测试在哪"漂移）：单元测试与实现同文件 `#[cfg(test)]`；crate 级集成在 `{crate}/tests/`；跨 crate/跨语言互操作在仓库根 `interop/`（每语言一目录）；混沌剧本在 `chaos/`；仿真基建在 `tools/federation-lab/`（DEV_06 §5）。

---

## 2. 模拟仿真体系（三轴：拓扑 / 故障 / 规模）

### 2.1 拓扑仿真（federation-lab 的 compose 拓扑生成器）

| 拓扑剧本 | compose 形态 | 覆盖 | 阶段 |
|---|---|---|---|
| single | 1 节点 | 纯本地（协议零介入回归——E1.1 基线） | P1 |
| pair-tcp / pair-quic / pair-ws | 2 节点 ×3 载体 | 传输载体矩阵（E2.3 反压三载体） | P1/P2/P3 |
| trio-cluster | 3 节点 + gossip | SWIM 收敛/kill9（≤3.5s 门禁） | P2 |
| trio-cluster+k0 | 3 节点 + gossip + admin 证书 | K0 远程 spawn 管理协议（06 §P2.6）：spawn_named/AdminStop 回执/幂等冲突/权限拒绝 | P2 |
| hub-relay | 3 节点 hub 形态（复刻 POC p4b：jvm→parrot→erl） | 中继 cid 改写/回程 | P2 起持续回归 |
| dual-cluster | 2 集群 ×3 节点 + border + Directory | 跨集群 RESOLVE/缓存/降级链 | P5 |
| tri-mode | hub/mesh/hybrid 三配置同测 | 07 §11 P5 出口"三模式矩阵" | P5 |
| federation-50 | 50 集群（DEV_06 §5 折叠模式） | 规模参数表逐行 | P6 |

### 2.2 故障注入器（chaos/ —— 混沌剧本的统一工具箱）

| 注入器 | 实现 | 对应 07 §14.3 六注入 |
|---|---|---|
| `kill9 [node\|pct%]` | docker kill（SIGKILL） | 节点宕机 |
| `partition A B [dur]` | iptables/tc 分向阻断（POC bus.partition 的容器版） | 网络分区（对称/非对称） |
| `loss iface pct dur` | tc netem 丢包 | 链路抖动 |
| `slowdisk path latency` | device-mapper delay / fsync 拦截 | 慢磁盘（WAL） |
| `clockjump node ±s` | libfaketime 注入 | 时钟跳变 |
| `dualkill raft-group` | 同时 kill leader+一副本 | Directory 双杀 |

**剧本格式**（声明式 yaml，可重放）：`scenario: [拓扑剧本, 注入序列, 断言集]`——断言集直接引用各 DEV 的门禁数值（如 `swim_convergence: ≤3.5s`），执行器产出标准 JUnit XML + 指标快照。

### 2.3 规模仿真（数字孪生，DEV_06 §5 的计划锚点）

- **物理抽样**：50 集群 × 200 容器 × 4 逻辑节点（40k 进程内模拟）跑真实收敛/路由/故障剧本
- **孪生全遍历**：百万节点地址空间静态验证路由正确性（不启连接，≤10min）
- **数字基线**：每次仿真产出 JSON 基线（收敛时间/带宽/内存/缓存命中率），与上次基线 diff——规模性能回退可视（G5 的趋势部分）

---

## 3. 回归保障体系（防退化三件套）

### 3.1 冻结件（不可变凭据）

| 冻结件 | 落点 | 保护机制 |
|---|---|---|
| golden vectors | `docs/vectors/wire1.json`（DEV_01 DoD 落库） | **只增不改**：修改 = 协议 break = 必须走 version 协商（07 X2 纪律）；CI 断言四语言全部消费同一文件 |
| 错误码表 | `ErrCode` 枚举（13 项） | `errcode_table_frozen` 测试锁数值与顺序（DEV_01 §3.8） |
| C ABI 符号表 | cbindgen 输出（DEV_04 §6） | semver-checks CI |
| 性能基线 | `docs/PERF_BASELINE.md` + 每次 release 的 bench JSON | G3：回退 >5% 阻塞 |

### 3.2 语义回归套件（每 PR 必跑，跨阶段只增不减）

| 套件 | 内容 | 起始 | 扩展规则 |
|---|---|---|---|
| RC1-RC8 | 远程语义八项（恰好一次/保序/串扰/超时/死信/NotRemotable/断连/反压） | P1 | P2 加 gossip 开启三节点复跑；P5 加中继路径复跑——**环境加严断言不变** |
| X1-X8 | 进程内双引擎互通（test_cross_engine_poc） | 已有 | 永久回归（remote 引入前后必须都绿——E1.1 的实证） |
| vectors 矩阵 | 四/五/六语言逐字节 | P1 起 | 每新增语言扩一列 |
| 混沌六注入 | 07 §14.3 表 | P2 起 | 每阶段把该阶段新机制纳入注入面（P3 加 WAL 慢盘；P5 加 Raft 双杀） |

### 3.3 里程碑回归（阶段出口全量重跑）

每阶段 DoD 验收时，**重跑此前全部阶段的语义套件**（P4 出口 = RC + SWIM + 互操作 + durable + 边缘反压 + sharding 全绿）——防"新阶段破坏旧能力"。回归清单由 DEV_README 的阶段表自动展开（CI job `milestone-regression-{Pn}`）。

---

## 4. 各阶段验收流程（谁/跑什么/看什么/签什么）

### 4.1 通用流程（每阶段同构）

```
① 自验（开发者）   ：该 DEV 文档全部"测试义务"绿 → 提 PR
② CI 门禁（自动）  ：L0-L2 全绿 + 覆盖率 ≥85%/75% + vectors 矩阵绿
③ DoD 评审（评审者）：按 DEV 文档 DoD 清单逐项跑命令核数值（本文 §4.2 的命令表）
④ 里程碑回归（CI） ：milestone-regression-{Pn} 全绿
⑤ 签收（架构负责人）：TECH_DESIGN_04 §13 对应行勾选 + ADR 状态更新 + 基线归档
```

### 4.2 验收命令表（可直接执行的验收脚本骨架）

```bash
# P1 出口（DEV_01 §7）
cargo test --workspace --release                    # 632+新增全绿
cargo test -p parrot-remote --features mem,tcp      # RC1-RC8 双载体
cargo run --bin remote-bench -- --gate p1           # ask p50<150µs tell<60µs（独占裸机）
PARROT_TRACE=frame cargo test smoke_trace           # 帧摘要可读性

# P2 出口
cargo test -p parrot-remote swim -- --ignored       # kill9 ≤3.5s（真实 kill 用例）
cd parrot-protocol-jvm && mvn test                  # vectors JVM 侧
cargo run --bin interop-matrix                      # rust↔jvm ask RTT<300µs

# P3 出口
cd packages/lite && npm test && npm run size-check  # jest + bundle<50KB
cargo test durable -- --ignored                     # 断网 5min 零丢（真实超时用例 --ignored）
cd parrot-protocol-py && pytest                      # ray 1000 任务 <15%
cargo test mqtt_bridge                              # QoS 映射

# P4 出口
cargo test sharding singleton -- --ignored          # kill 节点 5s/singleton 13s
cd native/cpp-lite && make test                     # vectors C++ 侧

# P5 出口
cargo test raft:: -- --ignored                      # jepsen 式四套件
docker compose -f tools/federation-lab/dual-cluster.yaml up && cargo test federation_e2e
cargo test livekit_bridge -- --ignored              # 真实 livekit-server 容器

# P6 出口
tools/federation-lab/run.sh federation-50 --report docs/SCALE_REPORT.md   # 参数表逐行
tools/federation-lab/twin --million-nodes           # 孪生全遍历 ≤10min
diff docs/vectors/wire1.json <(cargo run --bin dump-vectors)  # E1.15 wire 零变更断言
```

（`--ignored` 约定：真实 kill/真实超时/真实容器用例标记 ignored——日常 CI 跳过，验收与每夜混沌显式跑。）

### 4.3 数值门禁的单一事实源

门禁数值**不重复维护**：本文件只编流程，数值一律引用各 DEV 文档的 DoD 节（P1=DEV_01 §7、P2=DEV_02 §8 …）；`remote-bench --gate p1` 等工具的阈值常量从 DEV 文档抄录时**必须带文档锚点注释**（`// 门禁源：DEV_01 §7-3`）——评审时对表核验。

---

## 5. 指标与观测（验收的眼睛）

每阶段验收必须能拿到以下指标快照（E1.10 的落地），无指标=验收无效：

| 指标族 | 采集 | 验收看 |
|---|---|---|
| 延迟 P50/P99（ask/tell/中继/RESOLVE） | remote-bench + metricsz 端点 | §4.2 各门禁行 |
| 收敛时间（SWIM/路由/选主） | 混沌剧本断言器 | 3.5s/10s/3s 各门禁 |
| 带宽（gossip 稳态/注入风暴期） | 节点 metrics 差分 | ≤50KB/s（P2 档） |
| cid 泄漏 / late_reply_dropped | Counter（P1 起内建） | 长跑测试后计数=0（泄漏）/ 可解释（迟到） |
| 内存 RSS（缓冲轰炸下） | 混沌注入中采样 | 平稳无锯齿（E2.2） |
| 缓存命中率（P5 起） | ResolveCache 计数 | >99.9% 稳态 |

---

## 6. 风险与未决

| # | 风险 | 缓解 |
|---|---|---|
| T1 | 独占裸机资源不足（L4 基准排队） | 夜间缩减版（核心三场景）+ release 全量；裸机规格写入 bench 报告头 |
| T2 | 跨语言 CI 矩阵维护成本（六语言） | vectors 是唯一跨语言强约束（单文件）；其余互操作按阶段按需开列 |
| T3 | 混沌剧本与实现漂移（改协议忘改剧本） | 剧本断言引用 DEV 门禁锚点（§4.3 同理）；CI lint 检查锚点有效性 |
| T4 | 仿真折叠模式（4:1）与真实部署偏差 | P6 季度全量物理抽样校准一次；偏差记录进 SCALE_REPORT |
