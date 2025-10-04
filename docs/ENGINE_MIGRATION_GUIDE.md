# Parrot 引擎迁移指南（M6 API 中立化）

> 适用版本：M6 之后的 workspace。本文描述 **behaviour / runtime 二分原则**、
> 引擎扩展 trait 模式，以及用户代码从旧 API（`receive_message_with_engine`
> 挂在规范 `Actor` trait）到新 API 的迁移路径。

## 1. 规范面的 5 个擦除符号（引擎中立核心）

`parrot-api::engine` 定义了引擎中立的擦除核心，任何新引擎只需实现这 5 个符号：

| 符号 | 位置 | 职责 |
|------|------|------|
| `ErasedActor` | `parrot_api::engine` | 引擎看到的最小 actor 接口（`receive(BoxedMessage) -> BoxedResult`） |
| `ActorRefErased` | `parrot_api::engine` | 用户侧最小 ref 接口（tell / ask / path） |
| `EngineRuntime` | `parrot_api::engine` | 引擎运行时接口（`spawn_erased`），每引擎一个实现 |
| `BoxedMessage` | `parrot_api::types` | 类型擦除消息（复用 `types::BoxedMessage`） |
| `BoxedResult` | `parrot_api::engine` | 类型擦除结果（`Result<BoxedMessage, String>`） |

## 2. behaviour / runtime 二分原则

- **behaviour**（用户写什么）：`Actor` trait + `TypedReceive<M>`。
  用户只实现 `receive_message` / `init` / 生命周期回调，**不含任何引擎细节**。
- **runtime**（引擎怎么跑）：`EngineRuntime` + 引擎自有调度器。
  引擎通过 `ErasedActor` 边界消费 behaviour，不触碰用户类型细节。

引擎侧扩展（如 actix 的同步快路径）归属**引擎 crate 的扩展 trait**，
不进入规范 trait。这是 M6 语义变更 #5 的核心（见 `SEMANTIC_CHANGES.md`）。

## 3. 引擎扩展 trait 模式（以 actix 为例）

### 3.1 为什么是独立 trait 而非规范 trait 方法

`receive_message_with_engine` 是 actix 的同步 dispatch 快路径，
thread 引擎完全不需要。放进规范 `Actor` trait 会让所有引擎背负
actix 语义（M6 之前的债务）。

### 3.2 `ActixEngineExt` 的定义与实现

```rust
// parrot-api/src/actor.rs（规范面只定义 trait，不给 blanket impl）
pub trait ActixEngineExt: Actor {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        engine_ctx: EngineContextHandle<'a>,
    ) -> Option<ActorResult<BoxedMessage>> {
        None // 默认：不参与快路径，走 receive_message
    }
}
```

实现方式有三种：

**a) derive 自动生成**（`engine = "actix"` 时）：

```rust
#[derive(ParrotActor)]
#[parrot_actor(engine = "actix")]
struct MyActor { /* ... */ }
// 宏自动生成 impl ActixEngineExt for MyActor（转发 handle_message_engine）
```

**b) 手工实现**（需要自定义快路径时）：

```rust
impl ActixEngineExt for MyActor {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        engine_ctx: EngineContextHandle<'a>,
    ) -> Option<ActorResult<BoxedMessage>> {
        // 自定义同步快路径；返回 None 则回落 receive_message
    }
}
```

**c) 不实现**（thread 引擎用户）：完全不感知该 trait。

> ⚠️ 由于稳定版 Rust 无特化（specialization），**不存在**
> `impl<T: Actor> ActixEngineExt for T` 的 blanket impl——它将与
> derive 生成的专属 impl 冲突（E0119）。actix 适配器通过类型约束
> `A: ActixEngineExt` 消费该扩展。

### 3.3 actix 适配器的约束链

`parrot::actix` 中的适配器类型要求：

```rust
impl<A> ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<ActixActor<A>>>
        + ActixEngineExt<Context = ActixContext<ActixActor<A>>>
        + Unpin + 'static,
```

`ActixActorRef<A>` / `spawn_root_typed` 等入口同款约束。

## 4. 迁移清单（旧 → 新）

| 旧写法（M6 前） | 新写法（M6 后） |
|------|------|
| `impl Actor for A { fn receive_message_with_engine(...) }` | `impl ActixEngineExt for A { fn receive_message_with_engine(...) }`（独立 impl 块） |
| 测试直接调 `actor.receive_message_with_engine(...)` | UFCS：`parrot_api::actor::ActixEngineExt::receive_message_with_engine(&mut actor, ...)` |
| thread 引擎 actor 被迫写 no-op 的 `receive_message_with_engine` | 删除（thread 引擎不感知该 trait） |
| derive 默认生成 `parrot::actix::*` 引用 | derive 通过 `__parrot_engine::EngineContext<Self>` 别名面（M1），输出零引擎符号（`scripts/macro_lint.sh` 门禁） |

## 5. 验证门禁

- `scripts/macro_lint.sh`：derive 宏展开输出零引擎符号引用。
- `cargo clippy --workspace --all-targets -- -D warnings`：0 警告。
- `cargo test --workspace`：全绿（507 passed，压测类 `#[ignore]` CI release 补跑）。
- `scripts/miri.sh`：unsafe 密集模块（single_alloc）miri 干净。

## 6. 静态类型轨（M4）双引擎支持

静态类型轨（`TypedActorRef` / `spawn_typed`）的通道机制在规范面
`parrot_api::typed_channel`（flume 类型化信封 + 消费循环体 + ref 面），
**双引擎共享同一份实现**——行为一致性由同一份代码保证：

| 引擎 | spawn 入口 | 消费循环执行器 |
|------|-----------|---------------|
| thread | `ThreadActorSystem::spawn_typed` | 系统 tokio runtime |
| actix | `ActixActorSystem::spawn_typed` | arbiter 池 worker（与动态轨同池 round-robin） |

性能对标（release，echo ask 往返，`bench_m4_actix_parity.rs`）：

| 路径 | 单消息往返 |
|------|-----------|
| 纯 actix（原生 `Addr::send`） | ~12.3 µs |
| parrot 静态轨（actix 引擎） | ~7.0 µs（**0.57x，快 1.8 倍**） |

零 `Envelope` 装箱 + 零 downcast 使静态轨不只"持平"而是显著快于
纯 actix 路径——M4 的性能目标超额达成。

## 7. 新引擎接入速览

1. 为你的 runtime 实现 `EngineRuntime::spawn_erased`（消费 `ErasedActor`）。
2. 提供 `YourEngineActor<A>` 适配器（把 `A: Actor` 的 `receive_message`
   包装为 `ErasedActor::receive`）。
3. **静态轨一行接线**：调用 `parrot_api::typed_channel::typed_channel`
   建通道 + 在你的执行器上跑 `consume_one` 循环（照抄
   `parrot::actix::typed`，~20 行）。
4. 若有引擎专属快路径，定义自己的 `YourEngineExt` trait（模式照抄
   `ActixEngineExt`），不要放进规范面。
5. 在 `ParrotActorSystem` 注册引擎实例（参考 actix 的
   `register_actix_system`）。
