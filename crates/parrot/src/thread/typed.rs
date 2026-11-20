//! # 静态类型轨（M4）—— thread 引擎侧接线
//!
//! 双轨编程模型的静态轨：`spawn_typed` 用类型化通道
//! （元素 = actor 的枚举信封 `A::Msg`）直达 actor，全程零
//! `Box<dyn Any>`、零 downcast、零 `AskEnvelope` 装箱。动态轨
//! （`ActorRefExt::ask/tell`）完全不动；`into_dyn` 在显式边界装箱
//! 桥接两轨。
//!
//! M4 引擎中立化：通道机制、`TypedActorRef`、桥接与错误语义全部在
//! 规范面 `parrot_api::typed_channel`（与 actix 引擎共享通道准入和回复实现，
//! 生命周期仍由各自引擎负责，不能由共用通道推断完全等价）；本文件负责 thread 引擎接线：
//! 系统持有注册与清理；用户 poll 在有界阻塞池许可下隔离执行。

#[cfg(test)]
mod admission_contracts;
#[cfg(test)]
mod admission_lifecycle_contracts;
mod factory;

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

/// Original static child inputs returned when shared instance capacity is full
/// or closed. Their ownership and disposal remain with the caller.
pub struct TypedInstanceAdmissionInputs<A: ParrotTypedDispatch> {
    pub actor: A,
    pub strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
}

/// Recoverable static child rejection carried by [`ActorError::Other`].
/// Downcast the error and call [`Self::into_inputs`] to retry in a live context.
/// Formatting this error does not inspect user inputs; actors need only be Send.
pub struct TypedInstanceAdmissionError<A: ParrotTypedDispatch> {
    pub reason: super::instance::InstanceAdmissionFailure,
    inputs: Mutex<TypedInstanceAdmissionInputs<A>>,
}
impl<A: ParrotTypedDispatch> TypedInstanceAdmissionError<A> {
    pub fn into_inputs(self) -> TypedInstanceAdmissionInputs<A> {
        self.inputs
            .into_inner()
            .unwrap_or_else(|error| error.into_inner())
    }
}
impl<A: ParrotTypedDispatch> std::fmt::Display for TypedInstanceAdmissionError<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.reason.fmt(f)
    }
}
impl<A: ParrotTypedDispatch> std::fmt::Debug for TypedInstanceAdmissionError<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedInstanceAdmissionError")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl<A: ParrotTypedDispatch> std::error::Error for TypedInstanceAdmissionError<A> {}

async fn retire_source<A: ParrotTypedDispatch>(
    actor: Option<crate::owned_payload::OwnedPayload<A>>,
    factory: Option<factory::OwnedFactory<A>>,
    policy: Option<
        crate::owned_payload::OwnedPayload<Option<parrot_api::supervisor::SupervisorStrategyType>>,
    >,
) -> ActorResult<()> {
    // Separate unwind boundaries preserve every receipt after double failure.
    let (actor, factory, policy) = tokio::join!(
        async {
            match actor {
                Some(actor) => actor.retire().await,
                None => Ok(()),
            }
        },
        async {
            match factory {
                Some(factory) => factory.retire().await,
                None => Ok(()),
            }
        },
        async {
            match policy {
                Some(policy) => policy.retire().await,
                None => Ok(()),
            }
        },
    );
    actor.and(factory).and(policy)
}

pub(crate) struct TypedRegistration {
    pub actor_ref: Arc<dyn ActorRef>,
    pub control: Arc<TypedControl>,
}
impl Drop for TypedRegistration {
    fn drop(&mut self) {
        self.control
            .begin_cleanup(tokio::time::Instant::now() + self.control.timeout);
    }
}

pub(crate) struct TypedControl {
    tree: crate::typed_context::TypedTree,
    escalated: Mutex<Option<crate::error_receipt::ActorReceipt>>,
    escalation_changed: tokio::sync::Notify,
    explicit_stop: std::sync::atomic::AtomicBool,
    stop_context: crate::stop_context::StopContext,
    channel: TypedChannelState,
    lifetime: Arc<crate::children::ChildLifetime>,
    deadline: Mutex<Option<tokio::time::Instant>>,
    deadline_changed: tokio::sync::Notify,
    completion: StopCompletion,
    retirement: StopCompletion,
    timeout: Duration,
    creation_timeout: Duration,
    message_timeout: Duration,
    creation_limit: Duration,
    initialization_deadline: Mutex<Option<tokio::time::Instant>>,
    instance_reservation: Mutex<Option<Arc<tokio::sync::OwnedSemaphorePermit>>>,
    policy_slots: Arc<tokio::sync::Semaphore>,
    death_watch: parrot_api::death_watch::DeathWatch,
    pub(crate) legacy_watch: parrot_api::death_watch::DeathWatch,
}
impl TypedControl {
    fn initialization_deadline(&self) -> Option<tokio::time::Instant> {
        *self.initialization_deadline.lock().unwrap()
    }
    fn finish_initialization(&self) {
        self.initialization_deadline.lock().unwrap().take();
        self.release_instance_admission();
    }
    fn release_instance_admission(&self) {
        let reservation = self.instance_reservation.lock().unwrap().take();
        drop(reservation);
    }
    #[cfg(test)]
    pub(crate) fn escalation_hook(&self) -> crate::children::Escalate {
        self.tree.escalation_hook()
    }
    #[cfg(test)]
    pub(crate) fn check_escalation_locks(&self) {
        drop(self.escalated.lock().unwrap());
        drop(self.deadline.lock().unwrap());
    }
    #[cfg(test)]
    pub(crate) fn pending_escalation_error(&self) -> Option<Arc<ActorError>> {
        self.escalated
            .lock()
            .unwrap()
            .as_ref()
            .map(|error| Arc::clone(error))
    }
    #[cfg(test)]
    pub(crate) async fn escalation_deadline_waiter(&self) {
        self.expired().await;
    }
    async fn group_recovery(&self) -> Arc<crate::children::recovery::Recovery> {
        loop {
            let changed = self.escalation_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(group) = self.lifetime.recovery() {
                return group;
            }
            changed.await;
        }
    }
    async fn escalation(&self) -> crate::error_receipt::ActorReceipt {
        loop {
            let changed = self.escalation_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(error) = self.escalated.lock().unwrap().clone() {
                return error;
            }
            changed.await;
        }
    }

    pub(super) async fn wait_typed_stage<F: std::future::Future>(
        &self,
        future: F,
        runtime: &tokio::runtime::Handle,
    ) -> crate::typed_supervision::Step<F::Output> {
        use crate::typed_supervision::Step;
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                _ = self.channel.closed() => return Step::Stopped,
                group = self.group_recovery() => return Step::Recovery(group),
                error = self.escalation() => {
                    let deadline = self.deadline();
                    let resume = tokio::select! {
                        biased;
                        _ = self.channel.closed() => false,
                        _ = self.group_recovery() => false,
                        resume = self.lifetime.resume_escalation(error.clone(), deadline, runtime) => resume,
                    };
                    let resumed = if resume {
                        // Coordinate clearing the signal and its timer with the
                        // producer. A concurrent signal cannot rearm a stale timer.
                        let mut signal = self.escalated.lock().unwrap();
                        let completed = if signal.as_ref().is_some_and(|pending| Arc::ptr_eq(pending, &error)) {
                            self.construction_complete_quiet(deadline)
                        } else { Ok(false) };
                        let removed = if matches!(completed, Ok(true)) { signal.take() } else { None };
                        drop(signal);
                        self.deadline_changed.notify_waiters();
                        if completed.is_err() { self.request_stop(deadline); }
                        removed.is_some()
                    } else { false };
                    if !resumed { return Step::Escalated(error); }
                    // Resume the same pinned future: no replay, lost reply,
                    // partial initialization activation or generation change.
                },
                result = &mut future => return Step::Ready(result),
            }
        }
    }

    pub(crate) fn request_stop(&self, deadline: tokio::time::Instant) {
        self.stop_context.claim_current();
        self.explicit_stop
            .store(true, std::sync::atomic::Ordering::Release);
        self.begin_cleanup(deadline);
    }
    fn begin_cleanup(&self, deadline: tokio::time::Instant) {
        self.tree.close();
        let deadline = self
            .lifetime
            .decision_deadline()
            .map_or(deadline, |decision| deadline.min(decision));
        self.lifetime.cancel_recovery();
        self.death_watch.close();
        self.legacy_watch.close();
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
    pub(crate) fn retirement(&self) -> StopCompletion {
        self.retirement.clone()
    }
    fn deadline(&self) -> tokio::time::Instant {
        self.deadline
            .lock()
            .unwrap()
            .unwrap_or_else(|| tokio::time::Instant::now() + self.timeout)
    }
    async fn join_until_expired<F: std::future::Future>(
        &self,
        work: F,
        publish_timeout: impl FnOnce(),
    ) -> (F::Output, bool) {
        tokio::pin!(work);
        tokio::select! {
            biased;
            result = &mut work => {
                // Recovery may clear or replace this deadline while work is
                // pending. Inspect the current value, not a captured budget.
                let timed_out = self.deadline.lock().unwrap().is_some_and(|deadline| {
                    tokio::time::Instant::now() >= deadline
                });
                if timed_out {
                    publish_timeout();
                }
                (result, timed_out)
            },
            _ = self.expired() => {
                publish_timeout();
                (work.await, true)
            }
        }
    }

    async fn expired(&self) {
        loop {
            let changed = self.deadline_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let deadline = *self.deadline.lock().unwrap();
            if let Some(deadline) = deadline {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {
                        // A successful factory recovery clears its temporary
                        // deadline. A stale timer must not stop the new instance.
                        if self.deadline.lock().unwrap().is_some_and(|current| current <= tokio::time::Instant::now()) { return; }
                    },
                    _ = changed => {}
                }
            } else {
                changed.await;
            }
        }
    }
}
impl TypedActorLifecycle for TypedControl {
    fn retirement_status(&self) -> parrot_api::address::RetirementStatus {
        self.lifetime.retirement_status()
    }
    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        Some(self.death_watch.clone())
    }
    fn channel(&self) -> &TypedChannelState {
        &self.channel
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.request_stop(tokio::time::Instant::now() + self.timeout);
            await_stop(self.completion())
                .await
                .map_err(ActorError::from)
        }))
    }
}

// Keep mailbox and actor ownership outside user futures. Dropping an async
// execution task does not run the code following its last await.
struct TypedInputs<A: ParrotTypedDispatch> {
    actor: Option<A>,
    creation_context: Option<parrot_api::trace::TraceContext>,
    receiver: Option<flume::Receiver<TypedEnvelope<A>>>,
    factory: Option<Arc<crate::constructor::Factory<A>>>,
    cleanup_error: Option<crate::error_receipt::ActorReceipt>,
    dispatch_cancel_failed: bool,
    control: Arc<TypedControl>,
    retired:
        Option<tokio::sync::watch::Sender<Option<Result<(), crate::error_receipt::ActorReceipt>>>>,
}
impl<A: ParrotTypedDispatch> TypedInputs<A> {
    fn retire(&mut self) -> Result<(), crate::error_receipt::ActorReceipt> {
        self.control
            .begin_cleanup(tokio::time::Instant::now() + self.control.timeout);
        let mut result = self.cleanup_error.take().map_or(Ok(()), Err);
        if let Some(receiver) = self.receiver.take() {
            while let Ok(message) = receiver.try_recv() {
                if let Err(panic) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(message)))
                {
                    let message = typed_panic(panic);
                    if result.is_ok() {
                        result = Err(crate::error_receipt::ActorReceipt::from_error(
                            ActorError::Panic(message),
                        ));
                    }
                }
            }
            drop(receiver);
        }
        if let Some(actor) = self.actor.take()
            && let Err(panic) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor)))
        {
            let message = typed_panic(panic);
            if result.is_ok() {
                result = Err(crate::error_receipt::ActorReceipt::from_error(
                    ActorError::Panic(message),
                ));
            }
        }
        if let Some(retired) = self.retired.take() {
            retired.send_replace(Some(result.clone()));
        }
        result
    }
}
impl<A: ParrotTypedDispatch> Drop for TypedInputs<A> {
    fn drop(&mut self) {
        if self.retired.is_some() {
            // Independent unwind boundaries prevent a cancelled user future's
            // panic from combining with actor/message Drop into process abort.
            let _ = self.retire();
        }
    }
}
fn typed_panic(payload: Box<dyn std::any::Any + Send>) -> String {
    crate::owned_payload::panic_message(payload)
}

// Physical destructor panics retain their established system classification;
// other retirement errors retain their actual source and variant.
fn retirement_error(error: crate::error_receipt::ActorReceipt) -> SystemError {
    match error.error().as_ref() {
        ActorError::Panic(message) => SystemError::Panic(message.clone()),
        _ => SystemError::Other(anyhow::Error::new(error.into_error())),
    }
}

// Known framework variants preserve their existing system classification.
// Unknown business errors cross this boundary by ownership, without running
// their Display/source callbacks inside the actor execution or cleanup task.
fn operation_error(error: &Arc<ActorError>, initialization: bool) -> SystemError {
    match error.as_ref() {
        ActorError::Other(_) => SystemError::Other(anyhow::Error::new(
            crate::error_receipt::ActorReceipt::new(error.clone()).into_error(),
        )),
        ActorError::Panic(message) if !initialization => SystemError::Panic(message.clone()),
        error if initialization => SystemError::ActorCreationError(error.to_string()),
        error => SystemError::WorkerStateError(error.to_string()),
    }
}

// Keep known parent-escalation classifications while retaining arbitrary
// business causes without executing their Display/source callbacks.
fn escalation_error(error: &Arc<ActorError>) -> SystemError {
    match error.as_ref() {
        ActorError::Other(_) => SystemError::Other(anyhow::Error::new(
            crate::error_receipt::ActorReceipt::new(error.clone()).into_error(),
        )),
        error => SystemError::WorkerStateError(format!("child escalated: {error}")),
    }
}

#[derive(Debug)]
struct TypedExit {
    result: Result<(), SystemError>,
    reason: DeathReason,
    cleanup_failed: bool,
    failure: Option<crate::error_receipt::ActorReceipt>,
}

/// All user polls and destructors in this future execute in isolation. A stop
/// cancels the current dispatch before invoking before_stop, under one deadline.
async fn run_typed_actor<A: ParrotTypedDispatch>(
    mut inputs: TypedInputs<A>,
    control: Arc<TypedControl>,
    batch_size: usize,
    runtime: tokio::runtime::Handle,
) -> anyhow::Result<TypedExit> {
    if inputs.factory.is_some() {
        return factory::run(inputs, control, batch_size, runtime).await;
    }
    let context = control.tree.context();
    let mut failure = None;
    let mut hook_started = false;
    let mut was_escalated = false;
    let actor = inputs.actor.as_mut().expect("live typed instance");
    let receiver = inputs.receiver.as_ref().expect("live typed mailbox");
    let mut execution = std::panic::AssertUnwindSafe(async {
        if control.channel.is_sealed() { return Ok(()); }
        use crate::typed_supervision::Step;
        match control.wait_typed_stage(parrot_api::trace::scope(inputs.creation_context.take(), crate::typed_supervision::recorded_lifecycle_stage(parrot_api::trace::SpanKind::ActorInit, context.path(), || actor.init_with_context(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed).with_deadline(control.initialization_deadline()))), &runtime).await {
            Step::Stopped => return Ok(()),
            Step::Escalated(error) => { was_escalated=true;failure=Some(error.clone());return Err(escalation_error(&error)); },
            Step::Recovery(group) => { group.abort("non-repeatable parent cannot recover");return Err(SystemError::WorkerStateError("non-repeatable parent cannot recover".into())); },
            Step::Ready(initialized) => initialized.result.map_err(|message| {failure=Some(crate::error_receipt::ActorReceipt::from_error(ActorError::Panic(message.clone())));SystemError::Panic(message)})?.map_err(|error| {let error=crate::error_receipt::ActorReceipt::from_error(error);let result=operation_error(&error,true);failure=Some(error);result})?,
        }
        let mut reductions = 0;
        control.finish_initialization();
        loop {
            let message = match control.wait_typed_stage(receiver.recv_async(), &runtime).await {
                Step::Stopped => break,
                Step::Escalated(error) => {was_escalated=true;failure=Some(error.clone());return Err(escalation_error(&error));},
                Step::Recovery(group) => {group.abort("non-repeatable parent cannot recover");return Err(SystemError::WorkerStateError("non-repeatable parent cannot recover".into()));},
                Step::Ready(Ok(message)) => message,
                Step::Ready(Err(_)) => break,
            };
            control.channel.notify_capacity();
            match control.wait_typed_stage(crate::typed_supervision::dispatch(actor, message, &control.lifetime, &runtime, control.timeout, Some(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed).with_timeout(control.message_timeout)), &runtime).await {
                Step::Stopped => break,
                Step::Escalated(error) => {was_escalated=true;failure=Some(error.clone());return Err(escalation_error(&error));},
                Step::Recovery(group) => {group.abort("non-repeatable parent cannot recover");return Err(SystemError::WorkerStateError("non-repeatable parent cannot recover".into()));},
                Step::Ready(result) => {
                    if let Err(error) = &result {failure=Some(error.clone());}
                    result.map_err(|error| operation_error(&error, false))?;
                },
            }
            reductions += 1;
            if reductions >= batch_size.max(1) {
                reductions = 0;
                tokio::task::yield_now().await;
            }
        }
        // Explicit stop may cancel a handler while its failure decider is
        // still running. Do not enter a normal hook on that failed instance;
        // the system owner still joins the outstanding decision and captures.
        if let Some(error)=control.escalated.lock().unwrap().clone() {
            failure=Some(error.clone());
            return Err(escalation_error(&error));
        }
        if control.lifetime.decision_deadline().is_some() { return Ok(()); }
        control.tree.join().await.map_err(|error| SystemError::Other(anyhow::Error::new(error)))?;
        if control.deadline() <= tokio::time::Instant::now() {
            return Err(SystemError::Timeout("typed stop deadline elapsed".into()));
        }
        if inputs.dispatch_cancel_failed { return Ok(()); }
        hook_started=true;
        tokio::select! {
            biased;
            _ = control.expired() => Err(SystemError::Timeout("typed stop hook exceeded configured deadline".into())),
            result = parrot_api::trace::scope(control.stop_context.take(), crate::typed_supervision::recorded_lifecycle_stage(parrot_api::trace::SpanKind::ActorBeforeStop, context.path(), || actor.before_stop_with_context(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed))) => result.result.map_err(SystemError::Panic).and_then(|result| result.map_err(|error| SystemError::Other(anyhow::Error::new(error)))),
        }
    }).catch_unwind().await.map_err(typed_panic);
    let children = control
        .tree
        .join()
        .await
        .map_err(|error| SystemError::Other(anyhow::Error::new(error)));
    let cleanup = inputs.retire().map_err(retirement_error).and(children);
    let cleanup_failed = cleanup.is_err() || (hook_started && !matches!(&execution, Ok(Ok(()))));
    if let Err(error) = cleanup {
        execution = Ok(Err(error));
    }
    let (result, reason) = match execution {
        Ok(result) => {
            let reason = match &result {
                Ok(()) => DeathReason::Normal,
                // Only engine deadlines produce SystemError::Timeout here;
                // application errors are mapped at the init/hook boundary.
                Err(SystemError::Timeout(_)) => DeathReason::Killed,
                Err(SystemError::Panic(message)) => DeathReason::Panic(message.clone()),
                Err(error) if was_escalated => {
                    DeathReason::Escalated(crate::error_receipt::system_summary(error))
                }
                Err(error) => DeathReason::Failed(crate::error_receipt::system_summary(error)),
            };
            (result, reason)
        }
        Err(message) => {
            if failure.is_none() && !cleanup_failed {
                failure = Some(crate::error_receipt::ActorReceipt::from_error(
                    ActorError::Panic(message.clone()),
                ));
            }
            (
                Err(SystemError::Panic(message.clone())),
                DeathReason::Panic(message),
            )
        }
    };
    Ok(TypedExit {
        result,
        reason,
        cleanup_failed,
        failure,
    })
}

impl ThreadActorSystem {
    /// M4: 静态轨 spawn——类型化通道直达 actor，零 Any 全链路。
    ///
    /// 系统持有 actor 生命周期；显式 stop 或 shutdown 关闭通道并等待清理。
    /// 返回入口协议 M 的
    /// `TypedActorRef<A, M>`；其它协议用 `ref_for::<M2>()` 派生。
    /// Inputs acquire cleanup ownership when this method is called. Dropping
    /// the returned future before polling retires them without invoking actor
    /// construction or initialization; registration still starts on first poll.
    /// At first poll, native instances wait for shared creation capacity before
    /// registering. The creation deadline includes this wait. A returned reference
    /// can still be initializing; capacity returns after successful initialization
    /// or actual retirement, not merely reference publication.
    pub fn spawn_typed<'a, A, M>(
        self: &'a Arc<Self>,
        actor: A,
        path: &'a str,
    ) -> impl std::future::Future<Output = ActorResult<TypedActorRef<A, M>>> + Send + 'a
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        let strategy = if matches!(
            self.config().default_supervisor_strategy,
            crate::thread::config::SupervisorStrategy::Resume
        ) {
            parrot_api::supervisor::DefaultStrategy::ResumeOnFailure
        } else {
            parrot_api::supervisor::DefaultStrategy::StopOnFailure
        };
        self.spawn_typed_with_strategy(
            actor,
            path,
            parrot_api::supervisor::SupervisorStrategyType::Default(strategy),
        )
    }

    /// Spawn a static protocol instance with explicit handler supervision.
    /// Resume retains its state and queue. Other decisions retire the instance;
    /// this instance-only entry point does not supply a reconstruction factory.
    pub fn spawn_typed_with_strategy<'a, A, M>(
        self: &'a Arc<Self>,
        actor: A,
        path: &'a str,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> impl std::future::Future<Output = ActorResult<TypedActorRef<A, M>>> + Send + 'a
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        self.spawn_typed_source(Some(actor), None, path, strategy)
    }

    /// Admit a static instance into a root group. A OneForAll Restart stops
    /// the entire selected group if any member has no repeatable factory.
    pub fn spawn_typed_in_cohort<'a, A, M>(
        self: &'a Arc<Self>,
        actor: A,
        path: &'a str,
        cohort: &'a crate::root_cohort::RootCohort,
    ) -> impl std::future::Future<Output = ActorResult<TypedActorRef<A, M>>> + Send + 'a
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        let payload = crate::owned_payload::OwnedPayload::new(
            actor,
            self.retirement_runtime(),
            &self.cleanup_slots,
        );
        let policy = crate::owned_payload::OwnedPayload::new(
            None,
            self.retirement_runtime(),
            &self.cleanup_slots,
        );
        parrot_api::trace::capture(self.spawn_typed_owned_source(
            Some(payload),
            None,
            path,
            policy,
            None,
            Some(cohort),
        ))
    }

    /// Admit a repeatable static root into the group's shared recovery barriers.
    pub fn spawn_typed_factory_in_cohort<'a, A, M, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
        cohort: &'a crate::root_cohort::RootCohort,
    ) -> impl std::future::Future<Output = ActorResult<TypedActorRef<A, M>>> + Send + 'a
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        let factory = factory::OwnedFactory::new(
            factory,
            self.factory_slots.clone(),
            self.cleanup_slots.clone(),
            self.retirement_runtime().clone(),
        );
        let policy = crate::owned_payload::OwnedPayload::new(
            None,
            self.retirement_runtime(),
            &self.cleanup_slots,
        );
        parrot_api::trace::capture(self.spawn_typed_owned_source(
            None,
            Some(factory),
            path,
            policy,
            None,
            Some(cohort),
        ))
    }

    fn spawn_typed_source<'a, A, M>(
        self: &'a Arc<Self>,
        actor: Option<A>,
        factory: Option<factory::OwnedFactory<A>>,
        path: &'a str,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> impl std::future::Future<Output = ActorResult<TypedActorRef<A, M>>> + Send + 'a
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        let payload = actor.map(|actor| {
            crate::owned_payload::OwnedPayload::new(
                actor,
                self.retirement_runtime(),
                &self.cleanup_slots,
            )
        });
        let policy = crate::owned_payload::OwnedPayload::new(
            Some(strategy),
            self.retirement_runtime(),
            &self.cleanup_slots,
        );
        parrot_api::trace::capture(
            self.spawn_typed_owned_source(payload, factory, path, policy, None, None),
        )
    }

    pub(crate) async fn spawn_typed_child_factory_source<A, M, F>(
        self: &Arc<Self>,
        factory: crate::owned_payload::OwnedPayload<F>,
        policy: crate::owned_payload::OwnedPayload<
            Option<parrot_api::supervisor::SupervisorStrategyType>,
        >,
        path: &str,
        parent: &crate::typed_context::ManagedContext,
    ) -> ActorResult<TypedActorRef<A, M>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        let factory = crate::constructor::owned::OwnedFactory::new(
            factory.take(),
            self.factory_slots.clone(),
            self.cleanup_slots.clone(),
            self.retirement_runtime().clone(),
        );
        self.spawn_typed_owned_source(None, Some(factory), path, policy, Some(parent), None)
            .await
    }

    pub(crate) async fn spawn_typed_child_source<A, M>(
        self: &Arc<Self>,
        actor: crate::owned_payload::OwnedPayload<A>,
        policy: crate::owned_payload::OwnedPayload<
            Option<parrot_api::supervisor::SupervisorStrategyType>,
        >,
        path: &str,
        parent: &crate::typed_context::ManagedContext,
    ) -> ActorResult<TypedActorRef<A, M>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        self.spawn_typed_owned_source(Some(actor), None, path, policy, Some(parent), None)
            .await
    }

    async fn spawn_typed_owned_source<A, M>(
        self: &Arc<Self>,
        payload: Option<crate::owned_payload::OwnedPayload<A>>,
        factory: Option<factory::OwnedFactory<A>>,
        path: &str,
        policy: crate::owned_payload::OwnedPayload<
            Option<parrot_api::supervisor::SupervisorStrategyType>,
        >,
        parent: Option<&crate::typed_context::ManagedContext>,
        cohort: Option<&crate::root_cohort::RootCohort>,
    ) -> ActorResult<TypedActorRef<A, M>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        let creation_context = parrot_api::trace::TraceContext::current();
        let started = tokio::time::Instant::now();
        let initialization_deadline = (!self.operation_timeouts.actor_creation.is_zero())
            .then(|| started + self.operation_timeouts.actor_creation);
        let (payload, reservation) = if let Some(payload) = payload {
            let reservation = if parent.is_some() {
                use super::instance::InstanceAdmissionFailure;
                let admitted = if self.is_shutting_down() {
                    Err(InstanceAdmissionFailure::ShuttingDown)
                } else {
                    self.instance_admission
                        .clone()
                        .try_acquire_owned()
                        .map_err(|error| match error {
                            tokio::sync::TryAcquireError::Closed => {
                                InstanceAdmissionFailure::ShuttingDown
                            }
                            tokio::sync::TryAcquireError::NoPermits => {
                                InstanceAdmissionFailure::Full
                            }
                        })
                };
                match admitted {
                    Ok(permit) => Arc::new(permit),
                    Err(reason) => {
                        return Err(ActorError::Other(anyhow::Error::new(
                            TypedInstanceAdmissionError {
                                reason,
                                inputs: Mutex::new(TypedInstanceAdmissionInputs {
                                    actor: payload.take(),
                                    strategy: policy.take(),
                                }),
                            },
                        )));
                    }
                }
            } else {
                let admitted = if self.is_shutting_down() {
                    Err(ActorError::Stopped)
                } else {
                    tokio::select! {
                        biased;
                        _ = tokio::time::sleep_until(started + self.creation_timeout()) => Err(ActorError::TimeoutDetail("static instance admission exceeded creation deadline".into())),
                        permit = self.instance_admission.clone().acquire_owned() => permit.map_err(|_| ActorError::Stopped),
                    }
                };
                match admitted {
                    Ok(permit) => Arc::new(permit),
                    Err(error) => {
                        retire_source(Some(payload), factory, Some(policy)).await?;
                        return Err(error);
                    }
                }
            };
            (
                Some(payload.with_reservation(&reservation)),
                Some(reservation),
            )
        } else {
            (None, None)
        };
        let policy = match &reservation {
            Some(reservation) => policy.with_reservation(reservation),
            None => policy,
        };
        let cohort = cohort.or_else(|| {
            parent
                .is_none()
                .then(|| self.root_cohorts.guardian())
                .flatten()
        });
        let policy = if cohort.is_some() {
            if let Err(error) = policy.retire().await {
                retire_source(payload, factory, None).await?;
                return Err(error);
            }
            crate::owned_payload::OwnedPayload::new(
                None,
                self.retirement_runtime(),
                &self.cleanup_slots,
            )
        } else {
            policy
        };
        let mut payload = payload;
        let mut factory = factory;
        let mut policy = Some(policy);
        let mut admit = |lifetime: Option<Arc<crate::children::ChildLifetime>>| {
            let (sender, receiver) = parrot_api::typed_channel::typed_channel::<A>(
                self.config().default_mailbox_capacity,
            );
            let (finished, completion) = tokio::sync::watch::channel(None);
            let (retired, retirement) = tokio::sync::watch::channel(None);
            let own = Arc::new(std::sync::OnceLock::new());
            let control = Arc::new_cyclic(|weak: &std::sync::Weak<TypedControl>| {
                let stop = weak.clone();
                let escalated = weak.clone();
                let hooks = crate::typed_context::Hooks {
                    stop: Arc::new(move |scope| {
                        if let Some(control) = stop.upgrade()
                            && control.tree.stop_generation(scope)
                        {
                            control.request_stop(tokio::time::Instant::now() + control.timeout);
                        }
                    }),
                    escalate: Arc::new(move |error| {
                        let error = crate::error_receipt::ActorReceipt::new(error);
                        if let Some(control) = escalated.upgrade()
                            && !control.channel.is_sealed()
                        {
                            let mut signal = control.escalated.lock().unwrap();
                            if signal.is_none() {
                                *signal = Some(error.clone());
                            }
                            control.begin_construction_quiet(
                                tokio::time::Instant::now() + control.timeout,
                            );
                            drop(signal);
                            control.deadline_changed.notify_waiters();
                            control.escalation_changed.notify_waiters();
                        }
                    }),
                };
                TypedControl {
                    tree: crate::typed_context::TypedTree::new(
                        self,
                        crate::typed_context::Identity {
                            path: path.to_owned(),
                            own: own.clone(),
                            parent: parent.and_then(|parent| parent.own()),
                        },
                        hooks,
                    ),
                    escalated: Mutex::new(None),
                    escalation_changed: tokio::sync::Notify::new(),
                    explicit_stop: std::sync::atomic::AtomicBool::new(false),
                    stop_context: Default::default(),
                    channel: TypedChannelState::default(),
                    lifetime: lifetime.unwrap_or_else(|| {
                        crate::children::ChildLifetime::with_policy(
                            std::sync::Weak::new(),
                            None,
                            self.cleanup_slots.clone(),
                        )
                    }),
                    deadline: Mutex::new(None),
                    deadline_changed: tokio::sync::Notify::new(),
                    completion,
                    retirement,
                    timeout: self.config().shutdown_timeout,
                    creation_timeout: self.creation_timeout(),
                    message_timeout: self.operation_timeouts.message_handling,
                    creation_limit: self.operation_timeouts.actor_creation,
                    initialization_deadline: Mutex::new(if payload.is_some() {
                        initialization_deadline
                    } else {
                        (!self.operation_timeouts.actor_creation.is_zero()).then(|| {
                            tokio::time::Instant::now() + self.operation_timeouts.actor_creation
                        })
                    }),
                    instance_reservation: Mutex::new(reservation.clone()),
                    policy_slots: self.cleanup_slots.clone(),
                    death_watch: parrot_api::death_watch::DeathWatch::new(1024),
                    legacy_watch: parrot_api::death_watch::DeathWatch::new(1024),
                }
            });
            if factory.is_some() {
                let weak = Arc::downgrade(&control);
                control.lifetime.bind_recovery(Arc::new(move |group| {
                    if let Some(control) = weak.upgrade() {
                        if control.channel.is_sealed() {
                            group.abort("static factory member is stopping");
                        }
                        control.begin_construction_quiet(group.deadline);
                        control.deadline_changed.notify_waiters();
                        control.escalation_changed.notify_waiters();
                    } else {
                        group.abort("static factory member owner disappeared");
                    }
                }));
            }
            let actor_ref = TypedActorRef::new(sender, path.to_owned())
                .with_lifecycle(control.clone(), self.config().default_ask_timeout);
            actor_ref.install_termination_recipient()?;
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
                let erased: Arc<dyn ActorRef> = Arc::from(actor_ref.clone().into_dyn());
                own.set(Arc::downgrade(&erased))
                    .unwrap_or_else(|_| unreachable!("one logical reference"));
                crate::thread::death_watch::install_typed_legacy(&control.legacy_watch, &erased);
                registry.insert(
                    path.to_owned(),
                    ActorRegistryEntry::Typed(TypedRegistration {
                        actor_ref: erased,
                        control: control.clone(),
                    }),
                );
                let payload = payload.take();
                let factory = factory.take();
                let source_factory = factory.as_ref().map(|owner| owner.factory.clone());
                let policy = policy.take().expect("one accepted typed policy");
                let unpublished_actor = payload.as_ref().map(|value| value.receipt());
                let unpublished_policy = policy.receipt();
                let runtime = self.runtime_handle.clone();
                let system = Arc::downgrade(self);
                let path = path.to_owned();
                let slots = self.typed_slots.clone();
                let batch_size = match self.config().default_scheduling_mode {
                    crate::thread::config::SchedulingMode::SharedPool {
                        max_messages_per_run,
                    } => max_messages_per_run,
                    _ => 10,
                };
                let (input_retired, mut input_retirement) = tokio::sync::watch::channel(None);
                // An unpolled/permit-waiting task owns admitted messages too.
                // Its disposal is offloaded like other unpublished user inputs.
                let inputs = crate::owned_payload::OwnedPayload::new(
                    TypedInputs {
                        actor: None,
                        creation_context: creation_context.clone(),
                        receiver: Some(receiver),
                        factory: source_factory,
                        cleanup_error: None,
                        dispatch_cancel_failed: false,
                        control: control.clone(),
                        retired: Some(input_retired),
                    },
                    self.retirement_runtime(),
                    &self.cleanup_slots,
                );
                let unpublished_inputs = inputs.receipt();
                let execution_runtime = self.runtime_handle.clone();
                let retirement_runtime = self.retirement_runtime().clone();
                self.retirement_runtime().spawn(async move {
                let task_control = control.clone();
                let execution = execution_runtime.spawn(async move {
                    let permit = slots.clone().acquire_owned().await?;
                    crate::thread::scheduler::shared::poll_isolated_with_retirement(
                        async move {
                            let mut inputs = inputs.take();
                            inputs.actor = payload.map(|value| value.take());
                            let strategy = policy.take();
                            task_control.lifetime.set_strategy(strategy);
                            run_typed_actor(inputs, task_control, batch_size, runtime).await
                        }, permit, slots, Arc::new(tokio::sync::Notify::new()), retirement_runtime
                    ).await
                });
                let (execution, mut timed_out) = control.join_until_expired(execution, || {
                    control.request_stop(control.deadline());
                    finished.send_replace(Some(StopOutcome::TimedOut(format!("typed actor {path} stop exceeded configured deadline"))));
                }).await;
                control.begin_cleanup(control.deadline());
                let (result, mut reason, cleanup_failed, failure) = match execution {
                    Ok(Ok(exit)) => (exit.result, Some(exit.reason), exit.cleanup_failed, exit.failure),
                    error => (Err(SystemError::ActorCreationError(format!("typed actor {path} execution failed without a disposal receipt: {error:?}"))), None, false, None),
                };
                let mut result = result.map_err(crate::error_receipt::SystemReceipt::new);
                let mut retirement = if cleanup_failed { result.as_ref().map(|_| ()).map_err(|error| error.actor_receipt()) } else { Ok(()) };
                let disposal = async {
                    let (actor, input_policy, input_owner) = tokio::join!(async { match unpublished_actor { Some(receipt) => receipt.await, None => Ok(()) } }, unpublished_policy, unpublished_inputs);
                    let inputs = async {
                        loop {
                            if let Some(result) = input_retirement.borrow_and_update().clone() {
                                return result.map_err(retirement_error);
                            }
                            input_retirement.changed().await.map_err(|_| SystemError::ShutdownError("typed input owner exited without retirement receipt".into()))?;
                        }
                    }.await;
                    let factory = match factory { Some(factory) => factory.retire_in_lifecycle().await, None => Ok(()) };
                    let children = control.tree.join().await;
                    let failure = failure.or_else(|| control.escalated.lock().unwrap().take());
                    let mut supervised=Ok(());
                    let planned=control.tree.take_failure();
                    if !control.explicit_stop.load(std::sync::atomic::Ordering::Acquire)
                        && let Some((error, policy))=planned {
                        supervised=policy.stop_siblings().await;
                        if policy.decision==parrot_api::supervisor::SupervisionDecision::Escalate && tokio::time::Instant::now()<control.deadline() && !control.explicit_stop.load(std::sync::atomic::Ordering::Acquire) { control.lifetime.escalate(error); }
                    } else if !control.explicit_stop.load(std::sync::atomic::Ordering::Acquire)
                        && let Some(error)=failure {
                        match control.lifetime.failure_policy(error.clone(), control.policy_slots.clone(), control.deadline()).await {
                            Ok(Some(policy)) if !control.explicit_stop.load(std::sync::atomic::Ordering::Acquire) && tokio::time::Instant::now()<control.deadline() => {
                                supervised=policy.stop_siblings().await;
                                if policy.decision==parrot_api::supervisor::SupervisionDecision::Escalate { control.lifetime.escalate(error); }
                            },
                            Ok(_) => {}, Err(error) => supervised=Err(error),
                        }
                    }
                    let policy = control.lifetime.dispose_policy().await;
                    inputs.and(actor.and(input_policy).and(input_owner).and(factory).and(children).and(supervised).and(policy).map_err(|error| SystemError::Other(anyhow::Error::new(error))))
                };
                let (disposed, disposal_timed_out) = control.join_until_expired(disposal, || {
                    finished.send_replace(Some(StopOutcome::TimedOut(format!("typed actor {path} policy disposal exceeded configured deadline"))));
                }).await;
                timed_out |= disposal_timed_out;
                let disposed = disposed.map_err(crate::error_receipt::SystemReceipt::new);
                if let Err(error) = &disposed && retirement.is_ok() { retirement=Err(error.actor_receipt()); }
                if let Err(error) = disposed && !cleanup_failed {
                    reason = reason.map(|_| DeathReason::Failed(crate::error_receipt::system_summary(error.error())));
                    result = Err(error);
                }
                if let Some(system) = system.upgrade() {
                    if let Some(reason) = &reason {
                        // Late disposal gets a new notification budget; the
                        // original stop receipt remains timed out forever.
                        let delivery_deadline = if timed_out {
                            tokio::time::Instant::now() + control.timeout
                        } else { control.deadline() };
                        result = result.and(system.notify_termination_until(&path, reason.clone(), delivery_deadline).await.map_err(crate::error_receipt::SystemReceipt::new));
                    } else {
                        system.forget_termination_watchers(&path);
                    }
                    let mut registry = system.registry.write().unwrap();
                    if matches!(registry.get(&path), Some(ActorRegistryEntry::Typed(entry)) if Arc::ptr_eq(&entry.control, &control)) {
                        registry.remove(&path);
                    }
                }
                let notification=control.lifetime.finish(retirement);
                let outcome = StopOutcome::from_result(result.map_err(crate::error_receipt::SystemReceipt::into_error));
                if !timed_out { finished.send_replace(Some(outcome.clone())); }
                if let Some(reason) = reason {
                    control.death_watch.terminated(parrot_api::context::ActorTerminated {
                        generation: control.death_watch.id(), path, reason,
                    }, control.timeout).await;
                } else {
                    // Executor cancellation is not proof of actor disposal.
                    control.death_watch.abandon();
                }
                retired.send_replace(Some(outcome));
                notification.await;
                control.release_instance_admission();
            });
            }
            Ok(actor_ref)
        };
        let admission = cohort
            .map_or(Ok(()), |cohort| cohort.validate(&self.root_cohorts))
            .and_then(|()| {
                if let Some(scope) = parent
                    .map(|parent| &parent.scope)
                    .or_else(|| cohort.map(|cohort| &cohort.scope))
                {
                    let mut reference = None;
                    let result = scope.spawn(|lifetime| {
                        let actor = admit(Some(lifetime.clone()))?;
                        let erased = actor.clone().into_dyn();
                        let stop = erased.clone_boxed();
                        reference = Some(actor);
                        Ok((
                            erased,
                            Arc::new(move || {
                                let actor = stop.clone_boxed();
                                let lifetime = lifetime.clone();
                                Box::pin(async move {
                                    let _ = actor.stop().await;
                                    lifetime.wait().await
                                })
                            }),
                        ))
                    });
                    result.map(|_| reference.expect("admitted typed child"))
                } else {
                    admit(None)
                }
            });
        match admission {
            Ok(reference) => Ok(reference),
            Err(error) => {
                retire_source(payload, factory, policy).await?;
                Err(error)
            }
        }
    }

    /// M4: 静态轨 spawn（单一协议便捷入口，无需 derive 枚举信封）。
    ///
    /// 适配器 `SingleDispatch` 把单个 `TypedReceive<M>` 包装成
    /// `ParrotTypedDispatch`（枚举只有一个变体），零 Any 语义不变。
    /// 用户消息经 `SingleVariant::wrap` 包装后 ask/tell（覆盖规则
    /// 要求绑定类型为本地类型）。
    pub fn spawn_typed_single<'a, A, M>(
        self: &'a Arc<Self>,
        actor: A,
        path: &'a str,
    ) -> impl std::future::Future<
        Output = ActorResult<TypedActorRef<SingleDispatch<A, M>, SingleVariant<A, M>>>,
    > + Send
    + 'a
    where
        A: TypedReceive<M>,
        M: Message,
    {
        self.spawn_typed::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            SingleDispatch::new(actor),
            path,
        )
    }

    /// Single-protocol static instance with an explicit handler failure strategy.
    pub fn spawn_typed_single_with_strategy<'a, A, M>(
        self: &'a Arc<Self>,
        actor: A,
        path: &'a str,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> impl std::future::Future<
        Output = ActorResult<
            TypedActorRef<
                crate::thread::typed::SingleDispatch<A, M>,
                crate::thread::typed::SingleVariant<A, M>,
            >,
        >,
    > + Send
    + 'a
    where
        A: TypedReceive<M>,
        M: parrot_api::message::Message,
    {
        use crate::thread::typed::{SingleDispatch, SingleVariant};
        self.spawn_typed_with_strategy::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            SingleDispatch::new(actor),
            path,
            strategy,
        )
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

    fn termination_protocol() -> Option<parrot_api::typed::TerminationProtocol<Self::Msg>> {
        if std::any::TypeId::of::<M>()
            == std::any::TypeId::of::<parrot_api::context::ActorTerminated>()
        {
            Some(|event| parrot_api::typed::cast_termination::<M>(event).map(SingleMsg::Msg))
        } else {
            None
        }
    }

    fn dispatch_with_context<'a>(
        &'a mut self,
        msg: Self::Msg,
        context: &'a parrot_api::typed_context::TypedContext,
    ) -> BoxedFuture<'a, ActorResult<Self::Reply>> {
        Box::pin(async move {
            let SingleMsg::Msg(message) = msg;
            self.0
                .receive_typed_with_context(message, context)
                .await
                .map(SingleReply::Reply)
        })
    }
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

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use std::sync::{
        Condvar,
        atomic::{AtomicUsize, Ordering},
    };
    #[derive(Default)]
    struct Gate {
        entered: tokio::sync::Notify,
        open: Mutex<bool>,
        changed: Condvar,
    }
    impl Gate {
        fn block(&self) {
            self.entered.notify_one();
            let (_guard, timeout) = self
                .changed
                .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(5), |open| {
                    !*open
                })
                .unwrap();
            assert!(!timeout.timed_out(), "test must release mailbox destructor");
        }
        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    struct Queued {
        gate: Option<Arc<Gate>>,
        count: Arc<AtomicUsize>,
        panic: bool,
    }
    impl Drop for Queued {
        fn drop(&mut self) {
            if let Some(gate) = &self.gate {
                gate.block();
            }
            self.count.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "typed cancelled mailbox bomb");
        }
    }
    impl Message for Queued {
        type Result = ();
    }
    struct Unpolled(Arc<AtomicUsize>);
    impl ParrotTypedDispatch for Unpolled {
        type Msg = Queued;
        type Reply = ();
        fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn dispatch(&mut self, _: Queued) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    impl ParrotMsgVariant<Unpolled> for Queued {
        fn inject(message: Self) -> Self {
            message
        }
        fn extract(_: ()) -> ActorResult<()> {
            Ok(())
        }
    }
    async fn rejected_execution_waits_for_mailbox(panic: bool) {
        let system = ThreadActorSystem::shared(crate::thread::config::ThreadActorSystemConfig {
            shared_pool_size: 2,
            shutdown_timeout: Duration::from_millis(80),
            ..Default::default()
        });
        // Reject execution at its real admission await, before first user poll.
        // The outer lifecycle monitor and its independent cleanup pool stay live.
        system.typed_slots.close();
        let init = Arc::new(AtomicUsize::new(0));
        let actor = system
            .spawn_typed::<_, Queued>(Unpolled(init.clone()), "/rejected-typed")
            .await
            .unwrap();
        let control = match system
            .registry
            .read()
            .unwrap()
            .get("/rejected-typed")
            .unwrap()
        {
            ActorRegistryEntry::Typed(entry) => entry.control.clone(),
            _ => unreachable!(),
        };
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let count = Arc::new(AtomicUsize::new(0));
        actor
            .tell(Queued {
                gate: Some(gate.clone()),
                count: count.clone(),
                panic,
            })
            .await
            .unwrap();
        actor
            .tell(Queued {
                gate: None,
                count: count.clone(),
                panic: false,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .expect("owned mailbox cleanup never started");
        assert_eq!(init.load(Ordering::SeqCst), 0);
        let error = tokio::time::timeout(Duration::from_secs(1), actor.stop())
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("deadline"), "{error}");
        assert!(
            system
                .registry
                .read()
                .unwrap()
                .contains_key("/rejected-typed")
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "a timeout is not retirement"
        );
        gate.release();
        let retired =
            tokio::time::timeout(Duration::from_secs(2), await_stop(control.retirement()))
                .await
                .unwrap()
                .unwrap_err();
        if panic {
            assert!(
                retired.to_string().contains("typed cancelled mailbox bomb"),
                "{retired}"
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(
            !system
                .registry
                .read()
                .unwrap()
                .contains_key("/rejected-typed")
        );
        assert!(!actor.is_alive());
        assert!(actor.stop().await.is_err());
        system.shutdown_internal().await.unwrap();
    }
    #[tokio::test]
    async fn rejected_typed_execution_retains_path_until_mailbox_retires() {
        rejected_execution_waits_for_mailbox(false).await;
    }
    #[tokio::test]
    async fn rejected_typed_execution_reports_real_mailbox_disposal_error() {
        rejected_execution_waits_for_mailbox(true).await;
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;
    use tokio::time::Instant;

    struct Request;
    impl Message for Request {
        type Result = u64;
    }
    struct Probe(Arc<AtomicUsize>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl ParrotTypedDispatch for Probe {
        type Msg = Request;
        type Reply = u64;
        fn dispatch(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<u64>> {
            Box::pin(async { Ok(42) })
        }
    }
    impl ParrotMsgVariant<Probe> for Request {
        fn inject(message: Self) -> Self {
            message
        }
        fn extract(reply: u64) -> ActorResult<u64> {
            Ok(reply)
        }
    }

    async fn check_control<F, Fut>(check: F)
    where
        F: FnOnce(Arc<TypedControl>) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let system = ThreadActorSystem::shared(crate::thread::config::ThreadActorSystemConfig {
            shared_pool_size: 1,
            ..Default::default()
        });
        let drops = Arc::new(AtomicUsize::new(0));
        let actor = system
            .spawn_typed::<_, Request>(Probe(drops.clone()), "/deadline-probe")
            .await
            .unwrap();
        assert_eq!(actor.ask(Request).await.unwrap(), 42);
        let control = match system
            .registry
            .read()
            .unwrap()
            .get("/deadline-probe")
            .unwrap()
        {
            ActorRegistryEntry::Typed(entry) => entry.control.clone(),
            _ => unreachable!(),
        };
        tokio::time::pause();
        let checked = std::panic::AssertUnwindSafe(async move {
            let live = check(control.clone()).await;
            if live {
                // Complete the simulated successful recovery before resuming
                // real time. A leftover few-millisecond budget must not turn
                // external worker scheduling into a false liveness failure.
                let deadline = *control.deadline.lock().unwrap();
                if let Some(deadline) = deadline {
                    assert!(control.construction_complete(deadline).unwrap());
                }
            }
            live
        })
        .catch_unwind()
        .await;
        tokio::time::resume();
        // A cleared/replaced deadline must preserve real business dispatch, not
        // merely a helper's boolean. Use the real clock for external worker polls.
        let live_reply = if matches!(&checked, Ok(true)) {
            Some(tokio::time::timeout(Duration::from_secs(2), actor.ask(Request)).await)
        } else {
            None
        };
        let _ = system.shutdown_internal().await;
        let retired = tokio::time::timeout(Duration::from_secs(2), async {
            while parrot_api::system::ActorSystem::status(&*system).state
                != parrot_api::system::SystemState::Stopped
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        assert!(
            retired.is_ok(),
            "fixture must retire actors and workers even after a failed assertion"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        if let Err(payload) = checked {
            std::panic::resume_unwind(payload);
        }
        if let Some(reply) = live_reply {
            assert_eq!(
                reply.unwrap().unwrap(),
                42,
                "recovery must preserve live dispatch"
            );
        }
    }

    #[tokio::test]
    async fn ready_completion_at_or_after_expiry_keeps_timeout() {
        check_control(|control| async move {
            control.begin_construction(Instant::now() + Duration::from_millis(10));
            tokio::time::advance(Duration::from_millis(10)).await;
            for extra in [Duration::ZERO, Duration::from_millis(1)] {
                tokio::time::advance(extra).await;
                for expected in [Ok(()), Err("late cleanup error")] {
                    let published = AtomicUsize::new(0);
                    let (actual, timed_out) = control
                        .join_until_expired(std::future::ready(expected), || {
                            published.fetch_add(1, Ordering::SeqCst);
                        })
                        .await;
                    assert!(
                        timed_out,
                        "ready typed completion must retain the expired deadline"
                    );
                    assert_eq!(actual, expected, "actual retirement result must survive");
                    assert_eq!(published.load(Ordering::SeqCst), 1);
                }
            }
            false
        })
        .await;
    }

    #[tokio::test]
    async fn completion_ready_on_expired_repoll_keeps_timeout() {
        check_control(|control| async move {
            control.begin_construction(Instant::now() + Duration::from_millis(10));
            let (send, receive) = oneshot::channel();
            let published = AtomicUsize::new(0);
            let owner = control.join_until_expired(receive, || {
                published.fetch_add(1, Ordering::SeqCst);
            });
            tokio::pin!(owner);
            assert!(futures::poll!(owner.as_mut()).is_pending());
            tokio::time::advance(Duration::from_millis(11)).await;
            send.send(Err::<(), _>("late cleanup error")).unwrap();
            let (actual, timed_out) = owner.await;
            assert!(timed_out, "typed both-ready repoll must retain timeout");
            assert_eq!(actual.unwrap(), Err("late cleanup error"));
            assert_eq!(published.load(Ordering::SeqCst), 1);
            false
        })
        .await;
    }

    #[tokio::test]
    async fn cleared_recovery_deadline_does_not_expire_live_actor() {
        check_control(|control| async move {
            let deadline = control.begin_construction(Instant::now() + Duration::from_millis(10));
            let (send, receive) = oneshot::channel();
            let published = AtomicUsize::new(0);
            let owner = control.join_until_expired(receive, || {
                published.fetch_add(1, Ordering::SeqCst);
            });
            tokio::pin!(owner);
            assert!(futures::poll!(owner.as_mut()).is_pending());
            assert!(control.construction_complete(deadline).unwrap());
            tokio::time::advance(Duration::from_millis(11)).await;
            assert!(futures::poll!(owner.as_mut()).is_pending());
            assert_eq!(published.load(Ordering::SeqCst), 0);
            send.send(Ok::<(), &str>(())).unwrap();
            let (actual, timed_out) = owner.await;
            assert!(
                !timed_out,
                "cleared deadline must not remain captured by the owner"
            );
            assert_eq!(actual.unwrap(), Ok(()));
            true
        })
        .await;
    }

    #[tokio::test]
    async fn rearmed_recovery_uses_new_deadline_and_rejects_late_completion() {
        check_control(|control| async move {
            let old = control.begin_construction(Instant::now() + Duration::from_millis(10));
            let (send, receive) = oneshot::channel();
            let published = AtomicUsize::new(0);
            let owner = control.join_until_expired(receive, || {
                published.fetch_add(1, Ordering::SeqCst);
            });
            tokio::pin!(owner);
            assert!(futures::poll!(owner.as_mut()).is_pending());
            assert!(control.construction_complete(old).unwrap());
            tokio::time::advance(Duration::from_millis(11)).await;
            control.begin_construction(Instant::now() + Duration::from_millis(10));
            tokio::time::advance(Duration::from_millis(5)).await;
            assert!(futures::poll!(owner.as_mut()).is_pending());
            assert_eq!(
                published.load(Ordering::SeqCst),
                0,
                "old timer must not expire the new recovery"
            );
            tokio::time::advance(Duration::from_millis(6)).await;
            send.send(Ok::<(), &str>(())).unwrap();
            let (actual, timed_out) = owner.await;
            assert!(
                timed_out,
                "rearmed typed deadline must reject its late ready completion"
            );
            assert_eq!(actual.unwrap(), Ok(()));
            assert_eq!(published.load(Ordering::SeqCst), 1);
            false
        })
        .await;
    }

    #[tokio::test]
    async fn ready_completion_during_rearmed_budget_keeps_success() {
        check_control(|control| async move {
            let old = control.begin_construction(Instant::now() + Duration::from_millis(10));
            let (send, receive) = oneshot::channel();
            let published = AtomicUsize::new(0);
            let owner = control.join_until_expired(receive, || {
                published.fetch_add(1, Ordering::SeqCst);
            });
            tokio::pin!(owner);
            assert!(futures::poll!(owner.as_mut()).is_pending());
            assert!(control.construction_complete(old).unwrap());
            tokio::time::advance(Duration::from_millis(11)).await;
            control.begin_construction(Instant::now() + Duration::from_millis(10));
            tokio::time::advance(Duration::from_millis(5)).await;
            send.send(Ok::<(), &str>(())).unwrap();
            let (actual, timed_out) = owner.await;
            assert!(
                !timed_out,
                "completion inside the rearmed budget must not inherit the old expiry"
            );
            assert_eq!(actual.unwrap(), Ok(()));
            assert_eq!(published.load(Ordering::SeqCst), 0);
            true
        })
        .await;
    }

    #[tokio::test]
    async fn early_success_and_error_keep_original_outcomes() {
        check_control(|control| async move {
            for armed in [false, true] {
                if armed {
                    control.begin_construction(Instant::now() + Duration::from_secs(1));
                }
                for expected in [Ok(()), Err("early cleanup error")] {
                    let (actual, timed_out) = control
                        .join_until_expired(std::future::ready(expected), || {
                            panic!("early typed work timed out")
                        })
                        .await;
                    assert!(!timed_out);
                    assert_eq!(actual, expected);
                }
            }
            true
        })
        .await;
    }

    #[tokio::test]
    async fn timeout_publishes_before_cleanup_and_keeps_actual_error() {
        check_control(|control| async move {
            control.begin_construction(Instant::now() + Duration::from_millis(10));
            let (send, receive) = oneshot::channel();
            let drops = Arc::new(AtomicUsize::new(0));
            let capture = Probe(drops.clone());
            let published = AtomicUsize::new(0);
            let owner = control.join_until_expired(
                async move {
                    let _capture = capture;
                    receive.await.unwrap();
                    Err::<(), _>("actual cleanup error")
                },
                || {
                    published.fetch_add(1, Ordering::SeqCst);
                },
            );
            tokio::pin!(owner);
            assert!(futures::poll!(owner.as_mut()).is_pending());
            tokio::time::advance(Duration::from_millis(11)).await;
            assert!(futures::poll!(owner.as_mut()).is_pending());
            assert_eq!(published.load(Ordering::SeqCst), 1);
            assert_eq!(
                drops.load(Ordering::SeqCst),
                0,
                "timeout must retain pending cleanup"
            );
            send.send(()).unwrap();
            let (actual, timed_out) = owner.await;
            assert!(timed_out);
            assert_eq!(actual, Err("actual cleanup error"));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(published.load(Ordering::SeqCst), 1);
            false
        })
        .await;
    }
}
