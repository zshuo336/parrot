# 05 跨语言消息与 Wire 原理

本章帮助你独立实现或修改一个业务消息。它不是完整 Wire 规范的替代品；管理协议、帧格式和各语言 codec 仍需查阅对应框架源码与协议文档。

## 1. 连接与远程 Actor 系统

Rust 创建 `RemoteActorSystem`，启动后维护到多个节点的 TCP 长连接。direct 由应用拨号，registry 由网关反拨，连接建立后都要完成握手并登记活跃链路。

握手交换节点身份、能力、帧长度和拓扑等信息。能力协商说明对方声称支持什么，真正操作还要检查返回值。节点身份不是密码，TCP 成功也不是身份认证。

一个节点上的多次业务调用复用连接，不是每条消息创建一个 TCP 会话。连接复用降低开销，也要求实现正确处理请求并发、回复关联、心跳、断连和资源限额。

## 2. 路径、type key 和关联 ID

路径回答“给哪个组件”；type key 回答“执行哪种协议消息”；关联 ID 回答“这个回复属于哪个请求”。三者不能互相代替。

例如同一个 search 路径可以接收 `bin:ws/Search`、`bin:ws/Healthz` 和 `bin:ws/CheckpointV2`。收到 Search 的回复时，还要检查预期回复类型，不能把 Healthz 的字节当搜索结果解析。

请求等待表关联目标节点，因此某条链路断开时可以失败该节点的等待请求，而不应误伤其他节点。超时只说明调用方没有及时获得可用结果，不能证明远端没有执行操作。

## 3. Rust 为什么需要独立包装类型

[main.rs](../apps/websearch/src/main.rs) 使用消息注册宏将不同 Rust 类型映射到不同 type key。例如 `WsPush` 对应 `bin:ws/Push`。

注册表在发送时根据类型查找编码器，在接收时根据 type key 查找解码器。即使两个消息都携带 `Vec<u8>`，也不应随意共用同一个消息类型，否则类型到 key 的映射会发生歧义。

本应用的 `bin:ws/*` 业务消息使用手工定义的定宽、小端字节协议。**`bin:` 前缀不意味着自动套用 Rust bincode 布局。** 管理消息的编码也不能直接用来推断业务载荷布局。

## 4. 基础字节约定

本章所写整数均为 little-endian。`u16/u32/u64` 分别占 2/4/8 字节。字符串为 `[字节数 u32][UTF-8 字节]`，长度不是 Unicode 字符数，也不是 Python `len(str)`。

例如中文“消息”是两个字符，但 UTF-8 通常占六字节，长度字段应写 6。Python 使用 `struct.pack('<I', len(text.encode('utf-8')))`，不能写字符数量。

每种解析都必须消费完整载荷。多余尾字节不是可随意忽略的内容，因为它可能表示版本不匹配、拼包错误或恶意输入。

## 5. frontier 协议

```text
Push:
  count:u32
  repeat count:
    url:string
    depth:u16
PushAck:
  newly_accepted:u32

Next:
  wanted:u32
Batch:
  count:u32
  repeat count:
    url:string
    depth:u16

Size: 空载荷
SizeR: queued:u32 seen:u32
```

PushAck 是新增计数，不是总队列长度。SizeR 的两个字段都是 u32，不能照搬历史注释写成 u64。Next 得到的批次可能少于 wanted，尤其在当前最浅层任务不足时。

## 6. tokenizer 协议

```text
Tokenize:
  count:u32
  repeat count:
    doc_id:u64
    text:string
Terms:
  count:u32
  repeat count:
    term:string
    doc_id:u64
    tf:u32

IndexStats: 空载荷
IndexStatsR: term_count:u32 posting_count:u64
```

Terms 的一条记录是一个文档中的一个唯一词项。posting_count 统计词项与文档的关联数，不是原文字符数，也不是所有 TF 的总和。

词项回复的校验应先于任何本批文档提交。若错误回复夹带另一个 doc ID，索引器不能替调用方默默接收。

## 7. 完整文档与查询协议

```text
IndexDocuments:
  count:u32
  repeat count:
    doc_id:u64
    url:string
    title:string
    snippet_source:string
    term_count:u32
    repeat term_count:
      term:string
      tf:u32
DocumentsAck: accepted_documents:u32

Search:
  k:u32
  query:string
SearchResult:
  UTF-8 JSON 搜索结果数组
```

Rust 构造摘要来源时只保留有限 Unicode 字符，JVM 再生成有限长度摘要；它不是保留整份网页源文档的归档服务。要做全文下载或审计存档，需要独立设计存储。

还可在源码找到 `DocMeta`、`IndexTerms`、`Flush`、`Clear`、`ListDocs`、`ListTerms`、`CrawlStateV2`、`PrepareIndex`、`CheckpointV2` 等消息。旧的分离式索引消息不是推荐恢复路径；正常抓取使用完整文档写入和版本化检查点。

控制与持久化消息关系到 epoch、generation 和历史验证，不能凭本章几行布局自行简化实现。修改前应同时阅读 Rust 解码、JVM 分派和对应契约测试。

## 8. JSON 回复同样是协议

Search、Healthz 和浏览回复虽然是 JSON，也需要 schema、字段类型、数值范围、字符串转义和完整消费验证。把任意 `serde_json::Value` 渲染进页面会把协议错误延后到更难定位的位置。

大计数必须保持精确整数，不能经过浮点数中转。控制字符与 Unicode 转义需要正确处理，数值格式不应受 JVM 默认 Locale 影响。错误回复应该产生明确 5xx，不能伪装成空结果。

## 9. 有界解码为什么放在业务入口

[protocol.rs](../apps/websearch/src/protocol.rs) 使用 `PayloadReader` 检查数量、最小记录长度、UTF-8、字段边界和尾部。示例中批次最多 65,536 项，传输帧也有长度限制。

只在分配 Vec 后检查长度太晚；恶意 count 会先触发巨大分配。只检查 TCP 帧大小也不够，短载荷可以声称包含数十亿项。因此需要“帧预算 + 字段预算 + 数量预算 + 完整消费”多层验证。

## 10. 添加消息的正确步骤

1. 先写清 type key、请求与回复布局、上限、错误与重试语义。
2. Rust 新增独立消息类型及 codec 注册。
3. 接收语言新增 handler，完成全部输入校验后再改变状态。
4. 调用方校验回复类型、结构与业务计数。
5. 用独立手工字节向量验证双方，而不是让同一个 encoder 给自己出题。
6. 通过真实网关连接跑成功、截断、尾部、未知类型和超时场景。

不兼容改动应采用新版本 key 或明确版本协商，不能把旧 key 下的 u32 改成 u64 后只更新 Rust。Golden vectors 和跨语言测试的目的就是暴露这种分歧。

## 11. 从 Wire 能获得什么，不能获得什么

Wire 提供统一传输和路由，RPC 提供关联回复；它们不自动保证业务事务、磁盘落盘、幂等或跨节点原子提交。连接断开、超时、重连时，副作用是否发生可能不确定。

因此 Websearch 在业务层实现发现日志、完整文档替换和检查点。其他应用也应根据自己的副作用设计恢复标准，而不是仅依赖 send 返回成功。
