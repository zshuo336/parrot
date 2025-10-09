//! Managed typed actors run on their owning Actix arbiter. Lifecycle admission,
//! cancellation and completion are shared with all protocol views and dynamic bridges.
//! Performance depends on the workload and must be measured with release benchmarks.
use crate::actix::{
    lifecycle::{ActorControl, panic_message},
    system::{ActixActorSystem, Registration},
};
use futures::FutureExt;
pub use parrot_api::typed_channel::{DEFAULT_ASK_TIMEOUT, TypedActorRef, TypedEnvelope};
use parrot_api::{
    errors::ActorError,
    typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive},
    types::ActorResult,
};
use std::sync::Arc;

async fn run_actor<A: ParrotTypedDispatch>(
    mut actor: A,
    receiver: flume::Receiver<TypedEnvelope<A>>,
    control: Arc<ActorControl>,
) -> ActorResult<()> {
    let result = std::panic::AssertUnwindSafe(async {
        if control.channel.is_sealed() { return Ok(()); }
        tokio::select! {
            biased;
            _ = control.channel.closed() => return Ok(()),
            result = actor.init() => result?,
        }
        let mut reductions = 0;
        loop {
            let envelope = tokio::select! {
                biased;
                _ = control.channel.closed() => break,
                envelope = receiver.recv_async() => match envelope { Ok(env) => env, Err(_) => break },
            };
            control.channel.notify_capacity();
            tokio::select! {
                biased;
                _ = control.channel.closed() => break,
                _ = parrot_api::typed_channel::consume_one(&mut actor, envelope) => {},
            }
            reductions += 1;
            if reductions >= 10 {
                reductions = 0;
                tokio::task::yield_now().await;
            }
        }
        if control.expired_now() { return Err(ActorError::Timeout); }
        tokio::select! {
            biased;
            _ = control.expired() => Err(ActorError::Timeout),
            result = actor.before_stop() => result,
        }
    }).catch_unwind().await;
    control.request_stop(tokio::time::Instant::now() + control.timeout);
    let mut result =
        result.unwrap_or_else(|payload| Err(ActorError::Panic(panic_message(payload))));
    // One failing destructor must not strand the rest of the bounded queue.
    while let Ok(envelope) = receiver.try_recv() {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(envelope)))
            && result.is_ok()
        {
            result = Err(ActorError::Panic(panic_message(payload)));
        }
    }
    drop(receiver);
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor)))
        && result.is_ok()
    {
        result = Err(ActorError::Panic(panic_message(payload)));
    }
    result
}

impl ActixActorSystem {
    /// Register one actor at the exact requested path. The system owns its lifetime.
    /// Stop cancels pending dispatch, runs the stop hook and joins disposal on the
    /// actor's arbiter. A blocking user poll can exceed the deadline; its cleanup
    /// remains owned and no further messages are admitted.
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
            parrot_api::typed_channel::typed_channel::<A>(self.settings.mailbox_capacity);
        let (control, finished) = ActorControl::with_task_limit(
            self.settings.shutdown_timeout,
            self.settings.max_context_tasks,
        );
        let actor_ref = TypedActorRef::new(sender, path.to_owned())
            .with_lifecycle(control.clone(), self.settings.ask_timeout);
        let mut registry = self.registry.write().unwrap();
        if registry.shutting_down {
            return Err(ActorError::Stopped);
        }
        if registry.actors.contains_key(path) {
            return Err(ActorError::InternalError(format!(
                "actor path already exists: {path}"
            )));
        }
        let arbiter = self.arbiter_pool().next_arbiter();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let task_control = control.clone();
        // The owner catches both poll panics and panics during normal disposal.
        if !arbiter.spawn(async move {
            let result = std::panic::AssertUnwindSafe(run_actor(actor, receiver, task_control))
                .catch_unwind()
                .await
                .unwrap_or_else(|payload| Err(ActorError::Panic(panic_message(payload))));
            let _ = result_tx.send(result);
        }) {
            return Err(ActorError::InternalError(
                "Actix arbiter rejected actor execution".into(),
            ));
        }
        registry.actors.insert(
            path.to_owned(),
            Registration {
                actor_ref: Arc::from(actor_ref.clone().into_dyn()),
                control: control.clone(),
            },
        );
        self.monitor_actor(path.to_owned(), control, finished, result_rx);
        Ok(actor_ref)
    }

    /// M4: 静态轨 spawn（单一协议便捷入口，无需 derive 枚举信封）。
    ///
    /// 与 thread 引擎的 `spawn_typed_single` 完全同构：适配器
    /// `SingleDispatch` 把单个 `TypedReceive<M>` 包装成
    /// `ParrotTypedDispatch`；用户消息经 `SingleVariant::wrap` 包装。
    pub async fn spawn_typed_single<A, M>(
        self: &Arc<Self>,
        actor: A,
        path: &str,
    ) -> ActorResult<
        TypedActorRef<
            crate::thread::typed::SingleDispatch<A, M>,
            crate::thread::typed::SingleVariant<A, M>,
        >,
    >
    where
        A: TypedReceive<M>,
        M: parrot_api::message::Message,
    {
        use crate::thread::typed::{SingleDispatch, SingleVariant};
        self.spawn_typed::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            SingleDispatch::new(actor),
            path,
        )
        .await
    }
}
