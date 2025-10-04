//! # Static Typed Track (M4)
//!
//! 双轨编程模型的静态轨规范面：消息类型在**编译期**固定，全程零
//! `Box<dyn Any>`、零 downcast。与动态轨（`ActorRefExt::ask/tell`，
//! 异构/远程场景）并存，无需任何配置选择——编程模型本身即选择。
//!
//! ## 三层结构
//!
//! 1. **协议层**：`TypedReceive<M>`——每个消息类型一个 impl，回复类型
//!    复用现有 `Message::Result`；`TypedAskRef<M>` 是静态 ask 的最小面。
//! 2. **分派层**：`ParrotTypedDispatch`——actor 全部协议的枚举信封
//!    （`Msg`）与回复枚举（`Reply`）；`ParrotMsgVariant<A, M>` 把单个
//!    `M` 绑定到枚举变体（inject/extract，零 Any 的类型级桥）。
//!    由 `#[derive(ParrotTypedActor)]` 自动生成，也可手写。
//! 3. **引擎层**：通道核心在 `parrot_api::typed_channel`（类型化
//!    `flume` 信封 + `TypedActorRef<A, M>` + `into_dyn()` 显式桥接），
//!    各引擎只接线消费循环——thread 跑系统 runtime，actix 跑
//!    arbiter 池。双引擎共享同一份通道实现。
//!
//! 签名风格与库内约定一致：trait 方法直接返回 `BoxedFuture`（见
//! `Actor::receive_message`），不使用 async fn in trait——实现方
//! `Box::pin(async move { .. })`，调用方 `.await`。
//!
//! ## 使用形态（thread / actix 引擎同款 API）
//!
//! ```ignore
//! use parrot_api::message::Message;
//! use parrot_api::typed::TypedReceive;
//! use parrot_api::types::{ActorResult, BoxedFuture};
//!
//! struct Add(u64);
//! impl Message for Add { type Result = u64; }
//!
//! #[derive(ParrotTypedActor)]
//! #[ParrotTypedActor(msgs(Add, Get))]
//! struct Calc { n: u64 }
//!
//! impl TypedReceive<Add> for Calc {
//!     fn receive_typed<'a>(&'a mut self, msg: Add)
//!         -> BoxedFuture<'a, ActorResult<u64>> {
//!         Box::pin(async move { self.n += msg.0; Ok(self.n) })
//!     }
//! }
//!
//! // system.spawn_typed(Calc { n: 0 }, "/calc") -> TypedActorRef<Calc, Add>
//! // r.ask(Add(41)).await -> u64，零 Any 装箱零 downcast
//! // r.ref_for::<Get>()  -> 同一 actor 的 Get 协议视图
//! ```

use crate::errors::ActorError;
use crate::message::Message;
use crate::types::{ActorResult, BoxedFuture};

/// 静态轨消息接收 trait：消息类型 M 与回复类型（`M::Result`）编译期固定。
///
/// 一个 actor 可实现多个 `TypedReceive<M>`（多协议）；derive 宏
/// `#[derive(ParrotTypedActor)]` 收集声明的消息集生成枚举信封与
/// match 分派（见 [`ParrotTypedDispatch`]）。
///
/// 与动态轨 `Actor::receive_message(BoxedMessage)` 的关系：静态轨是
/// 进程内高速通道，不经类型擦除；`into_dyn` 桥接点才装箱。
pub trait TypedReceive<M: Message>: Send + 'static {
    /// 处理一条类型化消息，返回 `M::Result` 类型的回复。
    fn receive_typed<'a>(&'a mut self, msg: M) -> BoxedFuture<'a, ActorResult<M::Result>>;
}

/// actor 的全协议分派面：枚举信封（`Msg`）+ 回复枚举（`Reply`）。
///
/// `#[derive(ParrotTypedActor)]` 按属性声明的消息集生成本 trait 的
/// 实现（枚举定义 + match 分派到各 `TypedReceive<M>`）；引擎的
/// `spawn_typed` 消费本 trait 建类型化通道——通道元素是 `Msg`，
/// 回复是 `Reply`，全链路零 `Any`。
pub trait ParrotTypedDispatch: Send + 'static {
    /// 枚举信封：每个协议消息一个变体。
    type Msg: Send + 'static;

    /// 回复枚举：每个协议回复一个变体（`<M as Message>::Result`）。
    type Reply: Send + 'static;

    /// 分派一条枚举消息到 actor 的对应 `TypedReceive<M>` impl。
    fn dispatch<'a>(&'a mut self, msg: Self::Msg) -> BoxedFuture<'a, ActorResult<Self::Reply>>;
}

/// 单个消息类型 M 到 actor 枚举信封的变体绑定（inject/extract）。
///
/// derive 宏为每个声明的消息生成 `impl ParrotMsgVariant<A> for M`；
/// `TypedActorRef<A, M>` 借此在**类型级**完成 `M ↔ A::Msg`、
/// `A::Reply ↔ M::Result` 的映射——零 Any、零运行时类型检查。
pub trait ParrotMsgVariant<A: ParrotTypedDispatch>: Message {
    /// 把类型化消息注入枚举信封。
    fn inject(msg: Self) -> A::Msg;

    /// 从回复枚举提取 `M::Result`（变体由 inject 对偶保证匹配）。
    fn extract(reply: A::Reply) -> ActorResult<Self::Result>;
}

/// 静态 ask 引用面：`ask::<M>(msg) -> M::Result` 无需调用方标注回复类型。
///
/// 引擎的 `TypedActorRef<A, M>` 实现本 trait 后，调用方写
/// `r.ask(Add(41)).await` 即得 `u64`。
pub trait TypedAskRef<M: Message>: Send + Sync {
    /// 静态 ask：消息直达类型化通道，等待 `M::Result` 回复。
    fn ask<'a>(&'a self, msg: M) -> BoxedFuture<'a, ActorResult<M::Result>>;

    /// 静态 tell：fire-and-forget，入队失败静默丢弃（与动态轨
    /// `ActorRefExt::tell` 的语义一致）。
    fn tell<'a>(&'a self, msg: M) -> BoxedFuture<'a, ()>;
}

/// 变体不匹配兜底错误（derive 生成代码与手写实现共用；属于逻辑
/// bug 而非用户数据错误）。
pub fn variant_mismatch_internal() -> ActorError {
    ActorError::MessageHandlingError(
        "typed reply variant mismatch (inject/extract not dual)".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // 最小静态轨栈：Calc { Add, Get } 双协议
    struct Add(u64);
    impl Message for Add {
        type Result = u64;
    }
    struct Get;
    impl Message for Get {
        type Result = u64;
    }

    struct Calc {
        n: u64,
    }

    impl TypedReceive<Add> for Calc {
        fn receive_typed<'a>(&'a mut self, msg: Add) -> BoxedFuture<'a, ActorResult<u64>> {
            Box::pin(async move {
                self.n += msg.0;
                Ok(self.n)
            })
        }
    }
    impl TypedReceive<Get> for Calc {
        fn receive_typed<'a>(&'a mut self, _msg: Get) -> BoxedFuture<'a, ActorResult<u64>> {
            Box::pin(async move { Ok(self.n) })
        }
    }

    // 手写枚举信封（对等 derive 宏生成物）
    enum CalcMsg {
        Add(Add),
        Get(Get),
    }
    enum CalcReply {
        Add(u64),
        Get(u64),
    }

    impl ParrotTypedDispatch for Calc {
        type Msg = CalcMsg;
        type Reply = CalcReply;
        fn dispatch<'a>(
            &'a mut self,
            msg: Self::Msg,
        ) -> BoxedFuture<'a, ActorResult<Self::Reply>> {
            Box::pin(async move {
                match msg {
                    CalcMsg::Add(m) => {
                        let r = TypedReceive::<Add>::receive_typed(self, m).await?;
                        Ok(CalcReply::Add(r))
                    }
                    CalcMsg::Get(m) => {
                        let r = TypedReceive::<Get>::receive_typed(self, m).await?;
                        Ok(CalcReply::Get(r))
                    }
                }
            })
        }
    }

    impl ParrotMsgVariant<Calc> for Add {
        fn inject(msg: Self) -> CalcMsg {
            CalcMsg::Add(msg)
        }
        fn extract(reply: CalcReply) -> ActorResult<u64> {
            match reply {
                CalcReply::Add(v) => Ok(v),
                _ => Err(variant_mismatch_internal()),
            }
        }
    }
    impl ParrotMsgVariant<Calc> for Get {
        fn inject(msg: Self) -> CalcMsg {
            CalcMsg::Get(msg)
        }
        fn extract(reply: CalcReply) -> ActorResult<u64> {
            match reply {
                CalcReply::Get(v) => Ok(v),
                _ => Err(variant_mismatch_internal()),
            }
        }
    }

    #[tokio::test]
    async fn dispatch_routes_envelope_to_typed_impls() {
        let mut c = Calc { n: 0 };
        let r1 = c.dispatch(CalcMsg::Add(Add(41))).await.unwrap();
        assert!(matches!(r1, CalcReply::Add(41)));
        let r2 = c.dispatch(CalcMsg::Get(Get)).await.unwrap();
        assert!(matches!(r2, CalcReply::Get(41)));
        // 状态跨消息保留
        let r3 = c.dispatch(CalcMsg::Add(Add(1))).await.unwrap();
        assert!(matches!(r3, CalcReply::Add(42)));
    }

    #[tokio::test]
    async fn inject_extract_roundtrip() {
        // Add ↔ CalcReply::Add（初始 n=5，+5 → 10）
        let msg = <Add as ParrotMsgVariant<Calc>>::inject(Add(5));
        let reply = Calc { n: 5 }.dispatch(msg).await.unwrap();
        assert_eq!(<Add as ParrotMsgVariant<Calc>>::extract(reply).unwrap(), 10);
        // Get ↔ CalcReply::Get
        let msg = <Get as ParrotMsgVariant<Calc>>::inject(Get);
        let reply = Calc { n: 7 }.dispatch(msg).await.unwrap();
        assert_eq!(<Get as ParrotMsgVariant<Calc>>::extract(reply).unwrap(), 7);
    }

    #[tokio::test]
    async fn variant_mismatch_yields_internal_error() {
        // Add 的 extract 收到 Get 的 reply → 内部错误
        let e = <Add as ParrotMsgVariant<Calc>>::extract(CalcReply::Get(1)).unwrap_err();
        assert!(e.to_string().contains("variant mismatch"));
        let e2 = <Get as ParrotMsgVariant<Calc>>::extract(CalcReply::Add(2)).unwrap_err();
        assert!(e2.to_string().contains("variant mismatch"));
    }

    #[test]
    fn variant_mismatch_internal_shape() {
        let e = variant_mismatch_internal();
        assert!(matches!(e, ActorError::MessageHandlingError(_)));
        assert!(e.to_string().contains("inject/extract"));
    }

    // TypedAskRef 最小实现（验证 trait 对象面可用）
    struct FakeAskRef;

    impl TypedAskRef<Add> for FakeAskRef {
        fn ask<'a>(&'a self, msg: Add) -> BoxedFuture<'a, ActorResult<u64>> {
            Box::pin(async move { Ok(msg.0 * 2) })
        }
        fn tell<'a>(&'a self, _msg: Add) -> BoxedFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn typed_ask_ref_contract() {
        let r = FakeAskRef;
        assert_eq!(TypedAskRef::<Add>::ask(&r, Add(21)).await.unwrap(), 42);
        TypedAskRef::<Add>::tell(&r, Add(1)).await;
    }
}
