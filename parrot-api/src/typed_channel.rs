//! # 静态类型轨引擎核心（M4 引擎中立层）
//!
//! 把 thread 引擎侧验证过的类型化通道机制（`flume` 信封 + dispatch
//! 消费循环 + `TypedActorRef` 面）提炼为**引擎无关**的核心，供各引擎
//! 一行接线复用：
//!
//! - thread 引擎：消费任务 `tokio::spawn` 在系统 runtime 上
//!   （`parrot::thread::typed::spawn_typed`）。
//! - actix 引擎：消费任务 `Arbiter::spawn` 在 arbiter 池上
//!   （`parrot::actix::typed::spawn_typed`）——多 arbiter 并行与
//!   零 Any 快路径兼得。
//!
//! 引擎差异只在"消费循环跑在哪个执行器上"；通道、信封、ref 面、
//! 超时与错误语义全部共享——双引擎行为一致由同一份代码保证。

use std::marker::PhantomData;
use std::time::Duration;

use crate::address::ActorRef;
use crate::errors::ActorError;
use crate::message::Message;
use crate::typed::{ParrotMsgVariant, ParrotTypedDispatch};
use crate::types::{ActorResult, BoxedFuture, BoxedMessage};

/// 类型化通道上流转的信封：枚举消息 + 可选回复通道。
pub struct TypedEnvelope<A: ParrotTypedDispatch> {
    pub msg: A::Msg,
    pub reply: Option<ReplyTx<A>>,
}

/// 引擎消费循环体：处理一条信封（dispatch + 回复回填）。
///
/// 各引擎的 spawn 实现在自己的执行器上循环调用本函数；引擎无关的
/// 错误语义（actor 错误经 reply 通道回传、通道关闭即退出）在此收敛。
pub async fn consume_one<A: ParrotTypedDispatch>(actor: &mut A, env: TypedEnvelope<A>) {
    let TypedEnvelope { msg, reply } = env;
    let result = actor.dispatch(msg).await;
    if let Some(rtx) = reply {
        let _ = rtx.send_async(result).await;
    }
}

/// 建立类型化通道（有界，容量由引擎配置传入）。
pub fn typed_channel<A: ParrotTypedDispatch>(
    capacity: usize,
) -> (
    flume::Sender<TypedEnvelope<A>>,
    flume::Receiver<TypedEnvelope<A>>,
) {
    flume::bounded::<TypedEnvelope<A>>(capacity.max(1))
}

/// 回复通道的发送端。
pub type ReplyTx<A> = flume::Sender<ActorResult<<A as ParrotTypedDispatch>::Reply>>;
/// 回复通道的接收端。
pub type ReplyRx<A> = flume::Receiver<ActorResult<<A as ParrotTypedDispatch>::Reply>>;

/// 建立回复通道（容量 1：每问一答恰好一个回复）。
pub fn reply_channel<A: ParrotTypedDispatch>() -> (ReplyTx<A>, ReplyRx<A>) {
    flume::bounded::<ActorResult<A::Reply>>(1)
}

/// 静态轨 actor ref：actor 类型 A 与**入口协议** M 编译期固定。
///
/// 引擎中立：持有 flume 类型化 Sender 的克隆，不感知消费方跑在哪个
/// 引擎上。A 必须实现 `ParrotTypedDispatch`（derive 生成枚举信封）；
/// M 必须实现 `ParrotMsgVariant<A>`。ask 时 M 经 `inject` 进枚举、
/// 回复经 `extract` 还原为 `M::Result`——类型级映射，零运行时检查。
pub struct TypedActorRef<A, M>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A>,
{
    tx: flume::Sender<TypedEnvelope<A>>,
    path: String,
    _marker: PhantomData<fn(&A, M)>,
}

impl<A, M> Clone for TypedActorRef<A, M>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A>,
{
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            path: self.path.clone(),
            _marker: PhantomData,
        }
    }
}

impl<A, M> TypedActorRef<A, M>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A>,
{
    /// 引擎 spawn 侧构造（通道由引擎建立后传入）。
    pub fn new(tx: flume::Sender<TypedEnvelope<A>>, path: String) -> Self {
        Self {
            tx,
            path,
            _marker: PhantomData,
        }
    }

    /// actor 路径。
    pub fn path(&self) -> &str {
        &self.path
    }

    /// actor 是否仍在接收（接收端未被 drop）。
    pub fn is_alive(&self) -> bool {
        !self.tx.is_disconnected()
    }

    /// 同一 actor 的另一协议视图：零成本类型改写（共享通道与路径）。
    ///
    /// 前提：M2 在 A 的 derive 声明消息集内（否则 `ParrotMsgVariant`
    /// 无 impl，编译期即报错——零 Any 的安全性在类型级保证）。
    pub fn ref_for<M2: ParrotMsgVariant<A>>(&self) -> TypedActorRef<A, M2> {
        TypedActorRef {
            tx: self.tx.clone(),
            path: self.path.clone(),
            _marker: PhantomData,
        }
    }

    /// 显式桥接到动态轨 `ActorRef` 面：装箱边界各一次。
    ///
    /// `send` 收 `BoxedMessage` → downcast 回 M → inject 进枚举信封 →
    /// 类型化通道 → `Reply` → extract → 装回 `BoxedMessage`。
    pub fn into_dyn(self) -> Box<dyn ActorRef>
    where
        M: Message,
    {
        Box::new(TypedToDynRef { inner: self })
    }
}

impl<A, M> TypedActorRef<A, M>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A> + Message,
{
    /// 静态 ask（默认 5s 超时；无界用 [`TypedActorRef::ask_unbounded`]）。
    pub async fn ask(&self, msg: M) -> ActorResult<M::Result> {
        self.ask_inner(msg, Some(DEFAULT_ASK_TIMEOUT)).await
    }

    /// 无界静态 ask：不设超时，actor 处理完才返回。
    pub async fn ask_unbounded(&self, msg: M) -> ActorResult<M::Result> {
        self.ask_inner(msg, None).await
    }

    /// 显式超时静态 ask。
    pub async fn ask_with_timeout(&self, msg: M, timeout: Duration) -> ActorResult<M::Result> {
        self.ask_inner(msg, Some(timeout)).await
    }

    pub(crate) async fn ask_inner(
        &self,
        msg: M,
        timeout: Option<Duration>,
    ) -> ActorResult<M::Result> {
        let (rtx, rrx) = reply_channel::<A>();
        let env = TypedEnvelope {
            msg: M::inject(msg),
            reply: Some(rtx),
        };
        if let Err(e) = self.tx.send_async(env).await {
            return Err(ActorError::InternalError(format!(
                "typed channel closed for {}: {}",
                self.path, e
            )));
        }
        let reply: A::Reply = match timeout {
            Some(d) => match tokio::time::timeout(d, rrx.recv_async()).await {
                Ok(Ok(r)) => r?,
                Ok(Err(e)) => {
                    return Err(ActorError::ReplyChannelError(format!("typed reply: {}", e)));
                }
                Err(_) => {
                    return Err(ActorError::TimeoutDetail(format!(
                        "typed ask {} timed out",
                        self.path
                    )));
                }
            },
            None => match rrx.recv_async().await {
                Ok(r) => r?,
                Err(e) => return Err(ActorError::ReplyChannelError(format!("typed reply: {}", e))),
            },
        };
        M::extract(reply)
    }

    /// 静态 tell：fire-and-forget（入队失败返回 Err，与动态轨 deliver 对齐）。
    pub async fn tell(&self, msg: M) -> ActorResult<()> {
        let env = TypedEnvelope {
            msg: M::inject(msg),
            reply: None,
        };
        self.tx
            .send_async(env)
            .await
            .map_err(|e| ActorError::InternalError(format!("typed tell {}: {}", self.path, e)))
    }
}

/// 静态轨默认 ask 超时（量级对齐动态轨系统配置默认值）。
pub const DEFAULT_ASK_TIMEOUT: Duration = Duration::from_secs(5);

/// 静态轨也实现规范面的 `TypedAskRef`（调用方无需引入引擎类型）。
impl<A, M> crate::typed::TypedAskRef<M> for TypedActorRef<A, M>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A> + Message,
{
    fn ask<'a>(&'a self, msg: M) -> BoxedFuture<'a, ActorResult<M::Result>> {
        Box::pin(async move { TypedActorRef::ask(self, msg).await })
    }

    fn tell<'a>(&'a self, msg: M) -> BoxedFuture<'a, ()> {
        Box::pin(async move {
            let _ = TypedActorRef::tell(self, msg).await;
        })
    }
}

/// 桥接 ref：静态轨 → 动态轨 `ActorRef` 面。
struct TypedToDynRef<A, M>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A> + Message,
{
    inner: TypedActorRef<A, M>,
}

impl<A, M> std::fmt::Debug for TypedToDynRef<A, M>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A> + Message,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedToDynRef")
            .field("path", &self.inner.path())
            .finish()
    }
}

#[async_trait::async_trait]
impl<A, M> ActorRef for TypedToDynRef<A, M>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A> + Message,
{
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let m = *msg.downcast::<M>().map_err(|_| {
                ActorError::MessageHandlingError(
                    "dynamic-bridge: message type mismatch".to_string(),
                )
            })?;
            let r = self.inner.ask_unbounded(m).await?;
            Ok(Box::new(r) as BoxedMessage)
        })
    }

    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let m = *msg.downcast::<M>().map_err(|_| {
                ActorError::MessageHandlingError(
                    "dynamic-bridge: message type mismatch".to_string(),
                )
            })?;
            let r = match timeout {
                Some(d) => self.inner.ask_with_timeout(m, d).await?,
                None => self.inner.ask_unbounded(m).await?,
            };
            Ok(Box::new(r) as BoxedMessage)
        })
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let m = *msg.downcast::<M>().map_err(|_| {
                ActorError::MessageHandlingError(
                    "dynamic-bridge: message type mismatch".to_string(),
                )
            })?;
            self.inner.tell(m).await
        })
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        // 静态轨停止：actor 任务随通道关闭（所有 ref drop）或系统
        // shutdown 退出；发送端无独立 stop 信号（keep-alive 语义同
        // 动态轨弱引用——不阻止也不强制死亡）。
        Box::pin(async move { Ok(()) })
    }

    fn path(&self) -> String {
        self.inner.path().to_string()
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move { self.inner.is_alive() })
    }

    fn clone_boxed(&self) -> crate::types::BoxedActorRef {
        Box::new(Self {
            inner: self.inner.clone(),
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
