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
//! 系统持有注册与清理；用户 poll 在有界阻塞池许可下隔离执行。

pub use parrot_api::typed_channel::{DEFAULT_ASK_TIMEOUT, TypedActorRef, TypedEnvelope};

use parrot_api::message::Message;
use parrot_api::typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive};
use parrot_api::types::{ActorResult, BoxedFuture};
use std::marker::PhantomData;
use std::sync::Arc;

use crate::thread::system::ThreadActorSystem;

use crate::thread::{
    error::SystemError,
    system::{ActorRegistryEntry, StopCompletion, StopOutcome, await_stop},
};
use futures::FutureExt;
use parrot_api::{
    address::ActorRef,
    errors::ActorError,
    supervisor::DeathReason,
    typed_channel::{TypedActorLifecycle, TypedChannelState},
};
use std::sync::Mutex;
use std::time::Duration;

pub(crate) struct TypedRegistration {
    pub actor_ref: Arc<dyn ActorRef>,
    pub control: Arc<TypedControl>,
}
impl Drop for TypedRegistration {
    fn drop(&mut self) {
        self.control
            .request_stop(tokio::time::Instant::now() + self.control.timeout);
    }
}

pub(crate) struct TypedControl {
    channel: TypedChannelState,
    deadline: Mutex<Option<tokio::time::Instant>>,
    deadline_changed: tokio::sync::Notify,
    completion: StopCompletion,
    timeout: Duration,
}
impl TypedControl {
    pub(crate) fn request_stop(&self, deadline: tokio::time::Instant) {
        {
            let mut current = self.deadline.lock().unwrap();
            *current = Some(current.map_or(deadline, |old| old.min(deadline)));
        }
        self.channel.seal();
        self.deadline_changed.notify_waiters();
    }
    pub(crate) fn completion(&self) -> StopCompletion {
        self.completion.clone()
    }
    fn deadline(&self) -> tokio::time::Instant {
        self.deadline
            .lock()
            .unwrap()
            .unwrap_or_else(|| tokio::time::Instant::now() + self.timeout)
    }
    async fn expired(&self) {
        loop {
            let changed = self.deadline_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let deadline = *self.deadline.lock().unwrap();
            if let Some(deadline) = deadline {
                tokio::select! { _ = tokio::time::sleep_until(deadline) => return, _ = changed => {} }
            } else {
                changed.await;
            }
        }
    }
}
impl TypedActorLifecycle for TypedControl {
    fn channel(&self) -> &TypedChannelState {
        &self.channel
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.request_stop(tokio::time::Instant::now() + self.timeout);
            await_stop(self.completion())
                .await
                .map_err(ActorError::from)
        })
    }
}

/// All user polls and destructors in this future execute in isolation. A stop
/// cancels the current dispatch before invoking before_stop, under one deadline.
async fn run_typed_actor<A: ParrotTypedDispatch>(
    mut actor: A,
    receiver: flume::Receiver<TypedEnvelope<A>>,
    control: Arc<TypedControl>,
    batch_size: usize,
) -> anyhow::Result<(Result<(), SystemError>, DeathReason)> {
    let mut execution = std::panic::AssertUnwindSafe(async {
        if control.channel.is_sealed() { return Ok(()); }
        tokio::select! {
            biased;
            _ = control.channel.closed() => return Ok(()),
            initialized = actor.init() => initialized.map_err(|error| SystemError::ActorCreationError(error.to_string()))?,
        }
        let mut reductions = 0;
        loop {
            let message = tokio::select! {
                biased;
                _ = control.channel.closed() => break,
                message = receiver.recv_async() => match message { Ok(message) => message, Err(_) => break },
            };
            control.channel.notify_capacity();
            tokio::select! {
                biased;
                _ = control.channel.closed() => break,
                _ = parrot_api::typed_channel::consume_one(&mut actor, message) => {},
            }
            reductions += 1;
            if reductions >= batch_size.max(1) {
                reductions = 0;
                tokio::task::yield_now().await;
            }
        }
        if control.deadline() <= tokio::time::Instant::now() {
            return Err(SystemError::Timeout("typed stop deadline elapsed".into()));
        }
        tokio::select! {
            biased;
            _ = control.expired() => Err(SystemError::Timeout("typed stop hook exceeded configured deadline".into())),
            result = actor.before_stop() => result.map_err(|error| SystemError::ShutdownError(error.to_string())),
        }
    }).catch_unwind().await;
    control.request_stop(tokio::time::Instant::now() + control.timeout);
    // Flume Receiver::drop alone does not release queued payloads while refs
    // retain Sender clones. Explicit draining also resolves queued reply senders.
    while let Ok(message) = receiver.try_recv() {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(message)))
            && matches!(execution, Ok(Ok(())))
        {
            execution = Err(payload);
        }
    }
    drop(receiver);
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor)))
        && matches!(execution, Ok(Ok(())))
    {
        execution = Err(payload);
    }
    let (result, reason) = match execution {
        Ok(result) => {
            let reason = if result.is_ok() {
                DeathReason::Normal
            } else {
                DeathReason::Killed
            };
            (result, reason)
        }
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "typed actor panicked with non-string payload".into());
            (
                Err(SystemError::ActorCreationError(message.clone())),
                DeathReason::Panic(message),
            )
        }
    };
    Ok((result, reason))
}

impl ThreadActorSystem {
    /// M4: 静态轨 spawn——类型化通道直达 actor，零 Any 全链路。
    ///
    /// 系统持有 actor 生命周期；显式 stop 或 shutdown 关闭通道并等待清理。
    /// 返回入口协议 M 的
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
        let (sender, receiver) =
            parrot_api::typed_channel::typed_channel::<A>(self.config().default_mailbox_capacity);
        let (finished, completion) = tokio::sync::watch::channel(None);
        let control = Arc::new(TypedControl {
            channel: TypedChannelState::default(),
            deadline: Mutex::new(None),
            deadline_changed: tokio::sync::Notify::new(),
            completion,
            timeout: self.config().shutdown_timeout,
        });
        let actor_ref = TypedActorRef::new(sender, path.to_owned())
            .with_lifecycle(control.clone(), self.config().default_ask_timeout);
        {
            let mut registry = self.registry.write().unwrap();
            if self.is_shutting_down() {
                return Err(ActorError::Stopped);
            }
            if registry.contains_key(path) {
                return Err(ActorError::InternalError(format!(
                    "actor path already exists: {path}"
                )));
            }
            registry.insert(
                path.to_owned(),
                ActorRegistryEntry::Typed(TypedRegistration {
                    actor_ref: Arc::from(actor_ref.clone().into_dyn()),
                    control: control.clone(),
                }),
            );
            let system = Arc::downgrade(self);
            let path = path.to_owned();
            let slots = self.typed_slots.clone();
            let batch_size = match self.config().default_scheduling_mode {
                crate::thread::config::SchedulingMode::SharedPool {
                    max_messages_per_run,
                } => max_messages_per_run,
                _ => 10,
            };
            self.runtime_handle.spawn(async move {
                let task_control = control.clone();
                let mut execution = tokio::spawn(async move {
                    let permit = slots.clone().acquire_owned().await?;
                    crate::thread::scheduler::shared::poll_isolated(
                        run_typed_actor(actor, receiver, task_control, batch_size), permit, slots, Arc::new(tokio::sync::Notify::new())
                    ).await
                });
                let (result, reason) = tokio::select! {
                    biased;
                    _ = control.expired() => (Err(SystemError::Timeout(format!("typed actor {path} stop exceeded configured deadline"))), DeathReason::Killed),
                    result = &mut execution => match result {
                        Ok(Ok(outcome)) => outcome,
                        error => {
                            let message = format!("typed actor {path} execution failed: {error:?}");
                            (Err(SystemError::ActorCreationError(message.clone())), DeathReason::Panic(message))
                        },
                    },
                };
                // Dropping the JoinHandle after timeout leaves cleanup owned.
                // A synchronous poll cannot be safely preempted; when it returns,
                // the sealed state and deadline prevent any further dispatch.
                control.request_stop(control.deadline());
                let mut result = result;
                if let Some(system) = system.upgrade() {
                    let notified = system.notify_termination_until(&path, reason, control.deadline()).await;
                    result = result.and(notified);
                    let mut registry = system.registry.write().unwrap();
                    if matches!(registry.get(&path), Some(ActorRegistryEntry::Typed(entry)) if Arc::ptr_eq(&entry.control, &control)) {
                        registry.remove(&path);
                    }
                }
                finished.send_replace(Some(StopOutcome::from_result(result)));
            });
        }
        Ok(actor_ref)
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
