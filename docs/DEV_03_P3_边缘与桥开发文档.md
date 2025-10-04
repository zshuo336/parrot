# DEV_03 · P3 边缘与桥开发文档（lite TS / durable tell / 反压贯通 / ray / erlang 网关 / MQTT 桥）

> 状态：**开发文档（实施合同）** · 2026-10-04 · 基准 commit `9e35fc0` + DEV_01/02 交付后
> 设计依据：[06](./TECH_DESIGN_06_P2-P4集群与联邦详细设计.md) 第二部分 + [07 §4.5](./TECH_DESIGN_07_异构联邦协议设计.md)（MQTT 裁定）+ POC（`poc/remote-poc/` erlang-gw / ray-adapter 实证）
> 前置：DEV_02 DoD（QUIC/TLS/Receptionist 可用——边缘接入的地基）
> 上位约束：[07 §14.6 E5](./TECH_DESIGN_07_异构联邦协议设计.md)；依赖白名单（rumqttc 已裁定）

---

## 0. 范围与红线

**做**：parrot-lite TS（E1）+ durable tell（E2）+ 反压贯通（E3）+ ray adapter（E4）+ Receptionist ACL（E5）+ erlang 网关正式化（E6，POC 转正）+ ParrotMqttBridge（E7，07 §4.5）。

**不做（红线）**：
- C++ lite（P4 与 C ABI 同期）
- LiveKit 桥（P5——07 §8.3）
- 动态 ACL/权限 actor 化（P4+；P3 静态 yaml）
- lite 的 STOP/SYSTEM_EVENT（协议子集，永久不做——07 §8.1 语义缺口列）

---

## 1. 任务分解

```
E1 lite TS ── 依赖 DEV_02 的 QUIC/TLS（WebTransport/降级链）
E2 durable tell ── 依赖 E1（端侧 ACK 位）+ 云侧 proxy actor（thread 引擎）
E3 反压贯通 ── 依赖 E1/E2（链路端到端才可测）
E4 ray adapter ── 独立（Python，仅依赖协议库）
E5 ACL ── 依赖 DEV_02 Receptionist
E6 erlang 网关 ── 独立（POC erlang-gw 转正，同 E4 并行）
E7 MQTT 桥 ── 依赖 E2（QoS1 ↔ durable tell 映射）
```

估算（06 P3.0 + 增补）：E1=6d E2=4d E3=2d E4=6d E5=2d E6=3d E7=5d（MQTT 桥含 broker 接入面）。

---

## 2. E1 · parrot-lite（TS，`packages/lite/`）

### 2.1 范围（协议子集）

实现帧：HANDSHAKE(_ACK)/HEARTBEAT(_ACK)/ASK/REPLY/REPLY_ERR/TELL。不实现：STOP/SYSTEM_EVENT/FRAGMENT/RESOLVE*。

### 2.2 API（06 §3.1.2 原样）

```typescript
import { ParrotLite } from "@parrot/lite";
const node = await ParrotLite.connect({
  url: "quic://gateway.example.com:443",   // WebTransport；降级 "tcp://"（Node）/ws://（浏览器）
  nodeId: "phone-" + deviceId,
  tls: { cert, key, ca },
});
const rpa = node.spawn("rpa-main", { onAsk, onTell });
await node.receptionist.register("edge/rpa", "rpa-main", { capabilities: ["screen", "tap"] });
const reply = await node.ask("parrot://cloud-1/thread-main/user/orch-1", RpaReport, data);
```

### 2.3 实现要点

- 帧解析 DataView 手写（07 §2.1 布局；golden vectors 移植为 jest 断言——读 `docs/vectors/wire1.json`）
- 一律 pb 栈（TS 无 bincode；07 §8.1 接入矩阵）
- 传输降级链：WebTransport → `@fails-components/webtransport`（Node）→ WebSocket+二进制帧（兜底）——运行时探测自动选
- 断线重连：指数退避 1s→30s；重连成功重 handshake + receptionist 重注册（云端按 (node_id,key,path) upsert 幂等）
- **体积预算 dist <50KB min+gz、零原生依赖**（门禁，bundle-size CI）

### 2.4 E1 测试义务

- `frame_vectors.test.ts`（golden 逐字节 + partial 半包语义）
- `handshake_negotiation.test.ts`（与 Rust 端 mem 互测——用 node 测试进程 + Rust 测试二进制对拉，CI matrix）
- `reconnect_idempotent.test.ts`（重连后重注册不产生重复事件）
- bundle-size 门禁测试

---

## 3. E2 · durable tell（云 proxy WAL）

### 3.1 CloudProxy actor（06 §3.2.1）

```rust
// parrot-remote/src/durable.rs（新）
// CloudProxy：thread 引擎，Sharded{affinity_key: node_id}（ADR-14 复用）
// 下行：云 actor tell → proxy 收 → 写 WAL(组提交 fsync 10ms 可配) → 转发端侧
// ACK：TELL 置 flags::TELL_ACK → 端侧处理完成回 HEARTBEAT 变体（payload=[u64 seq]）
// 断线：WAL 保留；重连按序重放；端侧按 (sender_path, seq) 去重 → 业务幂等层
```

### 3.2 WAL 实现裁定（06 ⚠ 复核点落地）

**文件追加 + 内存索引，不引入嵌入式 DB**（决策档：sled 维护成本 > 收益；单端 WAL 体积小截断频繁）：
- 段文件 64MB 滚动；段内 `[u32 len][crc32][record]`；ACK 水位推进后旧段整段删除（无逐条空洞）
- 崩溃恢复：扫尾段校验 crc，截断不完整记录

### 3.3 E2 测试义务（06 §3.2.2 验收）

- `durable_offline_5min`：端侧断网 5min，云端持续 tell 100 条 → 重连后 100 条全达零丢失、端侧去重断言零重复处理
- `wal_crash_recovery`：写一半 kill proxy → 重启扫描截断正确
- `tell_ack_latency`（ACK 路径不拖慢正常 tell：有 ACK 比无 ACK 额外延迟 <1ms p99）

---

## 4. E3 · 反压贯通（06 §3.3）

链路：端侧慢消费 → 端侧读循环挂起 → QUIC 流窗口收紧 → proxy 出站挂起 → proxy 邮箱(1024) 满 → 云编排 actor deliver 挂起 → 其邮箱堆积触发 Block/Error。

测试：`backpressure_e2e`——端侧 sleep 5s，云端 2000 tell；断言 Error 策略时编排 actor 收到 backpressure 错误、Block 策略时端侧唤醒后最终全达。

---

## 5. E4 · ray adapter（Python，06 §3.4）

POC `poc/remote-poc/ray-adapter/` 转正为 `parrot-protocol-py/` 包：
- 帧编解码（golden vectors 对齐，pytest）
- ParrotDispatcher（ray.remote actor）：TYPE_KEY → handler 分发；ask=ray.get / deliver=不 get（非取消语义，文档明示）
- gateway 主循环：parrot ASK → dispatcher → REPLY；路径映射 `parrot://…/ray/{name}` ↔ ActorHandle

测试：`ray_1000_tasks`（1000 并行 pb 任务派发，总耗时 vs 纯 ray 损耗 <15%——06 §3.4.3 门禁）。

---

## 6. E5 · Receptionist ACL（06 §3.5）

- `acl.yaml`：role（证书 CN 绑定）→ 允许 register/subscribe 的 scope 前缀
- 执行点：receptionist 注册/订阅入口（REPLY_ERR code 13 Forbidden）
- 测试：`acl_register_denied` / `acl_subscribe_filtered`（事件流不下发未授权 key）

---

## 7. E6 · erlang 网关正式化（POC 转正）

`poc/remote-poc/erlang-gw/` → `parrot-protocol-erl/`（rebar3 项目）：
- gen_server 管理连接（帧解析 binary 模式匹配——POC 已实证）
- 补齐：TLS（ssl 应用）、golden vectors eunit 断言、receptionist 桥（erlang 进程注册表 ↔ key）
- 测试：`erlang_interop_ask`（Rust↔Erlang 双向 ask，同 akka 门禁口径）+ vectors 三语言矩阵扩 Erlang

---

## 8. E7 · ParrotMqttBridge（07 §4.5 裁定的落地）

### 8.1 结构

```text
ParrotMqttBridge（parrot 节点，topology_role=Border）：
  MQTT 侧：rumqttc AsyncClient（可对接存量 EMQX/Mosquitto 或自身 broker 模式）
  parrot 侧：FrameLink（tcp/quic Transport 接入）
  映射：topic ↔ Receptionist key（"edge/rpa" ↔ "$parrot/edge/rpa"）；MQTT payload = 完整 Parrot 帧（透传）
  QoS：QoS0 ↔ TELL；QoS1 ↔ durable tell（E2 复用——桥就是 CloudProxy 的 MQTT 前端）
```

### 8.2 测试义务

- `mqtt_qos0_tell` / `mqtt_qos1_durable`（断线重连后 QoS1 消息零丢——复用 E2 断言集）
- `mqtt_topic_key_mapping`（双向注册/发现）
- `mqtt_existing_broker`（对接 EMQX 容器的互操作冒烟）

---

## 9. DoD

1. Rust workspace 全绿；TS jest 绿（含 vectors）；Python pytest 绿；Erlang eunit 绿——四语言 CI 矩阵
2. `durable_offline_5min`、`ray_1000_tasks` <15%、`backpressure_e2e`、MQTT QoS 映射全过
3. lite bundle <50KB 门禁绿
4. 04 §13 P3 勾选 + 接入矩阵（07 §8.1）TS/erlang/MQTT 三列状态更新

---

## 10. 实现注意事项

1. **lite 的 cid 生成**：`crypto.getRandomValues` u64——JS Number 精度！必须 BigInt 或双 u32 拼接（POC 未覆盖 JS，这是首个 JS 特有坑）。
2. **WebTransport 不可用检测**：`typeof WebTransport === "undefined"` 分支要早于 connect（浏览器/旧 Node 特性检测）。
3. **durable tell 的 seq 空间**：每 (sender,endpoint) 对一个独立 u64 序列，随 proxy 生命周期（重启后 WAL 重放延续）——不要全局 seq（多发送方会洞）。
4. **ray adapter 的 ray.get 超时**：对齐发起方剩余预算（`options.timeout - 已耗时`），防双超时竞态（与 ASK 远端不二次超时同理）。
5. **MQTT keep-alive vs Parrot 心跳**：MQTT 层 keep-alive 与 Wire 心跳**并存不冲突**（桥内两条链路各自维护）；但桥的重连退避取两者 max。
6. **erlang 帧解析的 binary 模式匹配**：`<<Len:32/little, Ver:8, ...>>` 直接对齐 07 §2.1——POC erlang-gw 已写好，转正时只补 TLS 与 vectors。
