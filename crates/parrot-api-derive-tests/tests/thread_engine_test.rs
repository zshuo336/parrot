//! M1 derive-decouple: thread 引擎形状的宏验证。
//!
//! 断言：
//! 1. `#[ParrotActor(engine = "thread")]` 生成可用 impl（Context 经
//!    `__parrot_engine::EngineContext<Self>` 绑定到 thread 形状的 mock）
//! 2. thread actor 不需要用户提供 Actix 同步 handler。
//! 3. 此测试 crate 不依赖 parrot 实现 crate；mock context 下编译并调用
//!    生成代码。真实引擎执行由 parrot 的集成测试和 quickstart 覆盖。

use parrot_api::actor::{Actor, ActorState};
use parrot_api::errors::ActorError;
use parrot_api::types::{ActorResult, BoxedMessage};
use parrot_api_derive::{Message, ParrotActor};

// thread 引擎形状的 mock（镜像 parrot::thread 的别名面形状：
// EngineContext<A> = ThreadContext<A>，无 ActixActor 包装层）
pub mod __parrot_engine {
    pub struct ThreadContext<A> {
        _phantom: std::marker::PhantomData<A>,
    }

    impl<A> Default for ThreadContext<A> {
        fn default() -> Self {
            Self::new()
        }
    }

    impl<A> ThreadContext<A> {
        pub fn new() -> Self {
            Self {
                _phantom: std::marker::PhantomData,
            }
        }
    }

    pub type EngineContext<A> = ThreadContext<A>;
}

#[derive(Message, Clone, Debug)]
#[message(result = "u64")]
pub struct Tick(pub u64);

#[derive(ParrotActor, Debug)]
#[ParrotActor(engine = "thread")]
pub struct ThreadCounter {
    count: u64,
}

impl ThreadCounter {
    pub async fn handle_message(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut __parrot_engine::EngineContext<Self>,
    ) -> ActorResult<BoxedMessage> {
        if let Some(t) = msg.downcast_ref::<Tick>() {
            self.count += t.0;
            return Ok(Box::new(self.count));
        }
        Err(ActorError::MessageHandlingError("unknown".into()))
    }
}

#[test]
fn thread_engine_derive_compiles_and_defaults() {
    let a = ThreadCounter { count: 0 };
    assert_eq!(a.state(), ActorState::Running);
    assert!(!a.use_async_handler(), "thread 引擎默认不同步快路径");
}

#[tokio::test]
async fn thread_engine_derive_dispatches_async() {
    let mut actor = ThreadCounter { count: 0 };
    let mut ctx = __parrot_engine::EngineContext::<ThreadCounter>::new();

    let r = actor.receive_message(Box::new(Tick(41)), &mut ctx).await;
    assert_eq!(*r.unwrap().downcast::<u64>().unwrap(), 41);

    let r = actor.receive_message(Box::new(Tick(1)), &mut ctx).await;
    assert_eq!(*r.unwrap().downcast::<u64>().unwrap(), 42);
}

#[tokio::test]
async fn thread_engine_reports_unknown_message_without_losing_state() {
    let mut actor = ThreadCounter { count: 0 };
    let mut ctx = __parrot_engine::EngineContext::<ThreadCounter>::new();
    let first = actor.receive_message(Box::new(Tick(3)), &mut ctx).await;
    assert_eq!(*first.unwrap().downcast::<u64>().unwrap(), 3);
    let error = actor
        .receive_message(Box::new("unsupported"), &mut ctx)
        .await;
    assert!(matches!(error, Err(ActorError::MessageHandlingError(_))));
    let next = actor.receive_message(Box::new(Tick(4)), &mut ctx).await;
    assert_eq!(*next.unwrap().downcast::<u64>().unwrap(), 7);
}

/// 历史 "tokio" 别名映射到 thread 引擎（兼容旧代码）。
#[derive(ParrotActor, Debug)]
#[ParrotActor(engine = "tokio")]
pub struct TokioAliasActor {
    pub v: u32,
}

impl TokioAliasActor {
    pub async fn handle_message(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut __parrot_engine::EngineContext<Self>,
    ) -> ActorResult<BoxedMessage> {
        Ok(Box::new(self.v))
    }
}

#[test]
fn tokio_alias_resolves_to_thread_kind() {
    let a = TokioAliasActor { v: 0 };
    assert_eq!(a.state(), ActorState::Running);
}

/// thread 引擎接受显式异步选项；这里只验证该标志，不声称覆盖未知引擎。
#[derive(ParrotActor, Debug)]
#[ParrotActor(
    engine = "thread",
    config = "parrot_api::actor::EmptyConfig",
    async_handler = true
)]
pub struct ThreadAsyncOptIn {
    pub v: u32,
}

impl ThreadAsyncOptIn {
    pub async fn handle_message(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut __parrot_engine::EngineContext<Self>,
    ) -> ActorResult<BoxedMessage> {
        if let Some(t) = msg.downcast_ref::<Tick>() {
            self.v += t.0 as u32;
        }
        Ok(Box::new(self.v))
    }
    pub fn handle_message_engine(
        &mut self,
        _m: BoxedMessage,
        _c: &mut __parrot_engine::EngineContext<Self>,
        _e: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        None
    }
}

#[test]
fn thread_async_opt_in_flag() {
    let a = ThreadAsyncOptIn { v: 0 };
    assert!(
        a.use_async_handler(),
        "async_handler = true 在 thread 引擎同样生效"
    );
}
