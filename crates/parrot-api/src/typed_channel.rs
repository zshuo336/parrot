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

/// Admission and wake state shared by all protocol views of a managed actor.
/// Engines call `notify_capacity` after dequeuing, and `seal` before disposal.
#[derive(Default)]
pub struct TypedChannelState {
    sealed: std::sync::Mutex<bool>,
    available: tokio::sync::Notify,
    closed: tokio::sync::Notify,
}
impl TypedChannelState {
    pub fn is_sealed(&self) -> bool {
        *self.sealed.lock().unwrap()
    }
    pub fn seal(&self) {
        *self.sealed.lock().unwrap() = true;
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
    async fn enqueue<T: Send>(&self, tx: &flume::Sender<T>, mut message: T) -> ActorResult<()> {
        loop {
            let available = self.available.notified();
            tokio::pin!(available);
            available.as_mut().enable();
            {
                let sealed = self.sealed.lock().unwrap();
                if *sealed {
                    return Err(ActorError::Stopped);
                }
                match tx.try_send(message) {
                    Ok(()) => return Ok(()),
                    Err(flume::TrySendError::Disconnected(returned)) => {
                        drop(sealed);
                        drop(returned);
                        return Err(ActorError::Stopped);
                    }
                    Err(flume::TrySendError::Full(returned)) => message = returned,
                }
            }
            available.await;
        }
    }
}

/// Engine-owned lifecycle of a managed typed channel. The callback must join
/// owned cleanup; dropping a stop caller must not cancel the operation.
pub trait TypedActorLifecycle: Send + Sync {
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
    tx: flume::Sender<TypedEnvelope<A>>,
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
            tx,
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
            None => self
                .tx
                .send_async(envelope)
                .await
                .map_err(|_| ActorError::Stopped),
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

impl<A, M> TypedActorRef<A, M>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A> + Message,
{
    /// 静态 ask（托管引用采用系统配置；原始通道默认 5s；无界用 [`TypedActorRef::ask_unbounded`]）。
    pub async fn ask(&self, msg: M) -> ActorResult<M::Result> {
        self.ask_inner(msg, Some(self.default_timeout)).await
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
        // A single deadline covers capacity backpressure and the reply.
        // Dropping this future also cancels a send that has not entered the queue.
        let exchange = async {
            self.enqueue(env).await?;
            let response = async {
                rrx.recv_async()
                    .await
                    .map_err(|e| ActorError::ReplyChannelError(format!("typed reply: {}", e)))?
            };
            if let Some(lifecycle) = &self.lifecycle {
                tokio::select! {
                    biased;
                    _ = lifecycle.channel().closed() => Err(ActorError::Stopped),
                    reply = response => reply,
                }
            } else {
                response.await
            }
        };
        let reply = match timeout {
            Some(d) => tokio::time::timeout(d, exchange).await.map_err(|_| {
                ActorError::TimeoutDetail(format!("typed ask {} timed out", self.path))
            })??,
            None => exchange.await?,
        };
        M::extract(reply)
    }

    /// 静态 tell：fire-and-forget（入队失败返回 Err，与动态轨 deliver 对齐）。
    pub async fn tell(&self, msg: M) -> ActorResult<()> {
        let env = TypedEnvelope {
            msg: M::inject(msg),
            reply: None,
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

    #[tokio::test]
    async fn managed_admission_releases_payload_outside_gate_on_rejection() {
        struct Reentrant(std::sync::Arc<TypedChannelState>);
        impl Drop for Reentrant {
            fn drop(&mut self) {
                assert!(
                    self.0.sealed.try_lock().is_ok(),
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
        tr.tell(BenchMsg::Echo(8)).await; // 静默
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
