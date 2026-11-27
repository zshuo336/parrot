use anyhow::anyhow;
use parrot_api::actor::{Actor, ActorState};
use parrot_api::address::ActorPath;
use parrot_api::errors::ActorError;
#[allow(unused_imports)] // 测试模块需要；lib 本体部分用
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::Arc;
use tracing::{debug, error, info};

use crate::thread::context::ThreadContext;
use crate::thread::envelope::{AskEnvelope, ControlMessage};
use crate::thread::stage::{StageCleanup, StageFailures, outcome};

mod message;
use message::MessageInput;

/// Thread-based implementation of an Actor.
///
/// This is a wrapper that adapts the generic Actor trait to the
/// thread-based execution model, handling message dispatching and lifecycle.
#[derive(Debug)]
pub struct ThreadActor<A>
where
    A: Actor + Send + Sync + 'static,
    // remove the Deref constraint on A::Context, and force the constraint that A::Context must be ThreadContext<A>
    A::Context: Send + 'static,
{
    /// The wrapped actor implementation
    inner: A,
    /// Current actor state
    state: ActorState,
    /// Actor path for addressing
    path: ActorPath,
    sender: parrot_api::sender::ActorSenderOwner,
    /// Set of watcher paths that are watching this actor
    watchers: Option<HashSet<String>>,
    /// Whether `Actor::init` has already run successfully. The state machine
    /// stays in `Starting` until the `Start` control message arrives, so the
    /// state alone cannot distinguish "not yet initialized" from
    /// "initialized, awaiting start".
    initialized: bool,
    initialization_deadline: Option<tokio::time::Instant>,
    // Failed/cancelled initialization is Stopped before shutdown has committed.
    shutdown_committed: bool,
    initialization_failure_context: Option<Option<parrot_api::trace::TraceContext>>,
    pub(crate) stage_failures: Arc<StageFailures>,
}

impl<A> ThreadActor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    /// Creates a new ThreadActor wrapping the provided actor implementation.
    pub fn new(actor: A, path: ActorPath) -> Self {
        Self {
            inner: actor,
            state: ActorState::Starting,
            sender: parrot_api::sender::ActorSenderOwner::new(path.path()),
            path,
            watchers: None,
            initialized: false,
            initialization_deadline: None,
            shutdown_committed: false,
            initialization_failure_context: None,
            stage_failures: Arc::new(StageFailures::default()),
        }
    }

    /// Creates a ThreadActor for unit tests with a placeholder path.
    pub fn new_for_test(actor: A) -> Self {
        Self::new(actor, ActorPath::placeholder("test://actor"))
    }
    pub(crate) fn with_initialization_deadline(
        mut self,
        deadline: Option<tokio::time::Instant>,
    ) -> Self {
        self.initialization_deadline = deadline;
        self
    }

    /// Gets the current state of the actor.
    pub fn state(&self) -> ActorState {
        self.state
    }

    /// Gets the actor's path.
    pub fn path(&self) -> &ActorPath {
        &self.path
    }

    pub(crate) fn initialization_complete(&self) -> bool {
        self.initialized
    }

    /// Initialize the actor.
    pub async fn initialize<'a>(&'a mut self, ctx: &'a mut ThreadContext<A>) -> ActorResult<()> {
        self.stage_failures.result()?;
        if self.initialized || self.state != ActorState::Starting {
            return Err(ActorError::Other(anyhow!("Actor is already initialized")));
        }

        debug!("Initializing actor at path: {:?}", self.path);

        // A polled initialization can mutate user storage before yielding.
        // Only full success makes that same instance eligible for Start.
        self.state = ActorState::Stopped;
        // current ctx is ThreadContext<A>, not type cast
        let mut cleanup = StageCleanup::new(self.stage_failures.clone());
        let timeout = ctx
            .system_weak_ref()
            .upgrade()
            .map_or(std::time::Duration::ZERO, |system| {
                system.operation_timeouts.actor_creation
            });
        let sender = self.sender.sender();
        let stage = sender
            .scope(crate::typed_supervision::recorded_lifecycle_stage(
                parrot_api::trace::SpanKind::ActorInit,
                self.path.path(),
                || self.inner.init(ctx),
                cleanup
                    .borrow()
                    .with_timeout(timeout)
                    .with_deadline(self.initialization_deadline),
            ))
            .await;
        match outcome(stage.result) {
            Ok(_) => {
                // Mark initialized; the transition to Running is handled by
                // process_message for the Start control message.
                self.initialized = true;
                self.state = ActorState::Starting;
                Ok(())
            }
            Err(e) => {
                self.initialization_failure_context = Some(stage.context);
                error!(
                    "Failed to initialize actor at {:?}: {}",
                    self.path,
                    crate::error_receipt::actor_summary(&e)
                );
                self.state = ActorState::Stopped;
                Err(e)
            }
        }
    }

    pub(crate) fn take_initialization_failure_context(
        &mut self,
    ) -> Option<Option<parrot_api::trace::TraceContext>> {
        self.initialization_failure_context.take()
    }

    /// Add an actor to the watchers list
    pub fn add_watcher(&mut self, watcher_path: String) {
        if self.watchers.is_none() {
            self.watchers = Some(HashSet::new());
        }
        self.watchers.as_mut().unwrap().insert(watcher_path);
    }

    /// Remove an actor from the watchers list
    pub fn remove_watcher(&mut self, watcher_path: &str) {
        if let Some(watchers) = self.watchers.as_mut() {
            watchers.remove(watcher_path);
        }
    }

    /// Get the watchers list (empty if none registered)
    pub fn watchers(&self) -> &HashSet<String> {
        static EMPTY: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
        self.watchers
            .as_ref()
            .unwrap_or_else(|| EMPTY.get_or_init(HashSet::new))
    }

    /// Process a message when polled. Cancellation before the first poll also
    /// retires an unpublished input and preserves any disposal failure.
    pub fn process_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut ThreadContext<A>,
    ) -> impl Future<Output = ActorResult<BoxedMessage>> + Send + 'a {
        let input = MessageInput::new(msg, self.stage_failures.clone());
        self.sender
            .sender()
            .scope(self.process_message_inner(input, ctx))
    }

    async fn process_message_inner<'a>(
        &'a mut self,
        mut msg: MessageInput,
        ctx: &'a mut ThreadContext<A>,
    ) -> ActorResult<BoxedMessage> {
        // Handle control messages specially
        if let Some(control_msg) = msg.value().downcast_ref::<ControlMessage>() {
            return self.handle_control_message(control_msg, ctx).await;
        }

        // Handle watch request
        if let Some(watch_req) = msg
            .value()
            .downcast_ref::<crate::thread::system::WatchRequest>()
        {
            debug!(
                "Actor at {:?} received watch request from {:?}",
                self.path, watch_req.watcher_path
            );
            self.add_watcher(watch_req.watcher_path.clone());
            return Ok(Box::new(()));
        }

        // Handle unwatch request
        if let Some(unwatch_req) = msg
            .value()
            .downcast_ref::<crate::thread::system::UnwatchRequest>()
        {
            debug!(
                "Actor at {:?} received unwatch request from {:?}",
                self.path, unwatch_req.watcher_path
            );
            self.remove_watcher(&unwatch_req.watcher_path);
            return Ok(Box::new(()));
        }

        // Only process regular messages if the actor is running
        if let Err(error) = self.stage_failures.result() {
            let _ = msg.retire();
            return Err(error);
        }
        if self.state != ActorState::Running {
            msg.retire()?;
            return Err(ActorError::Other(anyhow!("Actor is not running")));
        }

        let mut msg = msg.take();
        let _activity = if msg.is::<crate::receive_timeout::IdleSignal>() {
            let signal = *msg
                .downcast::<crate::receive_timeout::IdleSignal>()
                .unwrap();
            let Some(activity) = ctx.idle.claim(signal) else {
                return Ok(Box::new(()));
            };
            msg = Box::new(parrot_api::context::LifecycleEvent::ReceiveTimeout);
            activity
        } else {
            ctx.idle.enter()
        };

        if msg.is::<crate::children::ChildTerminated>() {
            let mut cleanup = StageCleanup::new(self.stage_failures.clone());
            outcome(
                crate::typed_supervision::lifecycle_stage(
                    || {
                        self.inner.handle_child_terminated(
                            msg.downcast::<crate::children::ChildTerminated>()
                                .unwrap()
                                .into_actor(),
                            ctx,
                        )
                    },
                    cleanup.borrow(),
                )
                .await,
            )?;
            return Ok(Box::new(()));
        }
        if msg.is::<crate::streams::StreamEvent>() {
            let timeout = ctx
                .system_weak_ref()
                .upgrade()
                .map_or(std::time::Duration::ZERO, |system| {
                    system.operation_timeouts.message_handling
                });
            let mut cleanup = StageCleanup::new(self.stage_failures.clone());
            return crate::streams::dispatch(
                *msg.downcast::<crate::streams::StreamEvent>().unwrap(),
                &mut self.inner,
                ctx,
                timeout,
                cleanup.borrow(),
            )
            .await;
        }
        // Handle Ask messages specially
        if msg.is::<AskEnvelope>() {
            return self
                .handle_ask_envelope(*msg.downcast::<AskEnvelope>().unwrap(), ctx)
                .await;
        }

        // Handle Ask messages specially
        // call inner actor's receive_message
        self.receive(msg, ctx).await
    }

    async fn receive(
        &mut self,
        msg: BoxedMessage,
        ctx: &mut ThreadContext<A>,
    ) -> ActorResult<BoxedMessage> {
        let mut cleanup = StageCleanup::new(self.stage_failures.clone());
        let timeout = ctx
            .system_weak_ref()
            .upgrade()
            .map_or(std::time::Duration::ZERO, |system| {
                system.operation_timeouts.message_handling
            });
        outcome(
            crate::typed_supervision::guarded_stage(
                || self.inner.receive_message(msg, ctx),
                cleanup.borrow().with_timeout(timeout),
            )
            .await,
        )
    }

    /// Exclude engine control and legacy envelopes before consulting user policy.
    pub(crate) fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        self.state == ActorState::Running
            && !message.is::<ControlMessage>()
            && !message.is::<crate::receive_timeout::IdleSignal>()
            && !message.is::<crate::streams::StreamEvent>()
            && !message.is::<crate::children::ChildTerminated>()
            && !message.is::<parrot_api::context::ActorTerminated>()
            && !message.is::<crate::thread::system::Terminated>()
            && !message.is::<crate::thread::system::WatchRequest>()
            && !message.is::<crate::thread::system::UnwatchRequest>()
            && !message.is::<AskEnvelope>()
            && self.inner.can_batch_message(message)
    }

    pub(crate) async fn process_business_batch(
        &mut self,
        messages: Vec<BoxedMessage>,
        ctx: &mut ThreadContext<A>,
    ) -> Vec<ActorResult<BoxedMessage>> {
        if self.state != ActorState::Running {
            return messages
                .into_iter()
                .map(|_| Err(ActorError::Stopped))
                .collect();
        }
        let _activity = ctx.idle.enter();
        let count = messages.len();
        let timeout = ctx
            .system_weak_ref()
            .upgrade()
            .map_or(std::time::Duration::ZERO, |system| {
                system.operation_timeouts.message_handling
            });
        let mut cleanup = StageCleanup::new(self.stage_failures.clone());
        let batch = cleanup.batch();
        outcome(
            self.sender
                .sender()
                .scope(parrot_api::batch_dispatch::scope(
                    batch,
                    crate::typed_supervision::guarded_batch_stage(
                        || self.inner.receive_batch(messages, ctx),
                        cleanup.borrow().with_timeout(timeout),
                    ),
                ))
                .await,
        )
        .unwrap_or_else(|error| {
            let receipt = crate::error_receipt::ActorReceipt::from_error(error);
            (0..count)
                .map(|_| Err(receipt.clone().into_error()))
                .collect()
        })
    }

    /// Handle internal control messages
    async fn handle_control_message<'a>(
        &'a mut self,
        control_msg: &ControlMessage,
        ctx: &'a mut ThreadContext<A>,
    ) -> ActorResult<BoxedMessage> {
        match control_msg {
            ControlMessage::Start => {
                self.stage_failures.result()?;
                debug!("Handling Start message for actor at {:?}", self.path);
                match self.state {
                    ActorState::Starting if self.initialized => {
                        self.state = ActorState::Running;
                        ctx.idle.activate();
                        info!("Actor at {:?} is now running", self.path);
                        Ok(Box::new(()))
                    }
                    ActorState::Starting => Err(ActorError::InitializationError(
                        "actor must complete initialization before Start".into(),
                    )),
                    ActorState::Running => Ok(Box::new(())),
                    ActorState::Stopping | ActorState::Stopped => Err(ActorError::Stopped),
                }
            }

            ControlMessage::Stop => {
                debug!("Handling Stop message for actor at {:?}", self.path);
                // Shut down the actor
                self.shutdown(ctx).await?;
                Ok(Box::new(()))
            }

            ControlMessage::ChildFailure { .. } => Err(ActorError::MessageHandlingError(
                "path-only ChildFailure cannot establish a generation failure or termination; managed supervision uses private ownership endpoints".into(),
            )),

            ControlMessage::SystemShutdown => {
                debug!(
                    "Handling SystemShutdown message for actor at {:?}",
                    self.path
                );
                // The system is shutting down, stop the actor
                self.shutdown(ctx).await?;
                Ok(Box::new(()))
            }

            ControlMessage::HealthCheck => {
                debug!("Handling HealthCheck message for actor at {:?}", self.path);
                // Just return the current state
                Ok(Box::new(self.state))
            }
        }
    }

    /// Handle ask envelopes
    async fn handle_ask_envelope<'a>(
        &'a mut self,
        mut envelope: AskEnvelope,
        ctx: &'a mut ThreadContext<A>,
    ) -> ActorResult<BoxedMessage> {
        // Only process messages if the actor is running
        if self.state != ActorState::Running {
            return Err(ActorError::Other(anyhow!("Actor is not running")));
        }

        // Process the message with the inner actor
        let reply = envelope.take_reply();
        let payload = envelope.payload.into_boxed();
        match self.receive(payload, ctx).await {
            Ok(response) => {
                // Send the reply and return an empty response.
                // A dropped receiver (asker timed out) is not an error here.
                let (_, cleanup) = super::reply::PendingReply::new(reply, Ok(response)).send();
                message::record_cleanup(
                    cleanup,
                    &self.stage_failures,
                    "direct ask reply disposal failed",
                )?;
                Ok(Box::new(()))
            }
            Err(e) => {
                // Send error reply
                let error = crate::error_receipt::ActorReceipt::from_error(e);
                let (_, cleanup) = super::reply::PendingReply::new(
                    reply,
                    Err(crate::supervision_policy::reply_error(error.clone())),
                )
                .send();
                let _ = message::record_cleanup(
                    cleanup,
                    &self.stage_failures,
                    "direct ask reply disposal failed",
                );
                Err(crate::supervision_policy::reply_error(error))
            }
        }
    }

    /// Shut down the actor.
    pub async fn shutdown<'a>(&'a mut self, ctx: &'a mut ThreadContext<A>) -> ActorResult<()> {
        if self.shutdown_committed {
            return self.stage_failures.result();
        }

        debug!("Shutting down actor at {:?}", self.path);

        // Transition to Stopping state
        self.state = ActorState::Stopping;
        ctx.idle.close();

        let result = if self.initialized {
            let mut cleanup = StageCleanup::new(self.stage_failures.clone());
            let sender = self.sender.sender();
            let result = outcome(
                sender
                    .scope(crate::typed_supervision::recorded_lifecycle_stage(
                        parrot_api::trace::SpanKind::ActorBeforeStop,
                        self.path.path(),
                        || self.inner.before_stop(ctx),
                        cleanup.borrow(),
                    ))
                    .await
                    .result,
            )
            .map_err(crate::error_receipt::protect_actor);
            drop(cleanup);
            result
        } else {
            Ok(())
        };

        // Transition to Stopped state regardless of result
        self.state = ActorState::Stopped;
        self.shutdown_committed = true;
        self.sender.retire();

        // Notify all watchers
        self.notify_watchers_of_termination(ctx).await;

        info!("Actor at {:?} has stopped", self.path);

        self.stage_failures.result().and(result)
    }

    /// Notify all watchers that this actor has terminated
    async fn notify_watchers_of_termination(&self, ctx: &ThreadContext<A>) {
        if let Some(watchers) = self.watchers.as_ref() {
            debug!(
                "Notifying {} watchers of termination for actor at {:?}",
                watchers.len(),
                self.path
            );

            // This legacy path predates the generation-aware DeathWatch owned
            // by the system registry. Keep it functional for direct
            // `ThreadActor` users by resolving each watcher through the same
            // system registry and delivering the compatibility `Terminated`
            // message. Managed actors use the newer bounded channel and do not
            // pass through this fallback.
            let Some(system) = ctx.system_weak_ref().upgrade() else {
                return;
            };
            let notify_timeout = system.config().shutdown_timeout;
            for watcher_path in watchers {
                let Some(watcher) = system.get_actor_ref(watcher_path) else {
                    continue;
                };
                let event = crate::thread::system::Terminated {
                    path: self.path.path.clone(),
                    reason: parrot_api::supervisor::DeathReason::Normal,
                };
                match tokio::time::timeout(notify_timeout, watcher.deliver(Box::new(event))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        debug!(
                            watcher = %watcher_path,
                            error = %error,
                            "legacy termination notification was not delivered"
                        );
                    }
                    Err(_) => {
                        debug!(
                            watcher = %watcher_path,
                            "legacy termination notification exceeded shutdown deadline"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::context::ThreadContext;
    use parrot_api::actor::EmptyConfig;
    use parrot_api::address::ActorRef;
    use std::sync::Arc;

    /// Actor that records lifecycle callbacks.
    #[derive(Debug, Default)]
    struct LifecycleActor {
        inits: u32,
        messages: Vec<String>,
        before_stop_called: bool,
    }

    impl Actor for LifecycleActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.inits += 1;
                Ok(())
            })
        }

        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if let Some(s) = msg.downcast_ref::<String>() {
                    self.messages.push(s.clone());
                }
                Ok(msg)
            })
        }

        fn before_stop<'a>(
            &'a mut self,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.before_stop_called = true;
                Ok(())
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    fn make_actor() -> (ThreadActor<LifecycleActor>, ThreadContext<LifecycleActor>) {
        (
            ThreadActor::new_for_test(LifecycleActor::default()),
            ThreadContext::new_for_test("test/thread-actor"),
        )
    }

    #[tokio::test]
    async fn test_initialize_transitions_and_allows_start() {
        let (mut actor, mut ctx) = make_actor();
        assert_eq!(actor.state(), ActorState::Starting);

        actor.initialize(&mut ctx).await.expect("init ok");
        // Still Starting until the Start control message arrives.
        assert_eq!(actor.state(), ActorState::Starting);
    }

    #[tokio::test]
    async fn test_initialize_twice_errors() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();

        let second = actor.initialize(&mut ctx).await;
        assert!(second.is_err(), "double init must be rejected");
    }

    #[tokio::test]
    async fn test_start_control_message_transitions_to_running() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();

        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();
        assert_eq!(actor.state(), ActorState::Running);
    }

    #[tokio::test]
    async fn test_regular_message_rejected_before_running() {
        let (mut actor, mut ctx) = make_actor();
        // Not started yet: regular messages must be rejected.
        let result = actor
            .process_message(Box::new(String::from("hi")), &mut ctx)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_regular_message_processed_when_running() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();
        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();

        let reply = actor
            .process_message(Box::new(String::from("hello")), &mut ctx)
            .await
            .unwrap();
        let echoed = reply.downcast::<String>().expect("echo is String");
        assert_eq!(*echoed, "hello");
    }

    #[tokio::test]
    async fn test_stop_control_message_shuts_down_and_calls_before_stop() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();
        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();

        actor
            .process_message(Box::new(ControlMessage::Stop), &mut ctx)
            .await
            .unwrap();
        assert_eq!(actor.state(), ActorState::Stopped);
    }

    #[tokio::test]
    async fn test_shutdown_is_idempotent() {
        let (mut actor, mut ctx) = make_actor();
        actor.shutdown(&mut ctx).await.unwrap();
        // Second shutdown is a no-op.
        actor.shutdown(&mut ctx).await.unwrap();
        assert_eq!(actor.state(), ActorState::Stopped);
    }

    #[tokio::test]
    async fn test_health_check_returns_state() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();
        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();

        let reply = actor
            .process_message(Box::new(ControlMessage::HealthCheck), &mut ctx)
            .await
            .unwrap();
        let state = reply.downcast::<ActorState>().expect("state payload");
        assert_eq!(*state, ActorState::Running);
    }

    #[tokio::test]
    async fn test_watch_request_registers_watcher() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();
        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();

        actor
            .process_message(
                Box::new(crate::thread::system::WatchRequest {
                    watcher_path: "/user/watcher".into(),
                }),
                &mut ctx,
            )
            .await
            .unwrap();

        assert!(actor.watchers().contains("/user/watcher"));

        // Unwatch removes it again.
        actor
            .process_message(
                Box::new(crate::thread::system::UnwatchRequest {
                    watcher_path: "/user/watcher".into(),
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!actor.watchers().contains("/user/watcher"));
    }

    #[tokio::test]
    async fn test_watchers_empty_by_default() {
        let (actor, _ctx) = make_actor();
        assert!(actor.watchers().is_empty());
    }

    #[tokio::test]
    async fn test_ask_envelope_replies_through_channel() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();
        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();

        let (envelope, rx) = AskEnvelope::new(Box::new(String::from("ping")));
        let reply = actor
            .process_message(Box::new(envelope), &mut ctx)
            .await
            .unwrap();

        // The actor consumes the envelope and returns unit; the actual reply
        // arrives on the oneshot channel.
        let _ = reply.downcast::<()>().expect("unit ack");
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("ask reply arrives")
            .expect("channel open");
        let payload = response.expect("Ok reply");
        assert_eq!(*payload.downcast::<String>().unwrap(), "ping");
    }

    #[tokio::test]
    async fn test_ask_envelope_rejected_when_not_running() {
        let (mut actor, mut ctx) = make_actor();
        let (envelope, rx) = AskEnvelope::new(Box::new(String::from("ping")));

        let result = actor.process_message(Box::new(envelope), &mut ctx).await;
        assert!(result.is_err(), "ask before running must fail");

        // Dropping rx here must not panic.
        drop(rx);
    }

    #[tokio::test]
    async fn test_failed_init_marks_actor_stopped() {
        #[derive(Debug, Default)]
        struct FailingActor;

        impl Actor for FailingActor {
            type Config = EmptyConfig;
            type Context = ThreadContext<Self>;

            fn init<'a>(
                &'a mut self,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Err(ActorError::MessageHandlingError("init boom".into())) })
            }

            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { Ok(msg) })
            }

            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }

        let (mut actor, mut ctx) = (
            ThreadActor::new_for_test(FailingActor),
            ThreadContext::new_for_test("test/failing"),
        );
        let result = actor.initialize(&mut ctx).await;
        assert!(result.is_err());
        assert_eq!(actor.state(), ActorState::Stopped);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_watcher_receives_termination_event() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Watcher(Arc<AtomicUsize>);
        impl Actor for Watcher {
            type Config = EmptyConfig;
            type Context = ThreadContext<Self>;

            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                if msg.is::<crate::thread::system::Terminated>() {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
                Box::pin(async move { Ok(msg) })
            }

            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }

        #[derive(Clone, Copy)]
        enum Begin {
            Fresh,
            Ready,
            Failed,
            Cancelled,
        }
        struct Direct {
            mode: Begin,
            stops: Arc<AtomicUsize>,
        }
        impl Actor for Direct {
            type Config = EmptyConfig;
            type Context = ThreadContext<Self>;
            fn init<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    match self.mode {
                        Begin::Cancelled => std::future::pending().await,
                        Begin::Failed => {
                            Err(ActorError::InitializationError("init refused".into()))
                        }
                        Begin::Fresh | Begin::Ready => Ok(()),
                    }
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { Ok(msg) })
            }
            fn before_stop<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                self.stops.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }

        let system = crate::thread::system::ThreadActorSystem::shared(
            crate::thread::config::ThreadActorSystemConfig::default(),
        );
        let seen = Arc::new(AtomicUsize::new(0));
        let watcher = system
            .spawn_at(
                Watcher(seen.clone()),
                "/user/legacy-watcher",
                None,
                crate::thread::config::ThreadActorConfig::default(),
            )
            .await
            .unwrap();

        let mut failures = Vec::new();
        for (name, mode) in [
            ("fresh", Begin::Fresh),
            ("ready", Begin::Ready),
            ("failed", Begin::Failed),
            ("cancelled", Begin::Cancelled),
        ] {
            let stops = Arc::new(AtomicUsize::new(0));
            let path = format!("/user/direct-legacy-{name}");
            let mut direct = ThreadActor::new(
                Direct {
                    mode,
                    stops: stops.clone(),
                },
                ActorPath::for_test(&path),
            );
            direct.add_watcher(watcher.path().to_string());
            let mut context =
                ThreadContext::<Direct>::new_for_test_with(&path, Arc::downgrade(&system));
            match mode {
                Begin::Fresh => {}
                Begin::Ready => {
                    direct.initialize(&mut context).await.unwrap();
                    direct
                        .process_message(Box::new(ControlMessage::Start), &mut context)
                        .await
                        .unwrap();
                }
                Begin::Failed => assert!(direct.initialize(&mut context).await.is_err()),
                Begin::Cancelled => {
                    use std::future::Future;
                    let mut init = Box::pin(direct.initialize(&mut context));
                    assert!(
                        init.as_mut()
                            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                            .is_pending()
                    );
                    drop(init);
                }
            }
            let before = seen.load(Ordering::SeqCst);
            direct.shutdown(&mut context).await.unwrap();
            direct.shutdown(&mut context).await.unwrap();
            // A follow-up ask is a mailbox barrier for both shutdown calls.
            watcher.send(Box::new(()) as BoxedMessage).await.unwrap();
            let notified = seen.load(Ordering::SeqCst) - before;
            if notified != 1 {
                failures.push(format!(
                    "{name}: expected one termination, observed {notified}"
                ));
            }
            assert_eq!(
                stops.load(Ordering::SeqCst),
                usize::from(matches!(mode, Begin::Ready))
            );
            assert_eq!(direct.state(), ActorState::Stopped);
        }
        system.shutdown_internal().await.unwrap();
        assert!(
            failures.is_empty(),
            "legacy termination missing or duplicated: {failures:?}"
        );
    }
}
