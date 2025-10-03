//! # 静态类型轨（M4）—— thread 引擎侧接线
//!
//! 双轨编程模型的静态轨：`spawn_typed` 用类型化通道
//! （元素 = actor 的枚举信封 `A::Msg`）直达 actor，全程零
//! `Box<dyn Any>`、零 downcast、零 `AskEnvelope` 装箱。动态轨
//! （`ActorRefExt::ask/tell`）完全不动；`into_dyn` 在显式边界装箱
//! 桥接两轨。
//!
//! M4 引擎中立化：通道机制、`TypedActorRef`、桥接与错误语义全部在
//! 规范面 `parrot_api::typed_channel`（与 actix 引擎共享同一份实现，
//! 双引擎行为一致由同一份代码保证）；本文件只剩 thread 引擎的接线：
//! 消费循环跑在系统 runtime（`tokio::spawn`）上。

pub use parrot_api::typed_channel::{DEFAULT_ASK_TIMEOUT, TypedActorRef, TypedEnvelope};

use parrot_api::message::Message;
use parrot_api::typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive};
use parrot_api::types::ActorResult;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::thread::system::ThreadActorSystem;

impl ThreadActorSystem {
    /// M4: 静态轨 spawn——类型化通道直达 actor，零 Any 全链路。
    ///
    /// actor 任务在系统 runtime 上运行；通道关闭（所有 ref drop）或
    /// 系统 shutdown 时任务退出。返回入口协议 M 的
    /// `TypedActorRef<A, M>`；其它协议用 `ref_for::<M2>()` 派生。
    pub async fn spawn_typed<A, M>(
        self: &Arc<Self>,
        actor: A,
        path: &str,
    ) -> ActorResult<TypedActorRef<A, M>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        let capacity = self.config().default_mailbox_capacity;
        let (tx, rx) = parrot_api::typed_channel::typed_channel::<A>(capacity);
        let mut actor = actor;
        let path_str = path.to_string();

        let runtime = self.runtime_handle.clone();
        runtime.spawn(async move {
            use parrot_api::typed_channel::consume_one;
            while let Ok(env) = rx.recv_async().await {
                consume_one(&mut actor, env).await;
            }
        });

        Ok(TypedActorRef::new(tx, path_str))
    }

    /// M4: 静态轨 spawn（单一协议便捷入口，无需 derive 枚举信封）。
    ///
    /// 适配器 `SingleDispatch` 把单个 `TypedReceive<M>` 包装成
    /// `ParrotTypedDispatch`（枚举只有一个变体），零 Any 语义不变。
    /// 用户消息经 `SingleVariant::wrap` 包装后 ask/tell（覆盖规则
    /// 要求绑定类型为本地类型）。
    pub async fn spawn_typed_single<A, M>(
        self: &Arc<Self>,
        actor: A,
        path: &str,
    ) -> ActorResult<TypedActorRef<SingleDispatch<A, M>, SingleVariant<A, M>>>
    where
        A: TypedReceive<M>,
        M: Message,
    {
        self.spawn_typed::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            SingleDispatch::new(actor),
            path,
        )
        .await
    }
}

/// 单一协议适配器：`TypedReceive<M>` → `ParrotTypedDispatch`。
pub struct SingleDispatch<A, M>(pub(crate) A, PhantomData<fn(M)>);

impl<A, M> SingleDispatch<A, M> {
    pub fn new(actor: A) -> Self {
        SingleDispatch(actor, PhantomData)
    }
}

/// 单变体枚举信封。
pub enum SingleMsg<M> {
    Msg(M),
}

/// 单变体回复枚举。
pub enum SingleReply<R> {
    Reply(R),
}

impl<A, M> ParrotTypedDispatch for SingleDispatch<A, M>
where
    A: TypedReceive<M>,
    M: Message,
{
    type Msg = SingleMsg<M>;
    type Reply = SingleReply<<M as Message>::Result>;

    fn dispatch<'a>(
        &'a mut self,
        msg: Self::Msg,
    ) -> parrot_api::types::BoxedFuture<'a, ActorResult<Self::Reply>> {
        Box::pin(async move {
            let SingleMsg::Msg(m) = msg;
            self.0.receive_typed(m).await.map(SingleReply::Reply)
        })
    }
}

/// 单一协议的变体绑定：本地包装类型满足覆盖规则。
///
/// 直接 `impl ParrotMsgVariant<..> for M` 违反覆盖规则（M 是
/// uncovered 类型参数），因此用户消息 M 由本结构包装实现绑定；
/// `From<M>` 允许用户消息无损进出包装。
pub struct SingleVariant<A, M>(pub(crate) Option<M>, PhantomData<fn(&A)>);

impl<A, M> SingleVariant<A, M>
where
    A: TypedReceive<M>,
    M: Message,
{
    pub fn wrap(msg: M) -> Self {
        SingleVariant(Some(msg), PhantomData)
    }

    pub fn into_inner(self) -> Option<M> {
        self.0
    }
}

impl<A, M> Clone for SingleVariant<A, M>
where
    A: TypedReceive<M>,
    M: Message + Clone,
{
    fn clone(&self) -> Self {
        SingleVariant(self.0.clone(), PhantomData)
    }
}

impl<A, M> Message for SingleVariant<A, M>
where
    A: TypedReceive<M>,
    M: Message,
{
    type Result = <M as Message>::Result;
}

impl<A, M> ParrotMsgVariant<SingleDispatch<A, M>> for SingleVariant<A, M>
where
    A: TypedReceive<M>,
    M: Message,
{
    fn inject(msg: Self) -> SingleMsg<M> {
        match msg.into_inner() {
            Some(m) => SingleMsg::Msg(m),
            None => unreachable!("SingleVariant must carry a message"),
        }
    }

    fn extract(reply: SingleReply<<M as Message>::Result>) -> ActorResult<M::Result> {
        match reply {
            SingleReply::Reply(r) => Ok(r),
        }
    }
}
