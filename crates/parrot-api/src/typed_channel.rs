//! # 静态类型轨引擎核心（M4 引擎中立层）
//!
//! 把 thread 引擎侧验证过的类型化通道机制（`flume` 信封 + dispatch
//! 消费循环 + `TypedActorRef` 面）提炼为**引擎无关**的核心，供各引擎
//! 按各引擎的生命周期策略接线：
//!
//! - thread 引擎：注册到系统，用户 poll 与析构隔离执行，支持有截止时间的停止
//!   （`parrot::thread::typed::spawn_typed`）。
//! - actix 引擎：消费任务 `Arbiter::spawn` 在 arbiter 池上
//!   （`parrot::actix::typed::spawn_typed`）——多 arbiter 并行与
//!   零 Any 快路径兼得。
//!
//! 消息通道、信封和超时语义共享；注册、关闭与生命周期由引擎接线，
//! 不能由共享通道推断两个引擎具备相同的生命周期能力。

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Caller identity captured before mailbox admission, not a handler span.
    pub trace_context: Option<crate::trace::TraceContext>,
}

impl<A: ParrotTypedDispatch> TypedEnvelope<A> {
    pub fn new(msg: A::Msg, reply: Option<ReplyTx<A>>) -> Self {
        Self {
            msg,
            reply,
            trace_context: crate::trace::TraceContext::current(),
        }
    }
}

/// Standalone dispatch-and-reply helper without managed supervision.
///
/// This returns ordinary handler errors to ASK callers and propagates panics.
/// Managed Thread and Actix systems use their own supervised dispatch owner;
/// using this helper directly does not create engine lifecycle guarantees.
pub async fn consume_one<A: ParrotTypedDispatch>(actor: &mut A, env: TypedEnvelope<A>) {
    let TypedEnvelope {
        msg,
        reply,
        trace_context,
    } = env;
    crate::trace::scope(trace_context.map(|parent| parent.child()), async move {
        let mut recording = Some(crate::trace::SpanGuard::current(
            crate::trace::SpanKind::TypedHandler,
            std::any::type_name::<A>(),
        ));
        let mut future =
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| actor.dispatch(msg))) {
                Ok(future) => future,
                Err(payload) => {
                    recording
                        .take()
                        .unwrap()
                        .finish(crate::trace::SpanStatus::Panicked);
                    std::panic::resume_unwind(payload);
                }
            };
        let result = std::future::poll_fn(|cx| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                future.as_mut().poll(cx)
            })) {
                Ok(poll) => poll,
                Err(payload) => {
                    recording
                        .take()
                        .unwrap()
                        .finish(crate::trace::SpanStatus::Panicked);
                    std::panic::resume_unwind(payload);
                }
            }
        })
        .await;
        drop(future);
        recording.take().unwrap().finish_result(&result);
        if let Some(rtx) = reply {
            let _ = rtx.send_async(result).await;
        }
    })
    .await;
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

/// Admission and wake state shared by all protocol views of a managed actor.
/// Engines call `notify_capacity` after dequeuing, and `seal` before disposal.
#[derive(Default)]
pub struct TypedChannelState {
    sealed: AtomicBool,
    admission: std::sync::Mutex<()>,
    available: tokio::sync::Notify,
    closed: tokio::sync::Notify,
}
impl TypedChannelState {
    pub fn is_sealed(&self) -> bool {
        // Lifecycle observation remains independent of the admission mutex.
        // Receiver notifications may query this state or call seal reentrantly.
        self.sealed.load(Ordering::Acquire)
    }
    pub fn seal(&self) {
        {
            let _admission = self.admission.lock().unwrap();
            self.sealed.store(true, Ordering::Release);
        }
        self.available.notify_waiters();
        self.closed.notify_waiters();
    }
    pub fn notify_capacity(&self) {
        self.available.notify_waiters();
    }
    pub async fn closed(&self) {
        let closed = self.closed.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        if !self.is_sealed() {
            closed.await;
        }
    }
    async fn enqueue<T: Send>(&self, tx: &flume::Sender<T>, message: T) -> ActorResult<()> {
        let mut message = crate::owned_input::Input::new(message);
        loop {
            let available = self.available.notified();
            tokio::pin!(available);
            available.as_mut().enable();
            {
                // Keep checking and sending atomic with respect to seal.
                let admission = self.admission.lock().unwrap();
                if self.is_sealed() {
                    drop(admission);
                    return message.finish(Err(ActorError::Stopped));
                }
                let (result, wakes) = tx.try_send_deferred(message.take());
                // The send is committed while admission is held. Its receiver
                // may seal this channel or inspect the queue from wake(), so
                // deliver callbacks only after both channel locks are released.
                drop(admission);
                drop(wakes);
                match result {
                    Ok(()) => return Ok(()),
                    Err(flume::TrySendError::Disconnected(returned)) => {
                        return crate::owned_input::Input::new(returned)
                            .finish(Err(ActorError::Stopped));
                    }
                    Err(flume::TrySendError::Full(returned)) => {
                        message = crate::owned_input::Input::new(returned);
                    }
                }
            }
            available.await;
        }
    }
}

/// Engine-owned lifecycle of a managed typed channel. The callback must join
/// owned cleanup; dropping a stop caller must not cancel the operation.
pub trait TypedActorLifecycle: Send + Sync {
    fn death_watch(&self) -> Option<crate::death_watch::DeathWatch> {
        None
    }
    fn channel(&self) -> &TypedChannelState;
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>>;
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
    tx: std::sync::Arc<flume::Sender<TypedEnvelope<A>>>,
    path: String,
    lifecycle: Option<std::sync::Arc<dyn TypedActorLifecycle>>,
    default_timeout: Duration,
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
            lifecycle: self.lifecycle.clone(),
            default_timeout: self.default_timeout,
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
            tx: std::sync::Arc::new(tx),
            path,
            lifecycle: None,
            default_timeout: DEFAULT_ASK_TIMEOUT,
            _marker: PhantomData,
        }
    }

    /// Bind a channel to its engine's registered lifecycle.
    pub fn with_lifecycle(
        mut self,
        lifecycle: std::sync::Arc<dyn TypedActorLifecycle>,
        default_timeout: Duration,
    ) -> Self {
        self.lifecycle = Some(lifecycle);
        self.default_timeout = default_timeout;
        self
    }

    /// Engines install this once before publishing a managed reference. An
    /// actor declaring ActorTerminated receives it through its normal typed
    /// channel, regardless of this reference's selected entry protocol.
    /// Unsupported protocols remain unregistered as watchers.
    pub fn install_termination_recipient(&self) -> ActorResult<()> {
        let Some(protocol) = crate::owned_input::call(A::termination_protocol)? else {
            return Ok(());
        };
        let lifecycle = self.lifecycle.as_ref().ok_or_else(|| {
            ActorError::InternalError("typed watcher has no lifecycle owner".into())
        })?;
        let handle = lifecycle.death_watch().ok_or_else(|| {
            ActorError::InternalError("typed watcher has no death-watch capability".into())
        })?;
        handle.set_recipient(std::sync::Arc::new(TypedTerminationRecipient::<A> {
            sender: std::sync::Arc::downgrade(&self.tx),
            lifecycle: std::sync::Arc::downgrade(lifecycle),
            protocol,
        }))
    }

    /// Stop the managed actor and await cleanup. Raw channels constructed with
    /// `new` have no lifecycle owner and report an error instead of false success.
    pub async fn stop(&self) -> ActorResult<()> {
        match &self.lifecycle {
            Some(lifecycle) => lifecycle.stop().await,
            None => Err(ActorError::InternalError(
                "typed channel has no lifecycle owner".into(),
            )),
        }
    }

    async fn enqueue(&self, envelope: TypedEnvelope<A>) -> ActorResult<()> {
        match &self.lifecycle {
            Some(lifecycle) => lifecycle.channel().enqueue(&self.tx, envelope).await,
            None => {
                // flume owns an unadmitted envelope while its send future waits.
                // Give cancellation an independent destructor boundary as well.
                let mut pending = crate::owned_input::Input::new(self.tx.send_async(envelope));
                let result = std::future::poll_fn(|cx| {
                    std::future::Future::poll(std::pin::Pin::new(pending.value_mut()), cx)
                })
                .await;
                let result = match result {
                    Ok(()) => Ok(()),
                    Err(flume::SendError(envelope)) => {
                        crate::owned_input::Input::new(envelope).finish(Err(ActorError::Stopped))
                    }
                };
                pending.finish(result)
            }
        }
    }

    /// actor 路径。
    pub fn path(&self) -> &str {
        &self.path
    }

    /// actor 是否仍在接收（接收端未被 drop）。
    pub fn is_alive(&self) -> bool {
        !self.tx.is_disconnected()
            && self
                .lifecycle
                .as_ref()
                .is_none_or(|lifecycle| !lifecycle.channel().is_sealed())
    }

    /// 同一 actor 的另一协议视图：零成本类型改写（共享通道与路径）。
    ///
    /// 前提：M2 在 A 的 derive 声明消息集内（否则 `ParrotMsgVariant`
    /// 无 impl，编译期即报错——零 Any 的安全性在类型级保证）。
    pub fn ref_for<M2: ParrotMsgVariant<A>>(&self) -> TypedActorRef<A, M2> {
        TypedActorRef {
            tx: self.tx.clone(),
            path: self.path.clone(),
            lifecycle: self.lifecycle.clone(),
            default_timeout: self.default_timeout,
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

struct TypedTerminationRecipient<A: ParrotTypedDispatch> {
    sender: std::sync::Weak<flume::Sender<TypedEnvelope<A>>>,
    lifecycle: std::sync::Weak<dyn TypedActorLifecycle>,
    protocol: crate::typed::TerminationProtocol<A::Msg>,
}

impl<A: ParrotTypedDispatch> crate::death_watch::TerminationRecipient
    for TypedTerminationRecipient<A>
{
    fn deliver(&self, event: crate::context::ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let lifecycle = self.lifecycle.upgrade().ok_or(ActorError::Stopped)?;
            let sender = self.sender.upgrade().ok_or(ActorError::Stopped)?;
            lifecycle
                .channel()
                .enqueue(
                    &sender,
                    TypedEnvelope::<A> {
                        msg: (self.protocol)(event)?,
                        reply: None,
                        // A lifecycle notification is not a child of whichever
                        // operation happened to publish the termination event.
                        trace_context: None,
                    },
                )
                .await
        })
    }
}

enum AskTimeout {
    Declared,
    Unbounded,
    Explicit(Duration),
}

impl<A, M> TypedActorRef<A, M>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A> + Message,
{
    /// 静态 ask（托管引用采用系统配置；原始通道默认 5s；无界用 [`TypedActorRef::ask_unbounded`]）。
    pub fn ask(
        &self,
        msg: M,
    ) -> impl std::future::Future<Output = ActorResult<M::Result>> + Send + '_ {
        self.ask_checked(msg, AskTimeout::Declared)
    }

    /// Unbounded static ask. Metadata still validates admission policy.
    pub fn ask_unbounded(
        &self,
        msg: M,
    ) -> impl std::future::Future<Output = ActorResult<M::Result>> + Send + '_ {
        self.ask_checked(msg, AskTimeout::Unbounded)
    }

    /// Static ask with one explicit deadline covering admission and response.
    pub fn ask_with_timeout(
        &self,
        msg: M,
        timeout: Duration,
    ) -> impl std::future::Future<Output = ActorResult<M::Result>> + Send + '_ {
        self.ask_checked(msg, AskTimeout::Explicit(timeout))
    }

    fn ask_checked(
        &self,
        msg: M,
        timeout: AskTimeout,
    ) -> impl std::future::Future<Output = ActorResult<M::Result>> + Send + '_ {
        let mut msg = crate::owned_input::Input::new(msg);
        crate::trace::capture(async move {
            let result = async {
                let options = crate::owned_input::call(|| {
                    crate::message::MessageOptions::for_message(msg.value())
                })?;
                options.validate_single_attempt()?;
                options.validate_fifo()?;
                let timeout = match timeout {
                    AskTimeout::Declared => options.timeout.or(Some(self.default_timeout)),
                    AskTimeout::Unbounded => None,
                    AskTimeout::Explicit(timeout) => Some(timeout),
                };
                self.ask_inner(msg.take(), timeout).await
            }
            .await;
            msg.finish(result)
        })
    }

    pub(crate) async fn ask_inner(
        &self,
        msg: M,
        timeout: Option<Duration>,
    ) -> ActorResult<M::Result> {
        let (rtx, rrx) = reply_channel::<A>();
        let rrx = crate::owned_input::Input::new(rrx);
        let env = TypedEnvelope {
            msg: crate::owned_input::call(|| M::inject(msg))?,
            reply: Some(rtx),
            trace_context: crate::trace::TraceContext::current(),
        };
        // A single deadline covers capacity backpressure and the reply.
        // Dropping this future also cancels a send that has not entered the queue.
        let exchange = async {
            self.enqueue(env).await?;
            let response = async {
                rrx.value().recv_async().await.map_err(|e| {
                    if self
                        .lifecycle
                        .as_ref()
                        .is_some_and(|owner| owner.channel().is_sealed())
                    {
                        ActorError::Stopped
                    } else {
                        ActorError::ReplyChannelError(format!("typed reply: {}", e))
                    }
                })?
            };
            if let Some(lifecycle) = &self.lifecycle {
                tokio::select! {
                    biased;
                    reply = response => reply,
                    _ = lifecycle.channel().closed() => Err(ActorError::Stopped),
                }
            } else {
                response.await
            }
        };
        let reply = match timeout {
            Some(d) => tokio::time::timeout(d, exchange)
                .await
                .map_err(|_| {
                    ActorError::TimeoutDetail(format!("typed ask {} timed out", self.path))
                })
                .and_then(|result| result),
            None => exchange.await,
        };
        let reply = rrx.finish(reply)?;
        crate::owned_input::call(|| M::extract(reply))?
    }

    /// 静态 tell：fire-and-forget（入队失败返回 Err，与动态轨 deliver 对齐）。
    pub fn tell(&self, msg: M) -> impl std::future::Future<Output = ActorResult<()>> + Send + '_ {
        let mut msg = crate::owned_input::Input::new(msg);
        crate::trace::capture(async move {
            let result = async {
                let options = crate::owned_input::call(|| {
                    crate::message::MessageOptions::for_message(msg.value())
                })?;
                options.validate_single_attempt()?;
                options.validate_fifo()?;
                match options.timeout {
                    Some(timeout) => tokio::time::timeout(timeout, self.tell_inner(msg.take()))
                        .await
                        .map_err(|_| ActorError::Timeout)?,
                    None => self.tell_inner(msg.take()).await,
                }
            }
            .await;
            msg.finish(result)
        })
    }

    async fn tell_inner(&self, msg: M) -> ActorResult<()> {
        let env = TypedEnvelope {
            msg: crate::owned_input::call(|| M::inject(msg))?,
            reply: None,
            trace_context: crate::trace::TraceContext::current(),
        };
        self.enqueue(env).await
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
        Box::pin(TypedActorRef::ask(self, msg))
    }

    fn tell<'a>(&'a self, msg: M) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(TypedActorRef::tell(self, msg))
    }
}

fn bridge_message<M: Message>(message: BoxedMessage) -> ActorResult<M> {
    match message.downcast::<M>() {
        Ok(message) => Ok(*message),
        Err(message) => crate::owned_input::Input::new(message).finish(Err(
            ActorError::MessageHandlingError("dynamic-bridge: message type mismatch".into()),
        )),
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
        self.send_with_timeout(msg, None)
    }

    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let mut msg = crate::owned_input::Input::new(msg);
        Box::pin(crate::trace::capture(async move {
            let result = async {
                let message = bridge_message::<M>(msg.take())?;
                self.inner
                    .ask_inner(message, timeout)
                    .await
                    .map(|reply| Box::new(reply) as BoxedMessage)
            }
            .await;
            msg.finish(result)
        }))
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        let mut msg = crate::owned_input::Input::new(msg);
        Box::pin(crate::trace::capture(async move {
            let result = async {
                let message = bridge_message::<M>(msg.take())?;
                self.inner.tell_inner(message).await
            }
            .await;
            msg.finish(result)
        }))
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { self.inner.stop().await })
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

    fn death_watch(&self) -> Option<crate::death_watch::DeathWatch> {
        self.inner
            .lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.death_watch())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ===========================================================================
// 单元测试（引擎中立层全路径：通道/ask 超时/tell/断连/桥接/协议切换）
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    struct EventActor;
    impl ParrotTypedDispatch for EventActor {
        type Msg = crate::context::ActorTerminated;
        type Reply = ();
        fn termination_protocol() -> Option<crate::typed::TerminationProtocol<Self::Msg>> {
            Some(Ok)
        }
        fn dispatch(&mut self, _: Self::Msg) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    impl ParrotMsgVariant<EventActor> for crate::context::ActorTerminated {
        fn inject(event: Self) -> Self {
            event
        }
        fn extract(_: ()) -> ActorResult<()> {
            Ok(())
        }
    }
    struct EventLifecycle {
        watch: crate::death_watch::DeathWatch,
        channel: TypedChannelState,
    }
    impl TypedActorLifecycle for EventLifecycle {
        fn death_watch(&self) -> Option<crate::death_watch::DeathWatch> {
            Some(self.watch.clone())
        }
        fn channel(&self) -> &TypedChannelState {
            &self.channel
        }
        fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async {
                self.channel.seal();
                self.watch.close();
                Ok(())
            })
        }
    }
    #[tokio::test]
    async fn typed_recipient_maps_manual_protocol_without_retaining_channel_or_lifecycle() {
        use std::sync::Arc;
        let (tx, rx) = typed_channel::<EventActor>(1);
        let lifecycle = Arc::new(EventLifecycle {
            watch: crate::death_watch::DeathWatch::new(2),
            channel: TypedChannelState::default(),
        });
        let weak_lifecycle = Arc::downgrade(&lifecycle);
        let watcher = TypedActorRef::<EventActor, crate::context::ActorTerminated>::new(
            tx,
            "/watcher".into(),
        )
        .with_lifecycle(lifecycle.clone(), DEFAULT_ASK_TIMEOUT);
        watcher.install_termination_recipient().unwrap();
        let retained_handle = lifecycle.watch.clone();
        let target = crate::death_watch::DeathWatch::new(2);
        target.watch(&retained_handle).unwrap();
        target
            .terminated(
                crate::context::ActorTerminated {
                    generation: target.id(),
                    path: "/target".into(),
                    reason: crate::DeathReason::Normal,
                },
                Duration::from_secs(1),
            )
            .await;
        let event = rx.try_recv().unwrap();
        assert_eq!(event.msg.generation, target.id());
        assert!(
            event.reply.is_none(),
            "system-event success is bounded admission"
        );
        let live_target = crate::death_watch::DeathWatch::new(2);
        live_target.watch(&retained_handle).unwrap();
        assert_eq!(live_target.subscriptions().0, 1);
        drop(watcher);
        drop(lifecycle);
        assert!(
            rx.is_disconnected(),
            "a recipient must not own the typed sender"
        );
        assert!(
            weak_lifecycle.upgrade().is_none(),
            "a retained watch handle must not form a lifecycle cycle"
        );
        drop(retained_handle);
        assert_eq!(live_target.subscriptions().0, 0);
    }

    #[tokio::test]
    async fn managed_admission_releases_payload_outside_gate_on_rejection() {
        struct Reentrant(std::sync::Arc<TypedChannelState>);
        impl Drop for Reentrant {
            fn drop(&mut self) {
                assert!(
                    self.0.admission.try_lock().is_ok(),
                    "payload destructor ran under admission lock"
                );
            }
        }
        for sealed in [false, true] {
            let state = std::sync::Arc::new(TypedChannelState::default());
            let (tx, rx) = flume::bounded(1);
            drop(rx);
            if sealed {
                state.seal();
            }
            assert!(state.enqueue(&tx, Reentrant(state.clone())).await.is_err());
        }
    }

    #[test]
    fn managed_receiver_wakeup_can_seal_without_deadlocking_admission() {
        const CHILD: &str = "PARROT_TYPED_WAKE_SEAL_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "typed_channel::tests::managed_receiver_wakeup_can_seal_without_deadlocking_admission",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "seal callback contract failed: {status}");
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    let status = child.wait().unwrap();
                    panic!("seal callback deadlocked; isolated child reaped: {status}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        use futures::{task::ArcWake, Future};
        use std::sync::Arc;
        use std::task::Context;
        struct StopOnWake(Arc<TypedChannelState>);
        impl ArcWake for StopOnWake {
            fn wake_by_ref(this: &Arc<Self>) {
                this.0.seal();
            }
        }
        let state = Arc::new(TypedChannelState::default());
        let (tx, rx) = flume::bounded(1);
        let observer = Arc::new(StopOnWake(state.clone()));
        let waker = futures::task::waker_ref(&observer);
        let mut receive = Box::pin(rx.recv_async());
        assert!(receive
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        futures::executor::block_on(state.enqueue(&tx, 42u64)).unwrap();
        assert!(state.is_sealed());
        assert_eq!(futures::executor::block_on(receive).unwrap(), 42);
        assert!(matches!(
            futures::executor::block_on(state.enqueue(&tx, 43u64)),
            Err(ActorError::Stopped)
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn managed_receiver_wakeup_can_query_admission_state() {
        const CHILD: &str = "PARROT_TYPED_WAKE_QUERY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // A lock regression can also block receiver destruction while
            // unwinding. Isolate it so a failed test cannot hang the suite.
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "typed_channel::tests::managed_receiver_wakeup_can_query_admission_state",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(
                        status.success(),
                        "receiver wakeup contract failed: {status}"
                    );
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    let status = child.wait().unwrap();
                    panic!("receiver wakeup deadlocked; isolated child reaped: {status}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        use futures::{task::ArcWake, Future};
        use std::sync::{mpsc, Arc};
        use std::task::Context;
        struct Observer {
            state: Arc<TypedChannelState>,
            entered: mpsc::Sender<()>,
            result: mpsc::Sender<bool>,
        }
        impl ArcWake for Observer {
            fn wake_by_ref(this: &Arc<Self>) {
                let _ = this.entered.send(());
                // Waking a waiting receiver may happen inside try_send. A
                // read-only lifecycle query must not wait for its own sender.
                let sealed = this.state.is_sealed();
                let _ = this.result.send(sealed);
            }
        }
        let state = Arc::new(TypedChannelState::default());
        let (tx, rx) = flume::bounded(1);
        let (entered, wake_entered) = mpsc::channel();
        let (result, observed) = mpsc::channel();
        let observer = Arc::new(Observer {
            state: state.clone(),
            entered,
            result,
        });
        let waker = futures::task::waker_ref(&observer);
        let mut receive = Box::pin(rx.recv_async());
        assert!(receive
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        let owner = state.clone();
        let sender =
            std::thread::spawn(move || futures::executor::block_on(owner.enqueue(&tx, 42u64)));
        wake_entered
            .recv_timeout(Duration::from_secs(3))
            .expect("receiver was not woken");
        assert!(!observed
            .recv_timeout(Duration::from_secs(3))
            .expect("receiver wakeup deadlocked querying admission state"));
        sender.join().unwrap().unwrap();
        assert_eq!(futures::executor::block_on(receive).unwrap(), 42);

        // A waker may be invoked more than once. Start the close observation
        // only after send and receive have both completed.
        while observed.try_recv().is_ok() {}

        // The irreversible close is also visible to a callback invoked by its
        // notification; later admission must fail without losing ownership.
        let mut closed = Box::pin(state.closed());
        assert!(closed
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        state.seal();
        assert!(observed.recv_timeout(Duration::from_secs(3)).unwrap());
        futures::executor::block_on(closed);
        let (tx, rx) = flume::bounded(1);
        assert!(matches!(
            futures::executor::block_on(state.enqueue(&tx, 43u64)),
            Err(ActorError::Stopped)
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn managed_admission_rearms_losing_waiter_after_capacity_is_taken() {
        use std::future::Future;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use std::task::{Context, Poll};
        #[derive(Default)]
        struct Wakes(AtomicUsize);
        impl futures::task::ArcWake for Wakes {
            fn wake_by_ref(this: &Arc<Self>) {
                this.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let state = TypedChannelState::default();
        let (tx, rx) = flume::bounded(1);
        tx.try_send(0).unwrap();
        let wakes = [Arc::new(Wakes::default()), Arc::new(Wakes::default())];
        let wakers = wakes.each_ref().map(futures::task::waker_ref);
        let mut first = Box::pin(state.enqueue(&tx, 1));
        let mut second = Box::pin(state.enqueue(&tx, 2));
        assert!(first
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[0]))
            .is_pending());
        assert!(second
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[1]))
            .is_pending());
        assert_eq!(rx.try_recv().unwrap(), 0);
        state.notify_capacity();
        assert_eq!(wakes[0].0.load(Ordering::SeqCst), 1);
        assert_eq!(wakes[1].0.load(Ordering::SeqCst), 1);
        assert!(matches!(
            first.as_mut().poll(&mut Context::from_waker(&wakers[0])),
            Poll::Ready(Ok(()))
        ));
        // Both waiters woke, but only one obtained the slot. The loser must
        // subscribe again even though its previous notification is already ready.
        assert!(second
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[1]))
            .is_pending());
        assert_eq!(rx.try_recv().unwrap(), 1);
        wakes[1].0.store(0, Ordering::SeqCst);
        state.notify_capacity();
        assert_eq!(
            wakes[1].0.load(Ordering::SeqCst),
            1,
            "losing producer was not rearmed"
        );
        assert!(matches!(
            second.as_mut().poll(&mut Context::from_waker(&wakers[1])),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(rx.try_recv().unwrap(), 2);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn managed_admission_seal_rejects_pending_and_new_producers() {
        let state = TypedChannelState::default();
        let (tx, rx) = flume::bounded(1);
        state.enqueue(&tx, 0).await.unwrap();
        let mut first = Box::pin(state.enqueue(&tx, 1));
        let mut second = Box::pin(state.enqueue(&tx, 2));
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        state.seal();
        assert_eq!(rx.try_recv().unwrap(), 0);
        state.notify_capacity();
        for pending in [first, second, Box::pin(state.enqueue(&tx, 3))] {
            let result = tokio::time::timeout(Duration::from_secs(1), pending)
                .await
                .expect("sealed producer did not complete");
            assert!(matches!(result, Err(ActorError::Stopped)));
        }
        assert!(
            rx.try_recv().is_err(),
            "sealing must win over newly freed capacity"
        );
    }

    #[tokio::test]
    async fn managed_admission_cancelled_waiter_releases_once_without_delivery() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        struct Payload {
            id: u8,
            drops: Arc<AtomicUsize>,
            state: Arc<TypedChannelState>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                assert!(self.state.admission.try_lock().is_ok());
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }
        let state = Arc::new(TypedChannelState::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let payload = |id| Payload {
            id,
            drops: drops.clone(),
            state: state.clone(),
        };
        let (tx, rx) = flume::bounded(1);
        state.enqueue(&tx, payload(0)).await.unwrap();
        let mut cancelled = Box::pin(state.enqueue(&tx, payload(1)));
        assert!(futures::poll!(&mut cancelled).is_pending());
        drop(cancelled);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let admitted = rx.try_recv().unwrap();
        assert_eq!(admitted.id, 0);
        drop(admitted);
        state.notify_capacity();
        assert!(rx.try_recv().is_err());
        state.enqueue(&tx, payload(2)).await.unwrap();
        let admitted = rx.try_recv().unwrap();
        assert_eq!(admitted.id, 2);
        drop(admitted);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn managed_admission_contended_producers_deliver_each_message_once() {
        let state = std::sync::Arc::new(TypedChannelState::default());
        let (tx, rx) = flume::bounded(1);
        let consumer_state = state.clone();
        let consumer = tokio::spawn(async move {
            let mut seen = std::collections::BTreeSet::new();
            while let Ok(value) = rx.recv_async().await {
                consumer_state.notify_capacity();
                assert!(seen.insert(value), "duplicate admission");
            }
            seen
        });
        let mut producers = Vec::new();
        for producer in 0..4 {
            let tx = tx.clone();
            let state = state.clone();
            producers.push(tokio::spawn(async move {
                for sequence in 0..200 {
                    state.enqueue(&tx, (producer, sequence)).await.unwrap();
                }
            }));
        }
        drop(tx);
        let seen = tokio::time::timeout(Duration::from_secs(10), async move {
            for producer in producers {
                producer.await.unwrap();
            }
            consumer.await.unwrap()
        })
        .await
        .expect("capacity notification lost under contention");
        let expected: std::collections::BTreeSet<_> = (0..4)
            .flat_map(|producer| (0..200).map(move |sequence| (producer, sequence)))
            .collect();
        assert_eq!(seen, expected);
    }

    // ---- 测试用枚举信封 actor（手写 dispatch，覆盖 inject/extract 对偶）----

    #[derive(Debug)]
    pub enum BenchMsg {
        Echo(u64),
        Add(u64, u64),
        Fail(String),
        Slow(u64),
    }

    #[derive(Debug)]
    #[allow(dead_code)] // Fail 变体保留：枚举信封宏的全形状示例
    pub enum BenchReply {
        Echo(u64),
        Add(u64),
        Fail(String),
        Slow(u64),
    }

    pub struct BenchActor {
        pub processed: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl ParrotTypedDispatch for BenchActor {
        type Msg = BenchMsg;
        type Reply = BenchReply;

        fn dispatch<'a>(&'a mut self, msg: Self::Msg) -> BoxedFuture<'a, ActorResult<Self::Reply>> {
            use std::sync::atomic::Ordering;
            Box::pin(async move {
                self.processed.fetch_add(1, Ordering::Relaxed);
                match msg {
                    BenchMsg::Echo(v) => Ok(BenchReply::Echo(v)),
                    BenchMsg::Add(a, b) => Ok(BenchReply::Add(a + b)),
                    BenchMsg::Fail(why) => Err(ActorError::MessageHandlingError(why)),
                    BenchMsg::Slow(ms) => {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Ok(BenchReply::Slow(ms))
                    }
                }
            })
        }
    }

    impl Message for BenchMsg {
        type Result = BenchReply;
        fn extract_result(r: BoxedMessage) -> ActorResult<BenchReply> {
            r.downcast::<BenchReply>()
                .map(|b| *b)
                .map_err(|_| ActorError::MessageHandlingError("type".into()))
        }
    }

    impl ParrotMsgVariant<BenchActor> for BenchMsg {
        fn inject(msg: Self) -> BenchMsg {
            msg
        }
        fn extract(reply: BenchReply) -> ActorResult<BenchReply> {
            Ok(reply)
        }
    }

    #[tokio::test]
    async fn ready_typed_reply_survives_simultaneous_channel_closure() {
        for failure in [false, true] {
            let (sender, receiver) = typed_channel::<BenchActor>(1);
            let lifecycle = std::sync::Arc::new(EventLifecycle {
                watch: crate::death_watch::DeathWatch::new(2),
                channel: TypedChannelState::default(),
            });
            let actor = TypedActorRef::<BenchActor, BenchMsg>::new(sender, "/ready".into())
                .with_lifecycle(lifecycle.clone(), DEFAULT_ASK_TIMEOUT);
            let ask = actor.ask(BenchMsg::Echo(7));
            tokio::pin!(ask);
            assert!(futures::poll!(&mut ask).is_pending());
            let envelope = receiver.try_recv().unwrap();
            let value = if failure {
                Err(ActorError::MessageHandlingError("original reply".into()))
            } else {
                Ok(BenchReply::Echo(7))
            };
            envelope.reply.unwrap().try_send(value).unwrap();
            lifecycle.channel.seal();
            let result = ask.await;
            if failure {
                assert!(
                    matches!(result, Err(ActorError::MessageHandlingError(s)) if s == "original reply")
                );
            } else {
                assert!(matches!(result, Ok(BenchReply::Echo(7))));
            }
        }
    }

    // ---- 测试辅助：建 actor + 消费循环 ----

    async fn spawn_bench(
        capacity: usize,
    ) -> (
        TypedActorRef<BenchActor, BenchMsg>,
        std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        let (tx, rx) = typed_channel::<BenchActor>(capacity);
        let processed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let actor = BenchActor {
            processed: processed.clone(),
        };
        tokio::spawn(async move {
            let mut actor = actor;
            while let Ok(env) = rx.recv_async().await {
                consume_one(&mut actor, env).await;
            }
        });
        (TypedActorRef::new(tx, "/test/bench".into()), processed)
    }

    // ---- 用例 ----

    #[tokio::test]
    async fn channel_capacity_zero_clamps_to_one() {
        // typed_channel(0) 应钳位为 1（max(1) 分支）
        let (tx, rx) = typed_channel::<BenchActor>(0);
        tx.send_async(TypedEnvelope {
            msg: BenchMsg::Echo(1),
            reply: None,
            trace_context: None,
        })
        .await
        .unwrap();
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn ask_echo_roundtrip() {
        let (r, _) = spawn_bench(4).await;
        let v = r.ask(BenchMsg::Echo(42)).await.unwrap();
        assert!(matches!(v, BenchReply::Echo(42)));
    }

    #[tokio::test]
    async fn ask_error_propagates_through_reply_channel() {
        let (r, _) = spawn_bench(4).await;
        let e = r.ask(BenchMsg::Fail("boom".into())).await.unwrap_err();
        assert!(e.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn ask_timeout_returns_timeout_detail() {
        let (r, _) = spawn_bench(4).await;
        let e = r
            .ask_with_timeout(BenchMsg::Slow(300), Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(matches!(e, ActorError::TimeoutDetail(_)));
    }

    #[tokio::test]
    async fn ask_timeout_cancels_wait_for_mailbox_capacity() {
        let (tx, rx) = typed_channel::<BenchActor>(1);
        let actor: TypedActorRef<BenchActor, BenchMsg> = TypedActorRef::new(tx, "/full".into());
        actor.tell(BenchMsg::Echo(1)).await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            actor.ask_with_timeout(BenchMsg::Echo(2), Duration::from_millis(20)),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(ActorError::TimeoutDetail(_)))),
            "{result:?}"
        );
        assert!(matches!(rx.try_recv().unwrap().msg, BenchMsg::Echo(1)));
        assert!(
            rx.try_recv().is_err(),
            "cancelled enqueue must not publish later"
        );
    }

    #[tokio::test]
    async fn ask_unbounded_waits_for_slow_handler() {
        let (r, _) = spawn_bench(4).await;
        let t0 = std::time::Instant::now();
        let v = r.ask_unbounded(BenchMsg::Slow(80)).await.unwrap();
        assert!(matches!(v, BenchReply::Slow(80)));
        assert!(t0.elapsed() >= Duration::from_millis(70));
    }

    #[tokio::test]
    async fn tell_fire_and_forget_delivers() {
        let (r, processed) = spawn_bench(8).await;
        r.tell(BenchMsg::Echo(1)).await.unwrap();
        // 消费循环异步处理：轮询等待
        let dl = std::time::Instant::now() + Duration::from_secs(5);
        while processed.load(std::sync::atomic::Ordering::Relaxed) < 1 {
            if std::time::Instant::now() > dl {
                panic!("tell message never processed");
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn ask_after_channel_close_returns_stopped() {
        // drop 接收端（消费任务退出）后 ask 必须报 Stopped
        let (tx, rx) = typed_channel::<BenchActor>(2);
        let r: TypedActorRef<BenchActor, BenchMsg> = TypedActorRef::new(tx, "/test/closed".into());
        drop(rx);
        let e = r.ask(BenchMsg::Echo(1)).await.unwrap_err();
        assert!(matches!(e, ActorError::Stopped));
        assert!(!r.is_alive());
    }

    #[tokio::test]
    async fn tell_after_channel_close_returns_stopped() {
        let (tx, rx) = typed_channel::<BenchActor>(2);
        let r: TypedActorRef<BenchActor, BenchMsg> = TypedActorRef::new(tx, "/test/closed2".into());
        drop(rx);
        let e = r.tell(BenchMsg::Echo(1)).await.unwrap_err();
        assert!(matches!(e, ActorError::Stopped));
        let erased: &dyn crate::typed::TypedAskRef<BenchMsg> = &r;
        assert!(
            matches!(
                erased.tell(BenchMsg::Echo(2)).await,
                Err(ActorError::Stopped)
            ),
            "trait object must preserve the same admission failure"
        );
    }

    #[tokio::test]
    async fn ref_for_shares_channel_and_path() {
        let (r, _) = spawn_bench(4).await;
        let r2 = r.clone();
        let r3 = r.ref_for::<BenchMsg>();
        assert_eq!(r.path(), "/test/bench");
        assert_eq!(r2.path(), r3.path());
        // 三视图共用同一通道：交替 ask 均可达
        assert!(matches!(
            r.ask(BenchMsg::Echo(1)).await.unwrap(),
            BenchReply::Echo(1)
        ));
        assert!(matches!(
            r2.ask(BenchMsg::Echo(2)).await.unwrap(),
            BenchReply::Echo(2)
        ));
        assert!(matches!(
            r3.ask(BenchMsg::Echo(3)).await.unwrap(),
            BenchReply::Echo(3)
        ));
    }

    #[tokio::test]
    async fn into_dyn_bridge_send_roundtrip() {
        let (r, _) = spawn_bench(4).await;
        let dynref = r.into_dyn();
        let resp = dynref
            .send(Box::new(BenchMsg::Add(1, 2)) as BoxedMessage)
            .await
            .unwrap();
        let v = resp.downcast::<BenchReply>().unwrap();
        assert!(matches!(*v, BenchReply::Add(3)));
        assert_eq!(dynref.path(), "/test/bench");
    }

    #[tokio::test]
    async fn into_dyn_bridge_type_mismatch_error() {
        let (r, _) = spawn_bench(4).await;
        let dynref = r.into_dyn();
        let e = dynref
            .send(Box::new(123u64) as BoxedMessage)
            .await
            .unwrap_err();
        assert!(matches!(e, ActorError::MessageHandlingError(_)));
    }

    #[tokio::test]
    async fn into_dyn_bridge_send_with_timeout_and_deliver() {
        let (r, processed) = spawn_bench(8).await;
        let dynref = r.into_dyn();
        // send_with_timeout(Some)
        let resp = dynref
            .send_with_timeout(
                Box::new(BenchMsg::Echo(9)) as BoxedMessage,
                Some(Duration::from_secs(2)),
            )
            .await
            .unwrap();
        assert!(resp.downcast::<BenchReply>().is_ok());
        // send_with_timeout 超时路径
        let e = dynref
            .send_with_timeout(
                Box::new(BenchMsg::Slow(200)) as BoxedMessage,
                Some(Duration::from_millis(20)),
            )
            .await
            .unwrap_err();
        assert!(matches!(e, ActorError::TimeoutDetail(_)));
        // deliver（tell 语义）
        dynref
            .deliver(Box::new(BenchMsg::Echo(10)) as BoxedMessage)
            .await
            .unwrap();
        let dl = std::time::Instant::now() + Duration::from_secs(5);
        while processed.load(std::sync::atomic::Ordering::Relaxed) < 3 {
            if std::time::Instant::now() > dl {
                panic!("deliver not processed");
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn unmanaged_dynamic_bridge_stop_reports_missing_lifecycle() {
        let (r, _) = spawn_bench(4).await;
        let dynref = r.into_dyn();
        assert!(dynref.stop().await.is_err());
        assert!(dynref.is_alive().await);
        // clone_boxed 语义
        let c = dynref.clone_boxed();
        assert!(c
            .send(Box::new(BenchMsg::Echo(5)) as BoxedMessage)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn typed_ask_ref_trait_object_roundtrip() {
        let (r, _) = spawn_bench(4).await;
        let tr: &dyn crate::typed::TypedAskRef<BenchMsg> = &r;
        let v = tr.ask(BenchMsg::Echo(7)).await.unwrap();
        assert!(matches!(v, BenchReply::Echo(7)));
        tr.tell(BenchMsg::Echo(8)).await.unwrap();
    }

    #[tokio::test]
    async fn bounded_channel_backpressure_blocks_until_consumed() {
        // 容量 1：第二条 tell 阻塞直到消费循环取走第一条
        let (tx, rx) = typed_channel::<BenchActor>(1);
        let r: TypedActorRef<BenchActor, BenchMsg> = TypedActorRef::new(tx, "/test/bp".into());
        r.tell(BenchMsg::Echo(1)).await.unwrap();
        // 无消费者时第二条必然阻塞 → 用 select 验证未完成
        let second = r.tell(BenchMsg::Echo(2));
        tokio::pin!(second);
        let blocked = tokio::select! {
            _ = &mut second => false,
            _ = tokio::time::sleep(Duration::from_millis(80)) => true,
        };
        assert!(blocked, "second tell must block on full channel");
        // 打开消费后解除（flume 唤醒 waiting sender，第二条自动入队）
        let mut consumed = 0;
        let mut actor = BenchActor {
            processed: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        while let Ok(env) = rx.try_recv() {
            consume_one(&mut actor, env).await;
            consumed += 1;
        }
        // flume bounded(1)：try_recv 取走第一条后，被挂起的第二条 send 由
        // flume 内部唤醒自动入队（不依赖我们再 poll second）——drain 可能
        // 看到 1 或 2 条（唤醒是异步的）。
        assert!((1..=2).contains(&consumed), "consumed={consumed}");
        // 确保 second 最终完成
        let _ = (&mut second).await;
        while let Ok(env) = rx.try_recv() {
            consume_one(&mut actor, env).await;
            consumed += 1;
        }
        assert_eq!(consumed, 2);
    }
}
