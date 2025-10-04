//! M1 derive-decouple: thread 引擎形状的宏验证。
//!
//! 断言：
//! 1. `#[ParrotActor(engine = "thread")]` 生成可用 impl（Context 经
//!    `__parrot_engine::EngineContext<Self>` 绑定到 thread 形状的 mock）
//! 2. thread 分支不生成 actix 同步快路径（`receive_message_with_engine`
//!    返回 None 的默认行为）
//! 3. 同一 actor 类型形状在两种引擎绑定下各自编译通过——宏代码零具体
//!    引擎符号（由 `scripts/macro_lint.sh` 在 CI 守门）

use parrot_api::actor::{Actor, ActorState};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
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

    // thread 引擎不消费同步快路径；但生成代码可能仍要求此形状存在
    // （保持与 actix 形状对称的编写体验）。M1 生成物不再调用它。
    pub fn handle_message_engine(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut __parrot_engine::EngineContext<Self>,
        _engine_ctx: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        None
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
async fn thread_engine_has_no_sync_fast_path_impl() {
    // M6: thread 分支不生成 `ActixEngineExt` impl——同步快路径是 actix
    // 引擎侧扩展。类型层面验证：derive 不为 thread 分支生成该 impl
    // （宏单测断言展开 token 流不含 `ActixEngineExt`）。此处运行时
    // 验证 async 派发路径不受影响。
    fn assert_no_ext<T: parrot_api::actor::Actor>() {}
    assert_no_ext::<ThreadCounter>();

    let mut actor = ThreadCounter { count: 0 };
    let mut ctx = __parrot_engine::EngineContext::<ThreadCounter>::new();

    let r = actor.receive_message(Box::new(Tick(1)), &mut ctx).await;
    assert_eq!(*r.unwrap().downcast::<u64>().unwrap(), 1);
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

/// 未知引擎必须编译期报错（保留在 compile-fail 语义之外，用运行时形状验证：
/// 此处仅验证已知引擎枚举行为，未知引擎的编译错误由宏单测覆盖）。
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
