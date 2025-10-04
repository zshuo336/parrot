# TECH_DESIGN_07 · Parrot Actor 联邦协议 1.0（正式规范）

> 状态：**协议规范（评审输入）** · 2026-10-04
> 上游：[04 架构](./TECH_DESIGN_04_远程与集群架构.md)（愿景与选型）· [05 P1 实现](./TECH_DESIGN_05_P1远程实现详细设计.md)（帧/传输/远程层）· [06 P2-P4 实现](./TECH_DESIGN_06_P2-P4集群与联邦详细设计.md)（集群/边缘/深化）
> 实证：`poc/Mx-poc/` + `poc/remote-poc/`（15 测试：帧四语言一致、TCP、SWIM、akka/ray/erlang 双向互访、双引擎节点、hub 中继）
> 定位：**本文是 04-06 的协议层统一与联邦扩展**。04-06 的全部架构决策继续有效；本文将其线协议部分收敛为正式 1.0 规范（消除文档间漂移），并补齐 04-06 未覆盖的联邦能力（全局寻址、拓扑模式、跨集群目录、结构化错误）。
> 版本语义：wire `version=0x01` 即本协议 1.0。POC 是验证手段而非兼容约束——1.0 不受 POC 简化边界限制。

---

## 0. 冲突消解记录（04/05/06 与实证的矛盾，逐项裁定）

| # | 矛盾 | 04/05/06 原文 | 实证 | 1.0 裁定 |
|---|---|---|---|---|
| X1 | 帧头长度 | 05 §1.1："头部定长 24 字节"（key_len 在定长头内，path 前固定 20B+4B） | POC 四语言实现为：ver(1)+ft(1)+flags(2)+cid(8)+reserved(8)+path_len(4)=24B 定长（path 前）+ key_len 内联于 path 后；body 固定开销 28B。**跨语言互操作暴露了 24B 文档与字段累加不符** | **采用实证布局**（§2.1）：reserved(8) 是演进空间（1.0 启用 hop 字段）。05 的 24B 表述作废，以本文为准 |
| X2 | 握手/心跳帧码点 | 05 §1.2：HANDSHAKE=0x01/ACK=0x02、HEARTBEAT=0x03/ACK=0x04、SYSTEM_EVENT=0x20、ERROR=0x7F | POC 只实现 0x10-0x14 | **沿用 05 码点分配**（§2.2），POC 未实现≠否决；新增帧只占用空闲码点（0x15、0x21-0x23） |
| X3 | 握手体编码 | 05 §1.4：bincode（P1），P2 网关换 pb（"v1 唯一允许的 wire 变更窗口"） | POC 网关未做握手 | 1.0 一次定死：握手体用**自描述 TLV**（§2.4）——任何语言零依赖可解析，消除双编码窗口 |
| X4 | 错误体格式 | 05 §9：REPLY_ERR 载荷 = RemoteError 的 bincode | POC：纯文本 + 字符串匹配（POC 已知缺陷） | 1.0：结构化 `[u16 code][u16 rsv][utf-8 detail]`（§2.5），错误码表合并 05 映射 + 联邦新增 |
| X5 | 寻址格式 arity | 04 §4.1：`parrot://{node_id}/{system}/user/{uuid}`（无 realm/cluster） | POC：本地路径 + hub 前缀 `/erl/...`（临时形态） | 1.0：**短形=04 原样**（单集群缺省），**全形增加可选 realm/cluster 前缀**（§3.1），两者同一文法 |
| X6 | gRPC 位置 | 04 §5：gRPC 不进数据面（延迟不达标），仅可选管理面 | — | **维持 04 裁定**：核心数据面 = TCP/QUIC/WS；HTTP/gRPC 归入**第三方传输插件**生态位（§4.4），不做核心承诺 |
| X7 | 拓扑 | 04 §10：边缘星型起步 + 云内 full mesh（叙述性） | POC p4b：hub 中继实证 | 1.0 形式化为三模式（hub/mesh/hybrid）可配（§5），hybrid 即 04 叙述的规范化 |
| X8 | 跨集群发现 | 04/06：未定义（06 止于单集群 SWIM+Receptionist） | POC：静态前缀表（雏形） | 1.0 新增 **L4 目录子层**（§6）：Directory 角色 + RESOLVE 帧 + 节点缓存。集群内机制（06）不动 |
| X9 | 路线图 | 04 §13：P1-P4 | 07 初稿：M1-M7（双轨冲突） | 统一为 **P1-P4（04/06 原样）+ P5 联邦 + P6 规模化**（§11） |
| X10 | akka 网关形态 | 06 §2.5：同进程 extension 优先 | POC：独立进程 AkkaGw | 两者皆合法部署形态（§8.1），协议层无差别 |
| X11 | MQTT 定位 | 04/05/06 未定义 | — | MQTT = 桥接传输插件 + 联邦接入网关（§4.5），不替代 Wire、不新增帧类型 |
| X12 | 控制面实现载体 | 04/05/06 未定义（隐含 parrot） | POC：hub 中继已用 parrot 端点实现 | 明确裁定：hub/Directory/路由全部 parrot actor 自举（§6.4），零外部服务 |
| X13 | LiveKit 流媒体定位 | 04/05/06 未定义 | — | 双平面桥接（§8.3）：信令面映射 actor 网络可编程，媒体面 WebRTC 旁路不经邮箱；P5 交付 ParrotLiveKitBridge |
| X14 | 工程实现要求 | 04/05/06 各自零散（性能预算/风险登记） | — | 升格为统一工程验收基线（§14 E1-E4）：世界级质量门禁、超级工业级运维条款、电信级容灾参数（99.999%）、百万级**节点**规模模型（三层扇出）——**适用于整个系统（进程内核心 + 远程/集群/联邦全栈）**；P1-P6 全阶段上位约束 |

---

## 1. 统一分层模型（沿用 04 编号，L4 细分）

```
L5  应用层      业务 actor：thread / actix / akka / ray / erlang / lite(TS/C++) / 未来引擎
L4  联邦层      4a 拓扑：hub 中继 / mesh 直连 / hybrid（可配，§5）
                4b 目录：跨集群寻址 Directory 角色 + 节点缓存（§6）
                4c 网关：akka-bridge(JVM) / ray-adapter(py) / erlang-gw / lite（§8）
L3  集群层      SWIM membership · Receptionist · Sharding · Singleton（06 全文有效）
L2  远程层      RemoteActorRef · 编解码双栈 · 投递语义 · 错误映射（05 §3-9 有效，错误体按 X4 升级）
L1  传输层      Transport trait：TcpTransport / QuicTransport / WsTransport（+插件生态位，§4）
L0  线缆协议    Parrot Wire 1.0（§2，28B body 头 + TLV 握手 + 结构化错误）
```

设计铁律不变（04 §2）：热路径零损（ADR-17）、统一门面（`ParrotActorSystem` + `BoxedActorRef`）、小设备可实现（C++/TS 千行内解析器）。

---

## 2. L0 · Parrot Wire 1.0

### 2.1 帧布局（定稿）

```
偏移   长度  字段             说明
0      4    frame_len        u32 LE，= 后续 body 总长。上限 MAX_FRAME_LEN=16 MiB（握手可协商下调）；超限立即断连（ERROR 帧 + close）
4      1    version          0x01 = 协议 1.0
5      1    frame_type       §2.2
6      2    flags            u16 LE 位域，§2.3
8      8    correlation_id   u64 LE（ASK/REPLY/REPLY_ERR 配对；TELL=0 或用作去重序号）
16     1    hop_count        中继已跳数，起始 0；每经一跳 +1
17     1    hop_limit        上限，默认 8（HELLO 可协商）；hop_count≥hop_limit 丢弃并回 RouteUnreachable
18     6    reserved         u48 保留 0
24     4    path_len         u32 LE
28     var  path             UTF-8 逻辑地址（§3.1）
28+p   4    key_len          u32 LE
..     var  type_key         UTF-8："bin:{crate}::{Type}#v{n}" / "pb:{package}.{Message}"（04 §3 双轨 + 06 I1 版本后缀）
..     var  payload          body_len − 28 − path_len − key_len
```

body 固定开销 28B（= X1 实证布局）。半包/粘包：解码器不足一帧返回"需更多数据"不消费（POC RC1 已锁定行为）；所有整数 LE。

### 2.2 frame_type（统一分配表）

| 码 | 帧 | 语义 | 来源 |
|---|---|---|---|
| 0x01 | HANDSHAKE | 建连首帧，能力协商（§2.4） | 05 §1.2 |
| 0x02 | HANDSHAKE_ACK | 握手应答 | 05 |
| 0x03 | HEARTBEAT | 2s 间隔；5 丢失判半开（05 §2.2） | 05 |
| 0x04 | HEARTBEAT_ACK | 心跳应答 | 05 |
| 0x10 | ASK | 请求-响应（cid 配对） | 05/POC |
| 0x11 | REPLY | 成功响应 | 05/POC |
| 0x12 | REPLY_ERR | 失败响应，载荷 = 结构化错误（§2.5） | 05/POC |
| 0x13 | TELL | 单向 at-most-once（默认） | 05/POC |
| 0x14 | STOP | 远程停止（不中继，节点内生效；akka 网关回 UnsupportedOperation——06 I4 语义缺口保留） | 05/POC |
| 0x15 | FRAGMENT | 大消息分片（cid 复用，flags bit2 标 first/last） | 1.0 新增 |
| 0x20 | SYSTEM_EVENT | 集群控制：MembershipGossip（06 §2.1.4）/ RouteGossip（§5.3）/ ReceptionistSync（06 §2.2）| 05 预留 → 06 启用 → 1.0 扩 RouteGossip |
| 0x21 | RESOLVE_Q | 目录查询：逻辑地址 → 物理端点集 | 1.0 新增（X8） |
| 0x22 | RESOLVE_R | 目录应答（含条目 version） | 1.0 新增 |
| 0x23 | INVALIDATE | Directory → 节点：缓存失效推送 | 1.0 新增 |
| 0x7F | ERROR | 协议级错误（版本不符/帧损坏/码点未知）→ 断连 | 05 |

> **MQTT 码点不新增**：MQTT 与 Parrot Wire 的关系裁定见 §4.5——MQTT 是承载 Parrot Wire 的**桥接传输插件**（MQTT payload = Parrot 帧），不是独立的帧类型；桥接服务 ParrotMqttBridge 用 parrot 自身实现（§6.4 同模式）。

规则（沿用 05 §1.4）：握手前收到非 HANDSHAKE 帧 = 协议违规断连；未知 frame_type = ERROR + 断连（1.0 不做前向兼容容忍，版本升级靠 version 字段协商）。

### 2.3 flags 位域（合并 05 §1.3 + 06 P3.2 ACK 位 + 1.0 增补）

| bit | 名称 | 语义 | 启用 |
|---|---|---|---|
| 0 | COMPRESSED_ZSTD | payload zstd 压缩 | P2（05） |
| 1 | TRACING | 透传 traceparent（payload 头部内嵌） | P2（05） |
| 2 | BATCH / FRAGMENT 尾片 | 批量帧（06 P4.3）/ 分片标记 | P4 / 1.0 |
| 3 | URGENT | 高优出队 | P2（05） |
| 4 | TELL_ACK | durable tell 回执（端侧处理完成回 HEARTBEAT 变体，06 P3.2） | P3（06） |
| 5 | APP_ENCRYPTED | 应用层端到端加密（TLS 之外的联邦域间叠加，可选） | 1.0 可选 |
| 6-15 | 保留 | — | — |

### 2.4 握手（X3 裁定：自描述 TLV，一次定死）

HANDSHAKE/HANDSHAKE_ACK 的 payload 为 TLV 序列：`[u8 tag][u16 len][bytes]`，UTF-8 字符串值。

| tag | 字段 | 类型 | 必填 |
|---|---|---|---|
| 1 | node_id | str | ✅ |
| 2 | realm | str | 联邦部署必填，单集群可省 |
| 3 | cluster | str | 同上 |
| 4 | capabilities | u32 位域：bit0 bincode 栈 / bit1 pb 栈 / bit2 zstd / bit3 quic / bit4 ws | ✅ |
| 5 | max_frame_len | u32 | ✅（取双方 min） |
| 6 | topology_role | u8：0 普通 / 1 hub / 2 border / 3 directory | ✅ |
| 7 | hop_limit | u8 | 缺省 8 |

协商规则（05 §1.4 沿用）：version 必须相等；capabilities 按位与，无公共 codec 栈 → ERROR(NoCommonCodec) 断连；mTLS 在 TLS 层完成，握手帧字段不变，node_id 必须匹配证书 CN/SAN（06 §2.4）。

### 2.5 结构化错误（X4 裁定）

REPLY_ERR payload：`[u16 code][u16 reserved][utf-8 detail]`。

| code | RemoteError | → ActorError 映射（05 §9 策略：不新增公共变体） | 来源 |
|---|---|---|---|
| 1 | ActorNotFound | ActorNotFound | 05/POC |
| 2 | Timeout | TimeoutDetail | 05/POC |
| 3 | Stopped | Stopped | 05/POC |
| 4 | NotRemotable | MessageHandlingError("not remotable: …") | 05/POC |
| 5 | CodecError | MessageHandlingError | 05 |
| 6 | UnknownTypeKey（两端类型不一致） | MessageHandlingError | 1.0（POC 实证场景） |
| 7 | RouteUnreachable | ActorNotFound | 1.0 |
| 8 | ConnectionLost（含中继链路断） | InternalError("remote connection lost") | 05/POC |
| 9 | DirectoryStale | InternalError | 1.0 |
| 10 | Overloaded / QueueFull | InternalError("remote outbound queue full") | 05 + 1.0 快速失败 |
| 11 | NoCommonCodec | InternalError | 05 |
| 12 | ProtocolViolation | InternalError | 05 |
| 13 | Forbidden（receptionist ACL，06 P3.5） | MessageHandlingError | 06 |

golden vectors（05 §1.5）扩展至全部 frame_type + 错误体 + TLV 握手，四语言（Rust/JVM/Python/Erlang）+ 未来 TS/C++ 共用同一向量集（04 R4、06 附录 A 的解法，POC 已给四语言样例）。

---

## 3. 寻址（X5 裁定：一文法两形态）

### 3.1 逻辑地址

```
全形（跨集群/联邦）： parrot://<realm>/<cluster>/<node>/<system>/<path>
短形（单集群缺省）：  parrot://<node>/<system>/<path>            ← 04 §4.1 原样
本地形（不经网络）：  /user/{uuid} · actix://{Type}/{uuid}        ← 现状保留
网关虚拟形：          parrot://akka-gw-1/jvm/user/{akkaPath}      ← 04 §8.1 / POC 实证
```

- realm = 信任边界（mTLS 域）；cluster = SWIM 域（06）；node = 集群成员 ID（04 §7）；system = 引擎子系统名（thread-main / actix / jvm / ray / lite）。
- 短形在跨集群流量出域时由 border 节点补全 realm/cluster 前缀（§5.3）。
- **path 字段即逻辑地址**：帧内直接携带上述任意形态，无需二次解析协议。

### 3.2 解析管线（04 三级 + 联邦第四级）

```
get_actor(path)
 ① 本地 registry（facade，O(1)；双引擎遍历兜底保留）           [04 §4.1 / POC p4a]
 ② 集群节点表前缀匹配（SWIM 成员即路由）                        [04 §4.1 / 06]
 ③ Receptionist key 查询（跨节点服务发现）                       [04 §7.2 / 06 §2.2]
 ④ Directory RESOLVE（跨集群/realm）+ 本地缓存（§6）             [1.0 新增]
 ⑤ 全 miss → RouteUnreachable(code 7)
 失败降级：直连不可达 → 中继（hybrid，§5.2）
```

### 3.3 RemoteActorRef

05 §5 全文有效（结构、ask/deliver/stop/is_alive 语义、reply_to 系统回程路径、超时后迟到 REPLY 查表 miss 丢弃）。1.0 增补：`send` 前置解析 ①-⑤；解析产物缓存于 ref（`NodeHandle`）；hop 字段由传输链路自动维护。

---

## 4. L1 · 传输层

### 4.1 Transport trait（05 §2.1 签名有效）

```rust
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn connect(&self, node: &NodeAddr) -> Result<ConnectionHandle, RemoteError>;
    async fn listen(&self, bind: SocketAddr) -> Result<(), RemoteError>;
    async fn accept(&self) -> Result<ConnectionHandle, RemoteError>;
    fn scheme(&self) -> &'static str;   // "tcp" | "quic" | "ws" | 插件名
}
```

### 4.2 核心数据面载体（裁定见 X6）

| 载体 | Endpoint | 特性 | 阶位 | 状态 |
|---|---|---|---|---|
| TCP | `tcp://host:port` | 集群主干；NODELAY；单连接串行入站（P1 特性，P2 拆 worker 池——06 I2） | P1 | 05 定稿 + POC 实证 |
| QUIC | `quic://host:port` | 边缘默认；流级无 HOL；0-RTT 重连；连接迁移（IP 变更不断流） | P2 | 06 §2.3 定稿 |
| WebSocket | `ws://host:port/path` | 浏览器/WebView TS lite 兜底（06 P3.1 降级链） | P3 | 06 定稿 |
| memory | `mem://` | 测试确定性 | P1 | POC 实证 |

### 4.3 连接生命周期（05 §2.2/§6.4 沿用）

心跳 2s/5 失联判半开；重连退避 1s/2s/4s/8s 上限 30s ±20% 抖动；断开时该节点 pending ask 全部 ConnectionLost 失败（CallbackRegistry::fail_all，POC 实证）；优雅停机 = 停 accept → 排空出站 → 等 pending drain（≤5s）→ 关闭。

### 4.4 插件生态位（非核心承诺）

HTTP/2、gRPC、KCP 等经 `Transport` trait 第三方实现接入（合规内网、代理穿透场景）。核心不交付、不担保延迟预算（04 §5 裁定：gRPC ~300µs-1ms 不达标）。插件命名空间 `ext:{name}://`。

### 4.5 MQTT 关系裁定（X11，1.0 定稿）

**结论：MQTT 不替代 Parrot Wire 作为底层协议，也不在 Parrot Wire 之上重建——MQTT 是承载 Parrot 帧的一种桥接传输插件，与 gRPC/HTTP 同处 §4.4 生态位，但因其边缘场景价值升级为半官方插件。** 三层关系如下：

| 关系命题 | 判定 | 理由 |
|---|---|---|
| MQTT 作为 Parrot 的底层协议（替代 TCP/QUIC）？ | **否决** | ① MQTT broker 中心拓扑与 SWIM 对等 gossip 冲突（成员表会被 broker 单点化）；② MQTT 无 cid 配对语义，ASK/REPLY 配对、心跳半开判定、流控都要在之上重建（等于重新发明 Wire 已有机制）；③ pub/sub 广播模型 ≠ actor 点对点寻址，主题需人工映射且无 `parrot://` 逻辑地址的解析能力；④ QoS1 重传与 ASK 重试叠加造成重复投递放大 |
| Parrot Wire 帧跑在 MQTT 之上？ | **允许（桥接插件）** | MQTT payload = 完整 Parrot 帧（帧内已有 hop/path/cid，无需利用 MQTT 任何语义）。**唯一强场景：存量 MQTT 基础设施**——工厂/车载已有 broker 的环境，设备不新增连接即可入联邦 |
| MQTT 基于 parrot 架构实现（broker 本体）？ | **支持（生态位）** | 见 §6.4 同模式：ParrotMqttBridge 用 parrot 实现——broker 接入面是 actix 引擎（IO 编排），MQTT 主题树映射 Receptionist key（`edge/rpa` ↔ `$parrot/edge/rpa`），桥接是联邦层普通 actor。对外部 MQTT 客户端呈现标准 broker，对 parrot 侧是网关 |

**架构关系一句话**：Parrot Wire 是协议本体（L0，含寻址/配对/心跳/路由）；MQTT 在联邦版图中的正确定位是**联邦层的一种接入网关**（L4c）+ **桥接传输插件**（L4.4 生态位）——就像 akka 网关让存量 JVM 系统入联邦，MQTT 桥让存量 MQTT 设备入联邦。**先有 Parrot 协议，MQTT 才有承载物**——因果方向不可倒置。

**交付形态**：
```text
ParrotMqttBridge（parrot 实现，部署在边缘网关或云端）：
  MQTT 侧：标准 broker 协议（存量设备零改造）或对接已有 EMQX/Mosquitto
  parrot 侧：FrameLink 接入（tcp/quic）
  映射：MQTT topic ↔ Receptionist key；MQTT payload ↔ Parrot 帧（透传）
  QoS 映射：QoS0 ↔ TELL(at-most-once)；QoS1 ↔ durable tell(TELL_ACK+WAL，06 P3.2)
```
列入 P3 交付物（与 ray adapter 同期——边缘接入矩阵的一列）。

---

## 5. L4a · 拓扑层（X7 裁定：04 §10 的形式化）

### 5.1 三模式

```toml
[topology]
mode = "hybrid"            # hub | mesh | hybrid（默认）
relay_fallback = true       # 直连失败自动降级中继
[hub]
bind = "tcp://0.0.0.0:9700"
advertise = "tcp://node-7:9700"   # NAT 后公网地址
[mesh]
direct_min_qos = "lan"
```

| 模式 | 数据面路径 | 适用 | 凭据/依据 |
|---|---|---|---|
| hub 中继 | 全部经 hub 前缀路由转发（cid 改写 + 回程映射） | 网关连接资源受限、审计管控、单机 compose | 04 §10 星型叙述 + POC p4b 实证（akka→parrot→erlang） |
| mesh 直连 | 解析端点后发起方直连 | 云内 parrot 节点、低延迟大流量 | 04 §10 full mesh + POC P3 系列 1×1 直连 |
| hybrid | 控制面星型（Directory/ seeds）+ 数据面直连 + 中继兜底 | **默认**；compose→集群→联邦三阶段平滑演进 | 04 §10 两段叙述的并集；p4b 同测试内两模式并存 |

### 5.2 降级链（hybrid）

```
直连（解析缓存端点）─失败→ 重新 RESOLVE（端点可能迁移）→ 直连
                        └─仍失败→ 就近 border/hub 中继（hop+1，hop_limit 内）
                                   └─不可达→ RouteUnreachable（快速失败，不悬挂）
```

### 5.3 路由表

```
RouteEntry { prefix, next_hop: LinkId, cost, version }
```
- 匹配：最长前缀（POC PrefixRouter::resolve 实证）。
- 来源：静态配置 → SYSTEM_EVENT(RouteGossip)（集群内，随 06 MembershipGossip 同车传播）→ Directory 下发（跨集群前缀 + INVALIDATE 失效）。
- 环路防护：hop_count/hop_limit（POC 为单跳无环；多跳为 1.0 新增风险面，由 hop 硬截止）。

---

## 6. L4b · 目录层（X8：1.0 新增，集群内机制不动）

### 6.1 结论：混合目录

| 层级 | 机制 | 中心化 | 依据 |
|---|---|---|---|
| 集群内 node 寻址 | SWIM gossip（成员表即路由表） | ❌ | 06 §2.1（全文有效） |
| 集群内 actor 寻址 | Receptionist 注册/订阅/事件流 | ❌ | 06 §2.2（全文有效） |
| 跨集群/realm | **Directory 角色**（Raft 3/5 副本） | ✅ HA | 1.0 新增 |
| 全节点 | 本地缓存 + version 校验 | ❌ 读去中心 | 1.0 新增 |

Directory **不是外部依赖**（04 §7.1 否决 etcd 进核心的立场不变）：它是 parrot 节点的一种 topology_role（握手 §2.4 tag6），由 border 节点集群兼任，协议内自举。

### 6.2 Directory 职责与 wire

1. 节点注册：`parrot://prod/eu-1/node-7` → `[tcp://10.0.3.7:9700, quic://..., ws://...]`（多端点并列，客户端按能力选）
2. border 声明：集群 border 注册携带 `parrot://prod/eu-1/#` 前缀
3. Receptionist 全局聚合：各集群上报 key 级可达性（细粒度 actor 表留在集群内——避免目录瓶颈；06 §2.2 语义不变）
4. 版本化下发 + INVALIDATE 推送；健康权威（集群 degraded/removed，联动 §9）

### 6.3 解析与缓存

```
ask(parrot://prod/eu-1/node-7/user/echo)
 → 本地缓存（带 version）─命中且 fresh→ 用缓存端点直连
 │ miss/stale → RESOLVE_Q → Directory（或就近 border 代理）→ RESOLVE_R{endpoints, version}
 → 写缓存（TTL 60s）→ connect → HANDSHAKE → 发帧
 → 直连失败 → 回源 RESOLVE 一次（端点迁移）→ 仍失败 → 中继降级（§5.2）
```
缓存三态：fresh / stale（过期可用，降级标记）/ invalid（INVALIDATE 或连通失败）。version 单调 + 推送失效 + TTL 兜底。**强制缓存**：无缓存则每次 ask 多一跳 RTT 且目录故障放大为全联邦不可用。

### 6.4 控制面服务的 parrot 自举（X12，1.0 裁定：核心中继/目录/路由全部用 parrot 实现）

**裁定：联邦控制面不引入任何外部服务——hub 中继、Directory、路由服务、Receptionist 聚合全部是 parrot 节点上的 actor，parrot 用自己承运自己的控制面。** 这是 04 "统一门面"铁律在联邦层的自然延伸，也规避了 etcd/consul/NATS 类外部依赖（04 §7.1 立场的联邦版）。

| 控制面服务 | parrot 实现形态 | 引擎分工 | 状态与一致性 |
|---|---|---|---|
| **hub 中继（RelayHub actor）** | hub 节点上的常驻 actor：持前缀路由表（RouteEntry），收 ASK/TELL → 最长前缀匹配 → 换 cid 转发 + 回程映射（POC PrefixRouter/relay_frame 的 actor 化） | thread 引擎（CPU 密集的路由查表 + 帧改写） | 路由表 = RouteGossip 收敛视图（最终一致）；hub 无自有状态，可任意多实例 |
| **Directory（目录服务）** | border 节点集群上的 actor 组：`DirectoryStore`（Raft 复制状态机，逻辑地址→端点表的强一致存储）+ `DirectoryApi`（RESOLVE_Q/R 应答、INVALIDATE 推送） | DirectoryStore = thread 引擎（Raft 日志复制）；DirectoryApi = actix 引擎（高并发 IO 应答） | Raft 3/5 副本；条目 version 单调；全灭时节点 stale 缓存续服务（§9） |
| **路由服务（RouteReflector）** | 每集群 border 上的 actor：聚合本集群 SWIM 成员可达性 → 生成 RouteGossip 增量 → 随 MembershipGossip 同车传播 + 上报 Directory | thread 引擎 | 视图 = 本集群成员表投影（无独立状态，随 SWIM 收敛） |
| **Receptionist 聚合** | 06 §2.2 原样（本地表 + gossip 携带 + 事件流），全局聚合版 = 各集群 border 上报 key 级可达性到 Directory | 双引擎皆可 | 最终一致 + 事件流推送 |
| **MQTT 桥（§4.5）** | ParrotMqttBridge：broker 接入面 actix 引擎，主题↔Receptionist key 映射 thread 引擎 | 见 §4.5 | — |

**自举的正确性依据**：控制面流量（握手/心跳/RESOLVE/GOSSIP）与数据面走同一 Wire 帧、同一 Transport——控制面 actor 天然继承全部容灾机制（SWIM 检测 border 死亡 → Directory 副本接管；hub 死亡 → 中继降级链切换）。**没有引导悖论**：节点入网靠静态 seeds（05 §2.3）+ HELLO，不依赖 Directory 先在（Directory 只服务跨集群寻址，集群内寻址走 SWIM/Receptionist 零中心机制）。

**实现落点**：这组 actor 进 `parrot-remote` crate 作为可选组件（`RemoteConfig.roles: Vec<Role>`，Role = Hub | Directory | Border | Plain），复用主线全部基建——这是"联邦层=parrot actor"的一行代码级表达。

---

## 7. L2 · 投递语义与消息契约（04/05 决策汇总，无变更）

| 语义 | 帧 | 机制 | 依据 |
|---|---|---|---|
| at-most-once | TELL 默认 | fire-and-forget；mailbox 接收即完成 | 04 §6 / 05 §5.1 / POC ingress |
| request-response | ASK | cid 配对 + 发起方超时 + 迟到回复安全丢弃；**远端不二次超时**（双超时竞态规避） | 05 §6.1 / POC RC6 实证 |
| at-least-once | ASK + retry_policy（用户显式）；durable tell（TELL_ACK + WAL，云 proxy store-and-forward） | 幂等由业务键去重达成 | 04 §6 / 06 P3.2 |
| exactly-once | **不做**（文档指导幂等组合） | — | 04 §6 明示 |

消息契约：`RemoteMessage` trait + `#[derive(RemoteMessage)]` + inventory 自注册 + 双栈 codec（bin/pb）+ TYPE_KEY 版本后缀（05 §3 全文有效；POC 显式注册模式实证了全部错误路径）。集群层（SWIM 参数/状态机/Receptionist API/QUIC/mTLS/Sharding/Singleton）以 06 为准，本文不重复。

---

## 8. L4c · 联邦运行时接入

### 8.1 接入矩阵（04 §8 + POC 实证合并）

| 运行时 | 接入形态 | 部署 | 语义缺口 | 凭据 |
|---|---|---|---|---|
| akka (JVM) | 协议网关 BridgeActor（parrot-protocol-jvm） | 同进程 extension（优）或独立 sidecar | STOP 不支持（06 I4） | 06 §2.5 设计 + POC AkkaGw 实证双向 ask |
| ray (Python) | 语义适配器（dispatcher actor），定位**计算任务域** | ray head 侧 gateway worker | deliver 非取消语义（06 §3.4.2 明示） | 06 §3.4 设计 + POC ray_gw 实证 |
| erlang/OTP | 协议网关（gen_server 管理连接） | 独立网关或嵌入式 | — | 1.0 新列（POC erlang_gw 实证，04/06 未列——补入联邦版图） |
| **MQTT 设备生态** | ParrotMqttBridge 桥接网关（§4.5），QoS↔投递语义映射 | 边缘网关/云端，对接存量 broker | 主题无 actor 生命周期语义 | 1.0 新列（§4.5 裁定） |
| **LiveKit 流媒体生态** | ParrotLiveKitBridge 双平面桥接（§8.3）：信令↔Parrot Wire，媒体↔WebRTC 自流 | 边缘/云端网关节点 | 媒体面不经 actor 邮箱（旁路直通，见 §8.3） | 1.0 新列（§8.3 裁定） |
| parrot-lite TS/C++ | 协议子集（HANDSHAKE/HEARTBEAT/ASK/REPLY/TELL + receptionist 注册） | 端侧内嵌 | 无 gossip/STOP/SYSTEM_EVENT | 04 §8.3 / 06 P3.1 |
| C++ 经 C ABI | 进程内直调（`pl_*` 函数族） | 机器人主控混合体 | — | 04 §9 |
| 进程内 JVM/Python | — | — | **永久否决**（"跨语言互通的正解是协议不是指针"，04 §9 原理） | 04 §9 |

### 8.2 网关与拓扑的关系

网关自身是 topology_role 中的普通节点（或挂靠 hub）。POC p4b 实证了"网关单连接 + hub 中继"形态：akka/erlang 网关各持一条到 hub 的连接，跨网关流量由 hub 前缀路由中继（cid 改写 + 回程映射）——**任意两运行时互访无需 N×N 直连**。mesh 模式下网关需实现多连接 accept 循环（POC 网关已知边界，正式实现项）。

### 8.3 LiveKit 流媒体关系裁定（X13，1.0 定稿）

**结论：parrot 对 LiveKit 做"协议级支持"，形态是双平面桥接网关（ParrotLiveKitBridge）——流媒体的**控制/信令面**（房间、参与者、发布/订阅、轨道元数据、权限）映射为 parrot actor 网络，可编程、可路由、可联邦；**媒体面**（RTP/RTCP 音视频包）绝不进 actor 邮箱，在桥的 WebRTC 端口上旁路直通。** 由此实现"流媒体的 actor 网络"：网络中每个终端（物理机器人摄像头、虚拟量化机器人的行情播报、软件虚拟人数字人、手机 RPA 屏幕共享）既是一个 parrot 节点（信令可编程），又是一个 LiveKit 语义终端（媒体可播放）。

#### 8.3.1 为什么是双平面桥接，而非"全帧走 Wire"

LiveKit = 信令（WebSocket/JSON-RPC）+ 媒体（WebRTC/SRTP，ICE/DTLS/SRTP 三层）双平面 SFU 协议。媒体面走 Parrot Wire 在两个维度上不成立：

| 维度 | Wire 1.0 承载媒体流 | 裁定 |
|---|---|---|
| 吞吐 | 音视频 50-200 pkt/s/轨道 × N 轨道；每包 28B 帧头 + actor 邮箱入队/出队——邮箱吞吐（万级/s）在几十轨道时即饱和，且热路径零损铁律（04 §2）被持续侵犯 | **否决** |
| 语义 | RTP 序号/时间戳/jitter buffer/NACK/PLI/FEC/带宽估计是**传输层实时语义**，actor at-most-once/at-least-once 投递模型与之正交——重建 RTP 语义 = 在 Wire 上重新发明 WebRTC | **否决** |
| 加密 | SRTP 密钥协商（DTLS）与 mTLS 域体系无关，端到端媒体加密不能被中间 actor 解密重加密（隐私边界） | **否决** |

而信令面天然是 actor 语义：房间=监督者 actor、参与者=子 actor、发布订阅=Receptionist key、权限=ACL——**信令面全量入联邦**。

#### 8.3.2 ParrotLiveKitBridge 架构

```text
                    parrot 联邦                                  LiveKit 生态
┌─────────────────────────────────────────┐   ┌────────────────────────────────────┐
│ RoomSupervisor actor（每房间一个，        │   │ LiveKit Room（信令服务器语义）       │
│   thread 引擎，监督树：房间的生命周期）    │   │                                    │
│  ├── Participant actor（每端一个，        │   │ Participant（终端）                 │
│  │     actix 引擎，IO 编排）              │   │                                    │
│  ├── Track actor（每轨道元数据一个）      │   │ Track（轨道：发布/订阅/静音/分辨率）  │
│  └── Policy actor（权限：谁能订阅谁）      │   │                                    │
│                                         │   │                                    │
│ 信令路径：Parrot Wire 帧（ASK/TELL）⇄    ═══│══▶ WebSocket/JSON-RPC（信令）        │
│   联邦内任意 actor 可 ask "订阅轨道X"     │   │                                    │
│                                         │   │                                    │
│ 媒体路径：旁路（不进任何邮箱）             │   │                                    │
│   [RTP/SRTP 端点对] ◄──── WebRTC ────► 终端（LiveKit 客户端：浏览器/手机/C++）│
└─────────────────────────────────────────┘   └────────────────────────────────────┘
```

**映射表**：

| LiveKit 信令概念 | parrot 实体 | 说明 |
|---|---|---|
| Room | `RoomSupervisor` actor（`parrot://media/room-{id}`） | 生命周期=监督树；房间关闭=supervisor 停子 actor |
| Participant | `Participant` actor（`parrot://media/room-{id}/user/{pid}`） | actix 引擎（IO 编排：信令转发/状态推送） |
| Track（发布） | `Track` actor + Receptionist 注册 `media/track/{room}/{track}` | 轨道元数据（SID/类型/分辨率），**不是媒体包** |
| subscribe/unsubscribe | 对 Track actor 的 ASK（可编程编排：录制策略、转推、AI 消费） | 联邦内任意 actor 都能触发订阅——**这就是流媒体 actor 网络的可编程性** |
| 权限（track permission） | `Policy` actor + 06 P3.5 ACL | 动态授权=给 Policy actor 发消息 |
| 信令事件（participant connected/track muted…） | Track/Participant actor 的状态变迁 → Receptionist 事件流推送 | 云端编排 actor 订阅即感知全网房间动态 |
| 媒体包（RTP/SRTP） | **不映射**——桥内 WebRTC 端口直通 | 唯一例外见 8.3.3 AI 分支 |

#### 8.3.3 典型链路（用户场景：终端=LiveKit 终端播放流媒体）

1. **基础播放**：机器人摄像头（parrot-lite C++）发布轨道 → 桥向联邦注册 `media/track/room-7/cam` → 手机 RPA 端（TS）ask 订阅 → 桥下发 LiveKit subscribe 信令 → 媒体经 WebRTC 从摄像头直达手机。**信令 3 跳皆 actor，媒体 0 跳经 actor。**
2. **AI 消费分支**（可选）：云端 AI actor（说话人识别/行情播报分析）需要媒体内容 → 订阅经桥的**旁挂转码 worker**（非 actor，桥的内部组件：WebRTC → 解码 → 帧级 TELL 流，降低为 1-10 fps 的元数据/张量流）→ 进 actor 网络。**降采样是硬性门槛**：进邮箱的必须是"信息流"不是"媒体流"。
3. **跨集群转推**：`quant/eu-1` 的行情播报轨道经 RelayHub 中继授权给 `media/cn-1` 房间订阅——媒体面由 LiveKit/SFU 层转发（桥到桥），控制授权经 parrot 目录与 ACL——**控制面联邦化，媒体面就近直连**。

#### 8.3.4 与 MQTT 裁定（§4.5）的同构性

LiveKit 桥与 MQTT 桥是同一模式的两个实例：**外部协议生态的接入面 = parrot 实现的桥接网关 actor；数据本体（MQTT 消息/LiveKit 媒体）只在语义允许时进联邦**。MQTT 消息体可整体入联邦（吞吐低），LiveKit 媒体不入（吞吐+实时语义）——分界线是**吞吐密度与实时语义是否超出 actor 模型的设计点**，这条线同时决定了两者映射深度的差异。

**交付**：P5 交付物（联邦网关矩阵第二列；依赖 P2 的 actix IO 引擎成熟度与 P3 的 Receptionist ACL）。桥本体是 parrot 节点（topology_role=Border），与 ParrotMqttBridge 同族部署。

---

## 9. 容灾与故障转移（分层模型，参数并入 05/06 既有值）

| 层 | 故障 | 检测 | 恢复 | 依据 |
|---|---|---|---|---|
| 链路 | 断连/半开 | 心跳 5×2s | pending 全败(code 8) → 退避重连(1-30s±20%) → 期间中继降级 | 05 §2.2/§6.4 + §5.2 |
| actor | 停止/失败 | 本地语义 | supervision（主线既有）；远端回 Stopped(code 3) | 05 §9 / POC |
| 节点 | 崩溃/OOM | SWIM 直接+间接探测（kill -9 ≤3.5s 收敛） | suspect→dead → 成员表 gossip 收敛 → is_alive=false | 06 §2.1（门禁值沿用） |
| 网关 | 运行时崩溃 | 同上（网关即节点） | 目录标 dead；Receptionist 批量 Unregistered 推送；compose restart 拉起后重握手 | 06 §2.2 + §6.2 |
| 集群 | border 失联/分区 | Directory 心跳超时 | degraded → 路由切备用 border → 无备用快速失败(code 7) | 1.0（分区语义 POC bus.partition 实证） |
| Directory | 副本故障 | Raft | 选主继续；全灭 → 节点 stale 缓存继续服务（可配窗口默认 300s） | 1.0 |
| 过载 | 队列水位 | 接收方 | ASK 回 Overloaded(code 10) 快速失败；链路级背压（deliver 挂起→读循环挂起→窗口收缩）天然贯通 | 05 §6.3 / POC 实证 + 1.0 |

故障转移：无状态 actor 由集群重建（Receptionist 推送 → 重新解析 → 新端点）；有状态 actor 状态外部化后回放（04 §7.3 边界）；graceful 退出 = 向邻居 gossip leaving + 向目录注销（1.0 增补）。

---

## 10. 安全（04 §11 + 06 §2.4 有效，联邦增补）

- 数据面 mTLS（TLS1.3 / QUIC 内建）；realm = 信任域；node_id ↔ 证书 CN/SAN 强绑 定。
- 设备首注册签发短期证书（云端 CA CSR）+ 轮换（06 §2.4）。
- 授权：Receptionist key 命名空间 ACL（06 P3.5）+ 路由前缀 ACL（Directory 下发，1.0 增补：哪个 realm/cluster 可访问哪些前缀）。
- 审计：hub 模式天然全流量审计点；SYSTEM_EVENT 记录节点加入/退出/证书轮换（04 §11）。
- P1 即做 mTLS 骨架（自签起步），不做"先明文后加密"（04 R7）。

---

## 11. 路线图（X9 统一：P1-P4 原样 + P5/P6 新增）

| 阶段 | 内容 | 依据 | 出口判据 |
|---|---|---|---|
| P1 Remote MVP | Wire 1.0（28B 头 + TLV 握手 + 结构化错误）+ TcpTransport + RemoteActorRef + 静态种子 + RemoteMessage 宏 + bin 栈 + remote-bench | 05 全文（按本文 X1/X3/X4 修正执行） | RC1-RC8 双跑绿；ask p50<150µs；golden 四语言 |
| P2 集群 | SWIM + Receptionist + QuicTransport + mTLS + akka 网关 + pb 栈 + SYSTEM_EVENT(MembershipGossip) + **K0 远程 spawn 管理协议**（06 §P2.6） | 06 第一部分 | 06 §2 各验收 |
| P3 边缘 | lite TS + durable tell + 反压贯通 + ray adapter + erlang 网关正式化 + **ParrotMqttBridge（§4.5）** + ACL | 06 第二部分 + 1.0（erlang/MQTT 接入） | 06 §3 各验收 + MQTT QoS 映射互操作测试 |
| P4 深化 | sharding + singleton + 批量帧 + C++ lite/C ABI | 06 第三部分 | 06 §4 各验收 |
| **P5 联邦（1.0 新增）** | RESOLVE/INVALIDATE 帧实现 + **控制面 actor 化（§6.4：RelayHub/Directory/RouteReflector 进 parrot-remote 可选角色）** + 节点缓存 + hop 字段 + 拓扑三模式配置 + RouteGossip + 中继降级 + 前缀 ACL + **ParrotLiveKitBridge（§8.3：信令 actor 化 + 媒体旁路）** | 本文 §5/§6/§8.3 | 两套集群经 Directory 互访全链路；p4b 扩展为三模式矩阵；跨 realm 拒绝访问；LiveKit 房间经联邦 actor 编排订阅播放 |
| **P6 规模化（1.0 新增）** | gossip digest 增量/push-pull（n>200，06 I6 ⚠ 复核点转正）+ 路由表分片 + Directory 分片化（§14.4）+ 50 集群 compose 仿真 + 数字孪生百万节点地址遍历 | 06 I6 + 本文 §14.4 | 50 集群收敛与路由正确性；E4 规模参数表达标（G5 门禁） |

每阶段延续方法论：POC 先行（`poc/` 独立 workspace）→ 测试锁定 → 进根 workspace。

---

## 12. 性能预算（04 §12 沿用，联邦增补两行）

| 链路 | 预算 | 参照 |
|---|---|---|
| 本地 thread↔actix ask | ~2-5µs | X-PoC 实测 |
| 同 LAN parrot↔parrot ask RTT | <150µs（目标 100µs） | akka artery 60-100µs |
| 同 LAN tell 单向 | <60µs | — |
| parrot↔akka 网关 ask | <300µs（同机回环含一跳） | 06 §2.5.3 |
| **hub 中继一跳 ask（1.0 增）** | <2×直连 + 中继处理 <100µs | POC p4b 拓扑 |
| **RESOLVE 缓存命中（1.0 增）** | 0 网络开销（本地查表） | §6.3 |
| 端↔云 ask | 网络主导（协议份额 <5%） | 04 §12 |

---

## 13. 六需求 → 规范条款索引

| 需求 | 条款 |
|---|---|
| 1. 框架/运行时无关协议 | §2（Wire 1.0）· §8（接入矩阵）· §1 铁律"小设备可实现" |
| 2. 多互联拓扑可配置 | §5（hub/mesh/hybrid + 降级链 + 路由表） |
| 3. 全局位置无关逻辑地址 | §3.1（一文法两形态 URI） |
| 4. 逻辑→物理转换（中心服务？缓存？） | §6（混合目录：集群 SWIM 零中心 + 跨集群 Directory 角色 Raft HA + 强制节点缓存三态） |
| 5. 容灾/恢复/转移 | §9（七层故障模型）· §7（投递语义三档） |
| 6. 传输可插拔 | §4（核心 TCP/QUIC/WS + ext 插件生态位；gRPC 依 04 裁定不进核心数据面） |
| 增补 · MQTT 生态接入 | §4.5（桥接插件 + 联邦协议网关裁定） |
| 增补 · 流媒体 actor 网络（LiveKit） | §8.3（双平面桥接：信令 actor 化，媒体旁路） |
| 增补 · 工程实现要求（世界级/工业级/电信级/百万级节点） | §14（全系统工程基线：质量、SLO、容灾等级、节点规模模型、验收门禁——约束进程内核心与联邦全栈） |

---

## 14. 工程实现要求（E 系列：世界级架构 · 超级工业级实现 · 电信级容灾 · 百万级节点规模）

> **适用范围声明**：本章 E 系列是**整个 parrot 系统**的工程验收基线——涵盖进程内核心（双引擎/API/过程宏/调度器，即 01-03 的全部范畴）与远程/集群/联邦全栈（04-06 + 本文），**不是只约束联邦协议**。挂载于本文只因 07 是统一规范的汇总点（避免同一基线在多文档重复漂移）；进程内核心同样逐条受 E1-E5 约束（如 E1.1 热路径零损、E1.6 零 warning、E1.7 unsafe 只降不升、E1.8 覆盖率、E2.4 显式时间——全部直接作用于引擎层）。四项要求逐条转化为可测量、可验收的工程条款（E1-E4）+ 实现规约（E5）；无法量化处显式声明设计立场而非空话。对应冲突记录：**X14**。

### 14.1 E1 世界级的架构与代码实现质量、性能与扩展性

**架构质量条款**（延续既有铁律并升格为验收项）：

| # | 条款 | 验收方式 |
|---|---|---|
| E1.1 | 热路径零损：进程内互通不因远程层增加任何抽象层；`ActorRef` trait 签名不变（04 铁律 1，ADR-17） | bench 对比：remote 层引入前后进程内 ask 延迟差 = 0（统计不可分辨） |
| E1.2 | 编解码/路由查表只在"确认目标是远程"后介入 | 代码审查 + 火焰图：本地路径无 codec/route 符号 |
| E1.3 | 分层零环依赖：L0-L4 单向依赖，任何层不得反向调用 | `cargo modules`/依赖图 CI 检查 |
| E1.4 | 帧编解码确定性：同输入恒同字节（golden vectors 四语言+TS/C++ 全矩阵） | CI golden 矩阵（04 R4） |
| E1.5 | 公共 API 稳定性：parrot-api 语义化版本；breaking change 需 ADR | semver-checks CI |

**代码质量条款**（超级工业级基线）：

| # | 条款 | 验收方式 |
|---|---|---|
| E1.6 | 编译零 warning（含测试代码）；`#![deny(missing_docs)]` 公共项全覆盖 | CI `-D warnings` |
| E1.7 | 公共 unsafe 必须带 SAFETY 注释论证；parrot-remote/parrot-api 内 unsafe 数量随版本只降不升 | unsafe 审计脚本 CI（全项目基线 20 处，2026-10-04 核：single_alloc 11 + parrot-api/actor.rs 5 + actix/actor.rs 3 + message_pool 1；其中 actix EngineContextHandle 边界为已知项） |
| E1.8 | 测试金字塔：单元（帧/编解码/状态机）→ 集成（RC1-RC8/集群矩阵）→ 混沌（§14.3）→ 基准（E1.9 门禁）；**每修复一个 bug 必先落地一个复现测试** | 覆盖率门禁：parrot-remote 行覆盖 ≥85%，分支 ≥75%；bug-fix-without-test PR 拒绝 |
| E1.9 | 性能预算是发布门禁不是目标（超标=阻塞发布，05 §10.4 沿用）：LAN ask p50 <150µs / tell <60µs / 网关一跳 <300µs；每 release 跑 bench 回归，性能回退 >5% 需专项评审 | bench CI + 趋势报告 |
| E1.10 | 可观测性内建：每连接/每 actor 的 P50/P99 延迟、帧计数、cid 泄漏计数、路由表版本、缓存命中率全部指标化（metrics trait 出口，Prometheus 格式）；traceparent 透传（flags bit1）贯穿 hop | 验收环境 dashboard 全景可见 |
| E1.11 | 文档与实现同步：每个 public item 文档测试（rustdoc doctest）；协议文档章节号在代码注释中可回查（本文 §x 引用） | doctest CI |
| E1.12 | 无锁优先：热路径（cid 分配/回调表/路由查表）优先 lock-free（AtomicU64/segmented map）；必要时才用锁，且必须标注竞争域 | 代码审查 + 压测下 P99 无锯齿（锁竞争签名） |

**扩展性条款**：

| # | 条款 | 验收方式 |
|---|---|---|
| E1.13 | 引擎可插拔已实证（thread/actix 双引擎，POC p4a）；新引擎接入不动 L0-L4 任何层 | 第三引擎（beam-sched POC 已有雏形）接入验证 |
| E1.14 | 传输/编解码/网关三个维度的插件接口全部稳定公开（Transport trait / MessageCodec / 桥模式） | ext 传输（§4.4）与 MQTT/LiveKit 桥（§4.5/§8.3）以纯外部 crate 实现验证 |
| E1.15 | 规模轴扩展不改协议：从 1 节点（compose）到百万**节点**（§14.4）协议帧与状态机零变更，只变配置与部署形态 | §14.4 规模矩阵验证 |

### 14.2 E2 超级工业级的代码实现

在 E1 质量基线之上，叠加**运维工业级**要求：

| # | 条款 | 验收方式 |
|---|---|---|
| E2.1 | 优雅启停全覆盖：启动=配置校验→bind→握手→gossip 加入（失败自动退避重入）；停机=drain 出站→等 pending（≤5s）→向邻居发 leaving→向目录注销（05 §6.4 + 1.0 graceful 退出） | 启停 1000 次循环测试零悬挂零泄漏 |
| E2.2 | 资源边界全部显式：每连接缓冲上限、每节点 pending ask 上限（65536）、WAL 磁盘配额、gossip 带宽令牌桶——超限行为=快速失败（Overloaded code 10）而非 OOM | 故障注入：缓冲轰炸下内存 RSS 平稳 |
| E2.3 | 反压端到端贯通：邮箱→读循环→TCP 窗口→对端出站队列→对端调用方（05 §6.3 链路）在 QUIC/WS 载体下同语义 | 慢消费者矩阵测试 ×3 载体 |
| E2.4 | 时间是显式参数：所有超时/间隔/退避集中配置（无硬编码 sleep）；测试可注入时钟（tokio::time::pause 模式，POC 已用） | 配置审查 + 确定性测试 |
| E2.5 | 二进制可观测发布：内建 healthz（进程级）/readyz（联邦连通性）/metricsz 端点；core dump 安全（无密钥落盘） | 运维手册演练 |
| E2.6 | 配置单一事实源：parrot-node.toml（拓扑/角色/传输/安全/规模段）+ 配置校验器（启动期拒绝非法组合，如 hop_limit<2 或 directory 副本数为偶数） | 校验器单测全分支 |
| E2.7 | 崩溃面收敛：任何单连接/单帧/单 actor 的畸形输入不得导致节点崩溃（panic 边界=连接级隔离，catch_unwind 于 ingress 层） | fuzz：帧解码器与握手 TLV 解析器持续 fuzz（cargo-fuzz，语料入库） |
| E2.8 | 多租户隔离：realm 间流量/带宽/目录条目按配额隔离；跨 realm 访问默认拒绝（§10 ACL） | 多租户压测矩阵 |

### 14.3 E3 电信级容灾（99.999% 可用性目标）

> 电信级 = 故障是常态假设 + 检测/切换时间有硬承诺 + 无单点 + 数据不丢有契约。**99.999% 是整个系统（节点进程内 + 集群 + 联邦链路）端到端的可用性目标**——进程内核心（调度器/邮箱/监督）故障同属故障预算（E1/E2 的进程内条款是其组成部分），不只是网络与联邦层的事。07 §9 的七层故障模型在电信级参数下收紧：

| 故障层 | 1.0 参数（§9） | **电信级参数（E3 收紧）** | 手段 |
|---|---|---|---|
| 链路半开 | 心跳 2s×5=10s 判定 | **≤1s 判定**（QUIC 内建探活；TCP 载体心跳 200ms×5） | 传输层探活 + 快速 failover |
| 节点宕机 | SWIM ≤3.5s 收敛 | **≤2s 收敛**（probe 250ms + 间接探活 k=5 + suspect 1.5s） | SWIM 参数电信档 |
| 集群失联 | degraded → 切备用 border | **双活 border**（Active-Active，路由秒切，无主备切换窗口） | Directory 边界多宿主 |
| Directory 全灭 | stale 缓存续服务 300s | **≥24h stale 续服务**（可配）+ 降级只读 + 全量重建于复活后 | 本地 WAL 重放 |
| 市电级故障 | — | 节点重启后 **WAL 重放自动重新入网**（durable tell 的节点级扩展：路由表/注册项/checkpoint 落盘） | 节点级 WAL |
| 维护窗口 | — | **滚动升级零中断**：协议版本协商（version 位）允许 N/N-1 混跑一个发布周期 | 版本兼容窗口政策 |

**电信级验收（混沌工程矩阵，CI 夜间跑）**：

| 注入 | 断言 |
|---|---|
| kill -9 单节点/批量 10% 节点 | 2s 内成员收敛；期间 ask 失败率 <1%（中继降级承接）；零悬挂 |
| 网络分区（对称/非对称）30min | 分区两侧各自可用（AP 语义）；愈合后 gossip 收敛 ≤30s；无脑裂副作用（singleton 单实例跨分区唯一） |
| 链路抖动（50% 丢包 60s） | ask P99 退化线性（无雪崩）；重连风暴抑制（退避+抖动） |
| 慢磁盘（WAL fsync 10s） | durable tell 背压传导至云侧（不 OOM 不丢） |
| 时钟跳变 ±60s | 心跳/SWIM 不误杀（时间戳单调化处理） |
| 双杀（Directory 主 + 一副本同时） | Raft 3s 内选出新主；RESOLVE 无感知（客户端重试一次成功） |

**数据契约**：durable tell（at-least-once + 幂等去重）承诺"端侧断电重启后未 ACK 消息零丢失"；ask 无数据契约（调用方语义）；联邦边界流量默认可丢（TELL）除非声明 durable。

### 14.4 E4 大规模网络：100 万+ 节点量级

> **规模对象声明**：本节"百万级"指**百万级节点**（parrot 节点/进程，含边缘 lite 端），不是百万级 actor、也不是百万级连接的泛称。actor 数量在节点之上再放大 1k-100k 倍（亿级 actor 由寻址分层结构性承载，见下）。百万级不是"把参数调大"，是**结构分层**。单 SWIM 域的物理上限（gossip O(n) 带宽 + probe 全表扫描）约 1-5k 节点——百万级必须域分层。

**规模结构（三层扇出）**：

```
联邦 realm（最大单元，跨地域）
 └─ 集群 cluster（SWIM 域，1-5k 节点，全 gossip 对等）        ← 06 机制原样，尺寸封顶
     └─ 节点 node（进程）
         └─ actor（每节点 1k-100k）                            ← 04 §4.1 寻址已含此层级
百万节点 = ~200-500 集群 × ~2-5k 节点
```

**规模参数表（设计目标，P6 仿真验收）**：

| 指标 | 目标 | 依据 |
|---|---|---|
| 集群内收敛（节点加入/退出全网感知） | ≤3.5s（1.0）/ ≤2s（E3 档） | 06 门禁 |
| 跨集群路由收敛 | ≤10s（Directory 下发 + INVALIDATE） | §6 |
| 单 Directory 分片承载 | 100 万条目（内存 ~200MB，含索引） | 条目 128B 级 |
| Directory 分片 | 按 realm/cluster 哈希分片，每分片 Raft 3 副本 | 分片数=集群数级（百级） |
| 节点缓存命中率（稳态） | >99.9%（热路径零 RESOLVE） | TTL+推送失效 |
| 端到端跨集群 ask | 相对直连额外 ≤1 跳中继 + ≤10ms 目录成本（miss 时） | §12 |
| 控制面带宽（每节点） | gossip ≤50KB/s 稳态（1.0 档）；digest 增量后 ≤10KB/s（P6） | 06 I6 |
| 亿级 actor 寻址 | 目录不存 actor 级（Receptionist key 聚合上报，§6.2）；解析在集群内完成 | 结构性免中心化 |

**百万级的结构保证（为什么可行，规模对象=百万级节点）**：

1. **寻址分层**：actor 级寻址永不跨集群查目录（Receptionist 聚合到 key 级，条目数=服务数≪节点数）；节点级寻址才跨集群（条目数=百万级，分片 Raft 各承载 100 万）。
2. **流量分层**：数据面 P2P（任意两节点至多一跳中继，hop_limit=8 兜底）；控制面 gossip 域内封闭、跨域走 border 汇聚——**没有全联邦广播帧**（GOSSIP 携带 hop 限制）。
3. **目录分片无热点**：读走节点缓存（命中率 99.9%+），写走一致性哈希分片——Directory 集群吞吐与集群数线性扩展。
4. **边缘海量接入**：手机/传感器类百万端不进 SWIM 域（parrot-lite 静态连接入点，04 §8.3/06 P3.1 原设计）——**百万端的大部分根本不产生 gossip 负载**，只对接入层扩容（actix 引擎 IO 多路复用 + 接入点水平扩展）。
5. **验证路径（P6）**：单机 docker compose 仿真 50 集群 × 200 节点（容器复用进程模拟）+ 数字孪生压测（**百万节点**逻辑地址空间全遍历路由正确性）——不做百万物理容器。

**规模升级触发器（何时必须做 P6 项）**：

| 触发 | 动作 |
|---|---|
| 集群节点 >2k | gossip 换 digest 增量 + push/pull 对账（06 I6 ⚠ 转 P6） |
| 集群数 >100 | Directory 分片化 + border 路由聚合（supernet） |
| 接入点连接 >10 万 | 接入点 split（TCP accept 分片 + QUIC 多路复用天然支持） |
| 跨域流量 >1Gbps | border 间启用批量帧（06 P4.3）+ zstd（flags bit0） |

### 14.5 验收与门禁汇总（E 系列如何进 CI/发布）

| 门禁 | 时机 | 内容 |
|---|---|---|
| G1 编译质量 | 每 PR | -D warnings / clippy pedantic / semver-checks / unsafe 审计 |
| G2 测试质量 | 每 PR | 覆盖率 ≥85%/75% / RC+集群矩阵 / fuzz 语料增量 |
| G3 性能预算 | 每 release | E1.9 预算表全跑；回退 >5% 阻塞 |
| G4 混沌电信档 | 每夜 | §14.3 六注入矩阵 |
| G5 规模仿真 | P6 里程碑 | 50 集群仿真收敛 + 百万节点地址空间遍历 |
| G6 安全 | 每 release | mTLS 全链路 / 跨 realm 拒绝 / 密钥零落盘扫描 |
| G7 规约审计 | 每 PR | §14.6 E5 全表自动检查（依赖白名单/分层依赖/机制策略分离/结构度规/可观测三态）——见 §14.6 E5.6 |

> 以上五项要求（E1-E5）作为 P1-P6 全阶段的验收上位约束：**任何阶段出口判据（§11）不满足 E 系列对应条款时，视为该阶段未完成**。

### 14.6 E5 · 工业级实现规约（任何实现都必须遵循）

> 适用范围：parrot workspace 全部 crate + POC + 网关（JVM/Erlang/Python/TS/C++）+ 桥（MQTT/LiveKit）。与 E1-E4 并列的第五项上位约束，**对任何"实现"生效——无论谁写、哪个阶段、哪种语言**。五条规约逐项展开：

#### E5.1 依赖选型铁律（机制：选型决策框架；策略：逐库裁定表）

任何第三方库/中间件引入前必须过三问，**决策留档**（记入 `docs/DECISIONS_DEPENDENCIES.md`，PR 必附）：

1. **业界是否有久经考验的库？**（生产规模验证 ≥3 年或头部公司背书）→ 有则**优先使用**，禁止 NIH（Not-Invented-Here）重复造轮。
2. **场景是否匹配？**（性能预算 §12 / 嵌入约束 / 许可证 / 依赖传递面）→ 不匹配则换或自研，须写明不匹配点。
3. **无库或全不匹配 → 自研**，自研件必须按 E1.8 落测试 + 按 E5.5 落可观测，且在决策档标注"自研原因"与"可替换的业界库出现时的迁移路径"。

**裁定表现状**（随选型演进维护，全量见 `docs/DECISIONS_DEPENDENCIES.md`）：

| 领域 | 选择 | 依据 |
|---|---|---|
| tokio runtime | **用**（业界事实标准） | 全生态最长验证、POC 已实证 |
| bincode/protobuf 双栈 | **用** | bincode：Rust 零开销；prost：Google 官方系谱 |
| QUIC | **用 quinn** | Rust QUIC 事实标准，.cloudflare/.discord 生产背书 |
| **Raft** | **自研**（DirectoryStore 内嵌） | 现有库（raft-rs 等）面向通用 KV 复制，与 actor 单一职责 Directory（逻辑地址→端点表，万级条目、读极重写极轻）场景不匹配；且 §6.4 控制面自举要求 Raft 与 actor 生命周期同体（候选者/领导者即 actor 角色），嵌入式库做不到；自研范围**严格限定**在 Raft 日志复制 + 选主（不通用化），est. 1.5k 行 + jepsen 式验证（E1.8） |
| MQTT 桥 | **用 rumqttc** | Rust MQTT 事实标准 |
| mTLS | **用 tokio-rustls + rustls-pemfile** | rustls 系内存安全背书 |
| 序列化 | **用 serde 系** | 不自研 codec |
| 心跳/重连退避 | **自研**（~100 行） | 简单定时器逻辑，引库反而增依赖面 |
| SWIM | **自研**（06 §2.1） | 成员表即路由表的同体设计，外部库做不到 |

```rust
// 反例（禁止）：为"省事"引入通用 Raft 库塞进 DirectoryStore
let raft = raft::Raft::new(config, storage, transport); // ❌ 通用库强加 KV 抽象，目录场景水土不服

// 正例：自研 Raft 只做 Directory 需要的事
struct DirectoryStore { /* Raft 日志复制 + 选主，仅此而已 */ }
```

#### E5.2 高内聚低耦合（模块边界与依赖方向）

**内聚判据**：一个模块只因**同一个原因**被修改（单一职责）。**耦合判据**：跨模块依赖必须显式、单向、最少。

| 规则 | 内容 | 违例信号 |
|---|---|---|
| 分层依赖方向 | 严格单向：`parrot-api` ← `parrot` ← `parrot-remote`；桥/网关只依赖 parrot-api。**禁止反向** | cyclic dep 警告、为了用某工具类型把公共件下沉 |
| 模块归属三问 | 新代码放哪？① 是否热路径（→ 引擎 crate）② 是否跨 crate 公共（→ parrot-api）③ 是否仅本模块用（→ 模块内私有） | 同一函数被 copy 到两处 |
| 接口最小化 | pub 面积最小：默认私有，`#[doc(hidden)]` 不算公共 API；trait 定义与实现分文件 | pub struct 字段全裸 |
| 依赖倒置跨界处 | 跨层调用走 trait（Transport / Codec / Registry），实现注入 | 上层直接 import 下层具体类型 |

```
parrot-api ←── parrot(thread/actix) ←── parrot-remote
     ↑ 对外唯一公共面                网关/桥只准碰 parrot-api
```

#### E5.3 机制与策略分离（合适抽象的核心）

**同一机制只有一个实现载体，策略以可插拔配置/trait 对象注入**。判据：改一个策略不碰机制代码，加一个策略不改既有代码（开闭原则的落地形式）。

| 机制（唯一，稳定） | 策略（多个，演进） | 抽象点 |
|---|---|---|
| 帧编解码（frame.rs） | bin/pb 双栈、压缩算法 | `CodecStack` trait |
| 传输连接（Transport trait） | TCP/QUIC/WS/memory/插件 | `scheme()` 返回值 |
| 跔由查表（PrefixRouter） | 直连/中继/降级 | `RoutePolicy` |
| 投递语义框架 | at-most-once/at-least-once/durable | `DeliveryGuarantee` 枚举 + 行为注入 |
| 监督策略框架 | thread/actix 各策略 | `SupervisionStrategy` trait |
| 背压框架（ADR-12） | Block/DropNewest/Error/… | `BackpressureStrategy` 枚举 |
| 调度框架（ADR-14） | SharedPool/DedicatedThread/Sharded | `SchedulingMode` 枚举 |
| 提醒系统 | 轮询/时间轮/分层时间轮 | TimerWheel 层次 |

```rust
// 机制（唯一，稳定）：投递语义框架
pub enum DeliveryGuarantee { AtMostOnce, AtLeastOnce, Durable }

// 策略（可插拔）：重试策略由用户配置注入，机制不感知具体策略
let retry = RetryPolicy::exponential(3, Duration::from_millis(100));
options.retry_policy = Some(retry);
```

**流水账禁令**：禁止"机制代码里硬编码具体策略"（如 Transport::connect 里写 if tcp/quic 分支）——那是把 N 个策略内联成流水账。正确形态是机制持 trait 对象/枚举 + match 分派到策略实现。

**流水账禁令的边界**：机制内少量枚举 match 分派（如 `DeliveryGuarantee` 三值）是合法内聚，不属流水账；流水账专指**策略堆叠进机制体**（Transport::connect 内嵌 if scheme==tcp/quic/ws 三段重复逻辑）。判据：新增一个策略要改几处？一处都不用改（新增类型实现 trait）= 合格；要改机制文件 = 流水账。

#### E5.4 结构化与模块化（反流水账三原则）

文档与代码同构，两者都禁止流水账：

1. **代码分层金字塔**：概念层（60 秒说清）→ 结构层（模块图）→ 实现层（带行号锚点）→ 操作层（命令/测试）。跳层是大忌。
    - 反例：文档一上来就是 500 行 Cargo.toml 依赖清单或函数体逐行讲解——没人读得下去。
2. **模块自治**：每模块有唯一职责句（"frame.rs 管帧字节 ↔ 结构，不知道任何传输细节"）；模块内高内聚（帧常量/编解码/golden vectors 同居），模块间经显式接口交互。
3. **结构先于细节**：任何文档章节先给结构（表/图/管线），细节挂在其下。07 本身的 §2→§14 组织即此原则的示范。

#### E5.5 可调试、可排错、三态可读（人类 + 机器 + 大模型）

**一切运行时状态必须同时具备三种可读形态**——人类可读（排障时肉眼秒懂）、机器可读（程序可解析可聚合）、大模型可读（LLM agent 零上下文推断语义）：

| 形态 | 载体 | 硬性要求 |
|---|---|---|
| 人类可读 | `Debug`/`Display` 实现 | 帧解码失败必须打印**完整字段级诊断**（偏移/期望值/实际值），禁止裸字节 dump 或 `unwrap` 调试 |
| 机器可读 | 结构化错误体（§2.5：code + detail）、`type_key`（`bin:{crate}::{Type}#v{n}`） | 错误码稳定可编程处理；帧自带类型指纹，抓包即可判两端 schema 漂移 |
| 大模型可读 | 命名自明 + 协议自描述（TLV 握手 tag 语义化：node_id/realm/cluster/capabilities…）+ 文档锚点（本文 §x 引用入代码注释） | LLM 读帧 dump/错误日志/文档三者的任意一个都能独立推断语义，无需人工桥接 |

**排错基础设施要求**（实现必须内建，不是事后补）：

1. **帧级 hex dump 一键开关**（`PARROT_TRACE=frame` 环境变量 → 帧摘要单行输出：`ft=0x13 cid=42 hop=2/8 path=parrot://edge-7/lite/user/x key=pb:Echo.Msg len=1024`——**摘要不是原始字节**，直接可读）。
2. **cid 全链路贯穿**：ASK→REPLY→（中继改写）→回程，任一环节日志可按 cid grep 串起完整生命周期（§2.1 cid 配对 + §5.2 中继 cid 映射表）。
3. **连接状态机显式建模**：connection 有显式状态枚举（Connecting/Handshaking/Active/Draining/Closed），每次迁移打 `tracing` 事件——半开判定（2s×5）、优雅停机（排空 ≤5s）全过程可观测。
4. **golden vectors 即文档**：四语言共用向量集（§2.5 末尾定义，文件 `docs/vectors/wire1.json`）既是测试凭据也是协议人读快照——排障时"实际帧 vs golden 帧"逐字节 diff 即定位协议分歧。
5. **错误必带上下文**：`RemoteError` 系列全部携带定位信息（节点/路径/cid/type_key），映射到 `ActorError` 时保留 detail 字符串（§2.5）——禁止吞上下文的 `err.to_string()` 链。

```rust
// 人类 + 机器 + 大模型三态同源（一个数据结构，三种渲染）
#[derive(Debug)]  // 人类：{:?} 字段级展开
struct FrameDiag { offset: usize, expect: u8, actual: u8, field: &'static str }
impl Display for FrameDiag { /* 人类：'帧偏移 16 处 hop_count=255 超过 hop_limit=8' */ }
impl FrameDiag { fn code(&self) -> u16 { /* 机器：错误码 12 (ProtocolViolation) */ } }
// 大模型：字段名/错误码/文档锚点（§2.1）三重自描述，零上下文可推断
```

#### E5.6 规约执行与审计（G7 门禁如何落地）

| 规约 | 自动检查手段 | 违例处置 |
|---|---|---|
| E5.1 依赖铁律 | `cargo deny`（许可证/重复依赖/安全公告）+ 依赖白名单 diff（新增依赖未附 `DECISIONS_DEPENDENCIES.md` 条目 → PR 拒） | 拒绝合并 |
| E5.2 分层依赖 | `cargo depcheck` / workspace 依赖方向脚本（parrot-api 不得 import parrot 等） | CI 失败 |
| E5.3 机制策略分离 | 架构测试（如 cfg(feature) 矩阵编译 + "新增策略不改机制文件"的 diff 审计） | 评审否决 |
| E5.4 结构化 | 模块职责句检查（每模块头注释必含 `//! 职责：` 一句话）+ 文档锚点 lint | 评审否决 |
| E5.5 三态可读 | `PARROT_TRACE=frame` 冒烟 + 错误上下文断言测试（错误 detail 必含 cid/path） | CI 失败 |

---

---

## 附 · 与既有 ADR / 文档的关系

| 依据 | 本文角色 |
|---|---|
| ADR-1/10/12/14/17（04 附录） | 全部延续，无推翻 |
| 05 §1（帧） | 按 X1/X3/X4 修正后并入 §2（以本文为准） |
| 05 §2-§9（传输/codec/ref/ingress/错误） | 原样有效，§4/§7 引用不重复 |
| 06 全部（SWIM/Receptionist/QUIC/mTLS/网关/边缘/深化） | 原样有效；erlang 网关补入 §8.1；I6 转正为 P6 |
| 04 §9（进程内互通原理） | §8.1 引用（协议不是指针） |
| POC（Mx-poc / remote-poc） | 实证凭据：帧四语言一致、TCP、SWIM、三网关双向、双引擎节点、hub 中继、超时/断连/背压语义 |

> 评审通过后：05 §1 与本文 §2 的差异以本文为准同步修订；本文升级为 ADR-21(federation-protocol-1.0)。
