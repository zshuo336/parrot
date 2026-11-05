# 02 Parrot 核心概念与第一个 Actor

本章使用真实的 [quickstart 源码](../crates/parrot/examples/quickstart.rs) 建立概念，再把概念映射到 Websearch。建议边读边打开源码，避免把 API 名称当成无法解释的魔法。

## 1. Actor 为什么适合这个例子

Actor 把状态与处理消息的逻辑放在一个对象边界中。外部通过引用发送消息，而不是直接取得内部可变集合。搜索组件拥有倒排表，frontier 拥有 URL 队列；它们对外暴露“请求下一批 URL”“替换文档”“查询”等操作。

这种边界有两个好处：调用方无需知道内部集合怎么组织；组件可以移动到另一种运行时，只要保留协议。但是 Actor 不会自动让磁盘持久化、远程请求恰好执行一次，也不会自动防止队列无限增长。业务仍需定义限额、失败语义和恢复方案。

## 2. 消息与结果类型

quickstart 中的定义：

```rust
#[derive(Debug, Clone, Message)]
#[message(result = "u32")]
struct Increment(u32);

#[derive(Debug, Clone, Message)]
#[message(result = "String")]
struct Describe;
```

`Increment(2)` 表达“增加 2”，其结果是新的计数 `u32`。`Describe` 没有入参，返回说明字符串。结果类型属于消息契约，调用者无需从任意字符串猜测返回值。

本地 typed API 的类型安全并不自动提供跨语言编码。要发往远端，仍需为消息注册 codec，并让接收端实现相同字节布局。Websearch 正是这样做的。

## 3. Actor 的状态和处理函数

核心逻辑可缩写为：

```rust
if let Some(Increment(amount)) = msg.downcast_ref::<Increment>() {
    self.0 += amount;
    Ok(Box::new(self.0))
} else if msg.is::<Describe>() {
    Ok(Box::new(format!("count={}", self.0)))
} else {
    Err(ActorError::MessageHandlingError("unknown message".into()))
}
```

这里是源码片段，完整可编译程序以 quickstart 为准。计数保存在 `self.0`；函数通过识别消息类型选择操作，通过 `ActorResult<BoxedMessage>` 表达业务成功或失败。

未知消息应显式失败，不能随意返回一个看似成功的默认值。在跨语言场景中，回复类型、字段数量和计数也必须校验，否则错误组件可能污染下一阶段。

## 4. 引擎、上下文与 derive

quickstart 给同一组消息提供 Actix 与 thread 两套 Actor 实现。每个模块使用 `__parrot_engine` 指向对应引擎，并用 `ParrotActor` 选择相应上下文。这里的统一是公共消息与调用接口统一，并不意味着所有引擎共享完全一样的内部调度器。

Actix 示例需要 Actix System/arbiter 环境；thread 示例使用 `ThreadActorSystem::shared(Default::default())`，该方法安装 Actor 上下文使用的弱系统引用。不要未经核对就把任意构造器互换。

Websearch 清单宿主实际使用 thread 系统承载 Rust workflow Actor。它并不要求你把所有 Rust 抓取函数重写成细粒度 Actor。

## 5. ask、send 和 tell

quickstart 的 thread 调用：

```rust
let actor = system
    .spawn_root_typed_thread(Counter(0), EmptyConfig)
    .await?;
assert_eq!(ActorRefExt::ask(&actor, Increment(2)).await?, 2);
assert_eq!(ActorRefExt::ask(&actor, Increment(3)).await?, 5);
```

显式选择 `ActorRefExt::ask` 是因为 thread 引用还有固有的 boxed ask 方法。学习时保留示例的写法，能够直接获得消息声明的结果类型。

理解三层确认：

1. admission：消息被本地发送或接收机制接受。
2. business completion：处理函数返回结果。
3. durability：业务定义的持久化操作确认完成。

`tell(...).await` 的完成表示入队/接纳语义，不应当作业务执行完成。Websearch 使用 boxed `send` 请求跨语言回复；即使文档写入返回 `DocumentsAck`，它仍只是内存索引处理完成，后面还要等待持久化检查点。

## 6. 引用不是对象指针

本地引用定位运行时中的 Actor。远程引用还需要节点身份、目标路径和网络链路。业务通过引用表达目标，不直接调用远端对象的方法。

`parrot://节点/路径` 是理解寻址的形式；实际路径由部署回执给出。示例文档里的 `/user/search` 经常是角色示意，当前独立编排器会生成带应用身份和运行代际的组件名。不要在自己的工具中硬编码别人的实际部署地址。

## 7. 引擎、网关、组件和制品分别是什么

- 引擎：Actor 的执行环境，例如 thread、Actix、Akka、BEAM 或 Ray 适配环境。
- 网关：实现 Parrot Wire 与管理协议、接收部署并桥接到具体运行时的进程。
- 组件：被网关或本地宿主部署的一组业务行为，例如 search。
- 制品：组件需要加载的文件和入口，例如 JVM jar、Python 模块、Beam 模块或已链接的 Rust Props 工厂。
- 清单：声明组件、依赖、配置和接线的应用描述。

网关可以保持运行而组件被停止、重建；组件停止也不等于数据目录被删除。进程、Actor、网络连接和磁盘资源有不同生命周期。

## 8. Parrot 在 Websearch 中真正负责什么

Parrot 负责跨语言连接、消息类型路由、请求/回复、远程组件管理，以及清单宿主的应用装配。Websearch 自己负责 HTTP 抓取、robots、文本抽取、索引协议、日志检查点和搜索 UI。

这样的分工意味着你可以复用 Parrot 做图像任务、数据转换或业务服务，但需要为自己的操作定义消息与持久化规则。直接复用 `CheckpointV2` 的名字不会自动获得 Websearch 的恢复正确性。

## 9. 在仓库之外使用 Parrot

最稳妥的第一步是在本仓库运行示例。创建独立 workspace 后，按根 README 配置依赖，并特别处理 vendored Tokio：

```toml
[patch.crates-io]
tokio = { path = "/absolute/path/to/parrot/vendor/tokio" }
```

这个 patch 要写在你自己的 workspace 根部，依赖仓库的 patch 不会自动传递。原因和补丁维护边界见 [Tokio 补丁说明](../vendor/tokio/PARROT_PATCH.md)。路径只是示意，不能原样用于另一台机器。

不要仅凭 Websearch 成功就推断默认 crates.io Tokio、所有 feature 组合和所有引擎等价可用。先以真实最小程序验证你的依赖和引擎组合，再接入远程组件。
