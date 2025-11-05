//! Typed actor reference for the thread-based actor system.

use std::any::Any;
use std::marker::PhantomData;
#[allow(unused_imports)] // 测试模块需要 Weak
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;

use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::errors::ActorError;
use parrot_api::message::MessageOptions;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};

use crate::thread::config::BackpressureStrategy;
use crate::thread::envelope::AskEnvelope;
use crate::thread::mailbox::{Mailbox, WeakMailboxRef};
use crate::thread::scheduler::ThreadScheduler;

pub(crate) type StopRequest = Arc<tokio::sync::OnceCell<crate::thread::system::StopCompletion>>;

/// A typed reference to an actor running on the thread engine.
///
/// Holds only a *weak* reference to the actor's mailbox so the reference
/// never keeps a stopped actor's mailbox alive. The weak pointer sits in an
/// `Arc<Mutex<..>>` so supervised generations share one logical address.
/// It can be installed after construction (the mailbox
/// is created after the ref during spawn) while the ref itself is shared
/// through `Arc`.
///
/// `send_msg` and [`ActorRef::deliver`] enqueue without waiting for a reply.
/// [`ActorRef::send`] and `ask` await a response carried by an [`AskEnvelope`].
/// For system-registered references, [`ActorRef::stop`] waits for the owning
/// system's lifecycle operation under its configured shutdown deadline.
/// A stopped logical reference cannot stop an unrelated actor manually spawned
/// later at the same path. A supervised reference follows its replacements.
pub struct ThreadActorRef<A>
where
    A: parrot_api::actor::Actor<Context = crate::thread::context::ThreadContext<A>>
        + Send
        + Sync
        + 'static,
{
    /// Actor path (string copy; mailbox holds the full ActorPath)
    path: String,

    /// Weak reference to the actor's mailbox (interior mutability for late install)
    mailbox: Arc<Mutex<WeakMailboxRef>>,

    /// Owning system for registered actor lifecycle operations.
    system: crate::thread::context::WeakSystemRef,
    death_watch: Option<parrot_api::death_watch::DeathWatch>,
    registered: bool,
    stop_request: StopRequest,

    /// Default backpressure strategy for sends
    default_strategy: BackpressureStrategy,

    /// Default timeout for asks
    default_timeout: Duration,

    /// Optional typed scheduler handle (kept for engine-specific operations)
    scheduler: Option<Arc<dyn ThreadScheduler>>,

    /// Marker for the actor type
    _marker: PhantomData<fn() -> A>,
}

impl<A> ThreadActorRef<A>
where
    A: parrot_api::actor::Actor<Context = crate::thread::context::ThreadContext<A>>
        + Send
        + Sync
        + 'static,
{
    /// Create a new typed actor reference.
    pub fn new(
        path: ActorPath,
        mailbox: WeakMailboxRef,
        default_strategy: BackpressureStrategy,
        default_timeout: Duration,
        scheduler: Option<Arc<dyn ThreadScheduler>>,
    ) -> Self {
        Self {
            path: path.path,
            mailbox: Arc::new(Mutex::new(mailbox)),
            system: Weak::new(),
            death_watch: None,
            registered: false,
            stop_request: Default::default(),
            default_strategy,
            default_timeout,
            scheduler,
            _marker: PhantomData,
        }
    }

    pub(crate) fn with_system(mut self, system: crate::thread::context::WeakSystemRef) -> Self {
        self.system = system;
        self.registered = true;
        self
    }

    /// Build a managed view from its logical owner's existing address and stop
    /// receipt. Avoid allocating temporary cells only to replace them at once.
    pub(crate) fn registered(
        path: String,
        mailbox: Arc<Mutex<WeakMailboxRef>>,
        stop_request: StopRequest,
        system: crate::thread::context::WeakSystemRef,
        default_strategy: BackpressureStrategy,
        default_timeout: Duration,
    ) -> Self {
        Self {
            path,
            mailbox,
            system,
            death_watch: None,
            registered: true,
            stop_request,
            default_strategy,
            default_timeout,
            scheduler: None,
            _marker: PhantomData,
        }
    }

    pub(crate) fn with_death_watch(mut self, watch: parrot_api::death_watch::DeathWatch) -> Self {
        self.death_watch = Some(watch);
        self
    }

    pub(crate) fn stop_request(&self) -> StopRequest {
        self.stop_request.clone()
    }

    pub(crate) fn with_stop_request(mut self, request: StopRequest) -> Self {
        self.stop_request = request;
        self
    }

    /// Get the actor path string.
    pub fn actor_path(&self) -> &str {
        &self.path
    }

    /// Get the default ask timeout.
    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    /// Get the default backpressure strategy.
    pub fn default_strategy(&self) -> &BackpressureStrategy {
        &self.default_strategy
    }

    /// Share the logical address across supervised generations.
    pub(crate) fn with_mailbox_cell(mut self, mailbox: Arc<Mutex<WeakMailboxRef>>) -> Self {
        self.mailbox = mailbox;
        self
    }

    pub(crate) fn mailbox_cell(&self) -> Arc<Mutex<WeakMailboxRef>> {
        self.mailbox.clone()
    }

    /// Set (or fix up) the weak mailbox reference.
    ///
    /// Used during spawn: the ref is created before the mailbox, then the
    /// mailbox pointer is installed once the mailbox exists.
    pub fn set_mailbox(&self, mailbox: WeakMailboxRef) {
        *self.mailbox.lock().unwrap() = mailbox;
    }

    /// Read the current weak mailbox reference.
    fn weak_mailbox(&self) -> WeakMailboxRef {
        self.mailbox.lock().unwrap().clone()
    }

    /// Upgrade the weak mailbox reference, mapping failure to an ActorError.
    fn mailbox(&self) -> ActorResult<Arc<dyn Mailbox + Send + Sync>> {
        self.weak_mailbox().upgrade().ok_or(ActorError::Stopped)
    }

    fn enqueue_error(
        &self,
        error: crate::thread::error::MailboxError,
        operation: &str,
    ) -> ActorError {
        match error {
            crate::thread::error::MailboxError::Closed => ActorError::Stopped,
            error => ActorError::InternalError(format!(
                "Failed to enqueue {operation} for {}: {error:?}",
                self.path
            )),
        }
    }

    /// Send a message with an explicit backpressure strategy (tell semantics).
    pub fn send_with_strategy(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> impl std::future::Future<Output = ActorResult<()>> + '_ {
        parrot_api::trace::capture(async move {
            let msg = crate::thread::envelope::TraceEnvelope::capture(msg);
            let mailbox = self.mailbox()?;
            mailbox
                .push(msg, strategy)
                .await
                .map_err(|error| self.enqueue_error(error, "message"))
        })
    }

    /// M2: Send with an explicit priority lane.
    ///
    /// `high_priority = true` routes the message into the mailbox's high
    /// lane (drained before normal messages) AND (when the mailbox needs
    /// re-scheduling) into the scheduler's High lane, giving O(1) jumps
    /// over backlogs. Use for user messages whose `MessagePriority >= 70`;
    /// system/death messages are routed here automatically by the engine.
    pub fn send_with_priority(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
        high_priority: bool,
    ) -> impl std::future::Future<Output = ActorResult<()>> + '_ {
        parrot_api::trace::capture(async move {
            let msg = crate::thread::envelope::TraceEnvelope::capture(msg);
            let mailbox = self.mailbox()?;
            mailbox
                .push_with_priority(msg, strategy, high_priority)
                .await
                .map_err(|error| self.enqueue_error(error, "message"))?;
            Ok(())
        })
    }

    /// Send a message with the default backpressure strategy (tell semantics).
    pub fn send_msg(
        &self,
        msg: BoxedMessage,
    ) -> impl std::future::Future<Output = ActorResult<()>> + '_ {
        parrot_api::trace::capture(async move {
            self.send_with_strategy(msg, self.default_strategy.clone())
                .await
        })
    }

    /// Ask with explicit strategy and timeout (request-response semantics).
    ///
    /// Wraps the message in an [`AskEnvelope`] carrying a oneshot reply
    /// channel. The actor processor completes the channel with the actor's
    /// response; the ask future resolves with it, or times out.
    ///
    /// M5: the envelope is pushed **by value** into the mailbox
    /// (`push_ask`) — no envelope boxing. Small payloads (`new_inline`)
    /// are additionally zero-payload-alloc.
    pub fn ask_with_strategy_and_timeout(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
        timeout_duration: Duration,
    ) -> impl std::future::Future<Output = ActorResult<BoxedMessage>> + '_ {
        parrot_api::trace::capture(async move {
            let (envelope, reply_rx) = AskEnvelope::with_boxed(msg);
            self.ask_envelope_with_timeout(envelope, reply_rx, strategy, timeout_duration)
                .await
        })
    }

    async fn ask_with_options(
        &self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> ActorResult<BoxedMessage> {
        options.validate_single_attempt()?;
        let mailbox = self.mailbox()?;
        if !mailbox.supports_high_priority() {
            options.validate_fifo()?;
        }
        let timeout = options.timeout;
        let (envelope, reply_rx) = AskEnvelope::with_boxed_options(msg, options);
        let exchange = async {
            mailbox
                .push_ask(envelope, self.default_strategy.clone())
                .await
                .map_err(|error| self.enqueue_error(error, "ask"))?;
            reply_rx.await.map_err(|_| {
                ActorError::ReplyChannelError(format!(
                    "Reply channel closed for ask to {}",
                    self.path
                ))
            })?
        };
        match timeout {
            Some(duration) => tokio::time::timeout(duration, exchange)
                .await
                .map_err(|_| {
                    ActorError::TimeoutDetail(format!(
                        "Request to actor {} timed out after {}ms",
                        self.path,
                        duration.as_millis()
                    ))
                })?,
            None => exchange.await,
        }
    }

    /// Ask with an inline payload, with one deadline for enqueue and reply.
    pub fn ask_inline<P: crate::thread::envelope::InlineMsg>(
        &self,
        msg: P,
        timeout_duration: Duration,
    ) -> impl std::future::Future<Output = ActorResult<BoxedMessage>> + '_ {
        parrot_api::trace::capture(async move {
            let (envelope, reply_rx) = AskEnvelope::new_inline(msg);
            self.ask_envelope_with_timeout(
                envelope,
                reply_rx,
                self.default_strategy.clone(),
                timeout_duration,
            )
            .await
        })
    }

    async fn ask_envelope_with_timeout(
        &self,
        envelope: AskEnvelope,
        reply_rx: tokio::sync::oneshot::Receiver<ActorResult<BoxedMessage>>,
        strategy: BackpressureStrategy,
        timeout_duration: Duration,
    ) -> ActorResult<BoxedMessage> {
        let exchange = async {
            let mailbox = self.mailbox()?;
            mailbox
                .push_ask(envelope, strategy)
                .await
                .map_err(|error| self.enqueue_error(error, "ask"))?;
            reply_rx.await.map_err(|_| {
                ActorError::ReplyChannelError(format!(
                    "Reply channel closed for ask to {}",
                    self.path
                ))
            })?
        };
        tokio::time::timeout(timeout_duration, exchange)
            .await
            .map_err(|_| {
                ActorError::TimeoutDetail(format!(
                    "Request to actor {} timed out after {}ms",
                    self.path,
                    timeout_duration.as_millis()
                ))
            })?
    }

    /// Ask with default strategy and timeout.
    pub fn ask(
        &self,
        msg: BoxedMessage,
    ) -> impl std::future::Future<Output = ActorResult<BoxedMessage>> + '_ {
        parrot_api::trace::capture(async move {
            self.ask_with_strategy_and_timeout(
                msg,
                self.default_strategy.clone(),
                self.default_timeout,
            )
            .await
        })
    }

    /// Unbounded ask: enqueue the AskEnvelope and await the reply channel
    /// with no timeout wrapper. The future resolves only when the actor
    /// processes the message (or its mailbox drops).
    ///
    /// This backs the unified `ActorRef::send` semantics.
    pub fn ask_unbounded(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> impl std::future::Future<Output = ActorResult<BoxedMessage>> + '_ {
        parrot_api::trace::capture(async move {
            let mailbox = self.mailbox()?;

            let (envelope, reply_rx) = AskEnvelope::with_boxed(msg);

            mailbox
                .push_ask(envelope, strategy)
                .await
                .map_err(|error| self.enqueue_error(error, "ask"))?;

            match reply_rx.await {
                Ok(result) => result,
                Err(_) => Err(ActorError::ReplyChannelError(format!(
                    "Reply channel closed for ask to {}",
                    self.path
                ))),
            }
        })
    }

    /// Ask with custom timeout (default strategy).
    pub fn ask_with_timeout(
        &self,
        msg: BoxedMessage,
        timeout_duration: Duration,
    ) -> impl std::future::Future<Output = ActorResult<BoxedMessage>> + '_ {
        parrot_api::trace::capture(async move {
            self.ask_with_strategy_and_timeout(msg, self.default_strategy.clone(), timeout_duration)
                .await
        })
    }
}

#[async_trait]
impl<A> ActorRef for ThreadActorRef<A>
where
    A: parrot_api::actor::Actor<Context = crate::thread::context::ThreadContext<A>>
        + Send
        + Sync
        + 'static,
{
    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        self.death_watch.clone()
    }

    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        // Unified semantics (stress report §9): `send` is an *unbounded*
        // ask. No implicit engine default timeout — callers who need a
        // bound use send_with_timeout(Some(d)); fire-and-forget uses
        // `deliver`.
        Box::pin(parrot_api::trace::capture(async move {
            self.ask_unbounded(msg, self.default_strategy.clone()).await
        }))
    }

    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        timeout_duration: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            match timeout_duration {
                Some(duration) => {
                    self.ask_with_strategy_and_timeout(msg, self.default_strategy.clone(), duration)
                        .await
                }
                None => {
                    // Aligned with `send`: unbounded ask.
                    self.ask_unbounded(msg, self.default_strategy.clone()).await
                }
            }
        }))
    }

    fn send_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.ask_with_options(msg, options).await
        }))
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.send_with_strategy(msg, self.default_strategy.clone())
                .await
        }))
    }

    fn deliver_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            let msg = crate::thread::envelope::TraceEnvelope::capture(msg);
            options.validate_single_attempt()?;
            let mailbox = self.mailbox()?;
            if !mailbox.supports_high_priority() {
                options.validate_fifo()?;
            }
            let delivery = async {
                mailbox
                    .push_with_priority(
                        msg,
                        self.default_strategy.clone(),
                        options.priority.value() >= 70,
                    )
                    .await
                    .map_err(|error| self.enqueue_error(error, "message"))?;
                Ok(())
            };
            match options.timeout {
                Some(duration) => tokio::time::timeout(duration, delivery)
                    .await
                    .map_err(|_| ActorError::Timeout)?,
                None => delivery.await,
            }
        }))
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.registered {
                // Registry-owned stop reservation publishes the receipt while
                // it still owns the registration. A reference must not reserve
                // the OnceCell first: that would prevent a concurrent system
                // stop from publishing before the registration disappears.
                let completion = if let Some(completion) = self.stop_request.get() {
                    completion.clone()
                } else {
                    let requested =
                        self.system
                            .upgrade()
                            .ok_or(ActorError::Stopped)
                            .and_then(|system| {
                                system
                                    .request_stop_ref(&self.path, &self.mailbox)
                                    .map_err(ActorError::from)
                            });
                    // The other stop can publish and remove the registration
                    // between our first cache read and the registry lookup.
                    self.stop_request.get().cloned().map_or(requested, Ok)?
                };
                return crate::thread::system::await_stop(completion)
                    .await
                    .map_err(ActorError::from);
            }
            // Explicitly constructed, unregistered adapters have no registry
            // operation to join; retain their mailbox-only close behavior.
            self.mailbox()?.close().await;
            Ok(())
        })
    }

    fn path(&self) -> String {
        self.path.clone()
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move {
            match self.weak_mailbox().upgrade() {
                Some(mailbox) => !mailbox.is_closed().await,
                None => false,
            }
        })
    }

    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl<A> Clone for ThreadActorRef<A>
where
    A: parrot_api::actor::Actor<Context = crate::thread::context::ThreadContext<A>>
        + Send
        + Sync
        + 'static,
{
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            mailbox: self.mailbox.clone(),
            system: self.system.clone(),
            death_watch: self.death_watch.clone(),
            registered: self.registered,
            stop_request: self.stop_request.clone(),
            default_strategy: self.default_strategy.clone(),
            default_timeout: self.default_timeout,
            scheduler: self.scheduler.clone(),
            _marker: PhantomData,
        }
    }
}

impl<A> std::fmt::Debug for ThreadActorRef<A>
where
    A: parrot_api::actor::Actor<Context = crate::thread::context::ThreadContext<A>>
        + Send
        + Sync
        + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadActorRef")
            .field("path", &self.path)
            .field("alive", &self.weak_mailbox().upgrade().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::context::ThreadContext;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::actor::EmptyConfig;

    fn create_test_mailbox() -> (Arc<MpscMailbox>, ActorPath) {
        #[derive(Debug)]
        struct MockActorRef;

        #[async_trait]
        impl ActorRef for MockActorRef {
            fn send<'a>(
                &'a self,
                _msg: BoxedMessage,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async { Ok(Box::new(()) as Box<dyn std::any::Any + Send>) })
            }

            fn send_with_timeout<'a>(
                &'a self,
                _msg: BoxedMessage,
                _timeout_duration: Option<Duration>,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async { Ok(Box::new(()) as Box<dyn std::any::Any + Send>) })
            }

            fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Ok(()) })
            }

            fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Ok(()) })
            }

            fn path(&self) -> String {
                "mock-actor".to_string()
            }

            fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
                Box::pin(async { true })
            }

            fn clone_boxed(&self) -> BoxedActorRef {
                Box::new(Self)
            }

            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let path = ActorPath {
            path: "test-actor".to_string(),
            target: Arc::new(MockActorRef) as parrot_api::types::WeakActorTarget,
        };
        let mailbox = Arc::new(MpscMailbox::new(10, path.clone()));
        (mailbox, path)
    }

    #[derive(Debug)]
    struct TestActor;

    impl parrot_api::actor::Actor for TestActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(
            &'a mut self,
            _msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Ok(Box::new(()) as Box<dyn Any + Send>) })
        }

        fn state(&self) -> parrot_api::actor::ActorState {
            parrot_api::actor::ActorState::Running
        }
    }

    #[tokio::test]
    async fn test_send_message() {
        let (mailbox, path) = create_test_mailbox();
        let actor_ref = ThreadActorRef::<TestActor>::new(
            path,
            Arc::downgrade(&mailbox) as WeakMailboxRef,
            BackpressureStrategy::Block,
            Duration::from_millis(100),
            None,
        );

        // Unified semantics (ADR-10): bare `send` is an *unbounded* ask —
        // with no consumer answering, it would hang forever. Tests must
        // bound it explicitly via send_with_timeout(Some(d)).
        let message = Box::new("Hello, actor!") as BoxedMessage;
        let result = actor_ref
            .send_with_timeout(message, Some(Duration::from_millis(100)))
            .await;
        assert!(result.is_err(), "unanswered bounded ask must time out");

        let received = mailbox.pop().await;
        assert!(received.is_some(), "envelope must be enqueued");
    }

    #[tokio::test]
    async fn test_send_unbounded_receives_reply_when_answered() {
        use crate::thread::envelope::AskEnvelope;

        let (mailbox, path) = create_test_mailbox();
        let actor_ref = ThreadActorRef::<TestActor>::new(
            path,
            Arc::downgrade(&mailbox) as WeakMailboxRef,
            BackpressureStrategy::Block,
            Duration::from_secs(5),
            None,
        );

        // A consumer answers the envelope: the unbounded send resolves.
        let answerer = tokio::spawn(async move {
            let env = mailbox.pop().await.expect("envelope queued");
            let envelope = *env.downcast::<AskEnvelope>().expect("is AskEnvelope");
            envelope
                .reply_success(Box::new("ack") as BoxedMessage)
                .await;
        });

        let result = actor_ref.send(Box::new("hi") as BoxedMessage).await;
        assert!(result.is_ok(), "answered unbounded send must resolve");
        answerer.await.unwrap();
    }

    #[tokio::test]
    async fn test_dead_reference() {
        let (mailbox, path) = create_test_mailbox();
        let actor_ref = ThreadActorRef::<TestActor>::new(
            path,
            Arc::downgrade(&mailbox) as WeakMailboxRef,
            BackpressureStrategy::Block,
            Duration::from_secs(1),
            None,
        );

        drop(mailbox);

        let message = Box::new("This should fail") as BoxedMessage;
        let result = actor_ref.send(message).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ask_times_out_when_no_processor() {
        let (mailbox, path) = create_test_mailbox();
        let actor_ref = ThreadActorRef::<TestActor>::new(
            path,
            Arc::downgrade(&mailbox) as WeakMailboxRef,
            BackpressureStrategy::Block,
            Duration::from_millis(50),
            None,
        );

        // No processor is attached, so the ask envelope is never answered.
        let message = Box::new("ping") as BoxedMessage;
        let result = actor_ref.ask(message).await;
        assert!(matches!(result, Err(ActorError::TimeoutDetail(_))));
    }

    #[tokio::test]
    async fn test_ask_receives_reply_from_envelope() {
        use crate::thread::envelope::AskEnvelope;

        let (mailbox, path) = create_test_mailbox();
        let actor_ref = ThreadActorRef::<TestActor>::new(
            path,
            Arc::downgrade(&mailbox) as WeakMailboxRef,
            BackpressureStrategy::Block,
            Duration::from_secs(1),
            None,
        );

        // Simulate a processor answering the envelope.
        let answerer = tokio::spawn(async move {
            let env = mailbox.pop().await.expect("envelope queued");
            let envelope = *env.downcast::<AskEnvelope>().expect("is AskEnvelope");
            envelope
                .reply_success(Box::new("pong") as BoxedMessage)
                .await;
        });

        let message = Box::new("ping") as BoxedMessage;
        let result = actor_ref.ask(message).await;
        assert!(result.is_ok());
        let payload = result.unwrap();
        assert_eq!(*payload.downcast::<&str>().unwrap(), "pong");

        answerer.await.unwrap();
    }

    #[tokio::test]
    async fn test_stop_closes_mailbox() {
        let (mailbox, path) = create_test_mailbox();
        let actor_ref = ThreadActorRef::<TestActor>::new(
            path,
            Arc::downgrade(&mailbox) as WeakMailboxRef,
            BackpressureStrategy::Block,
            Duration::from_secs(1),
            None,
        );

        actor_ref.stop().await.unwrap();
        assert!(mailbox.is_closed().await);
    }

    #[tokio::test]
    async fn test_set_mailbox_late_install() {
        let (mailbox, path) = create_test_mailbox();
        let actor_ref = ThreadActorRef::<TestActor>::new(
            path,
            Weak::<MpscMailbox>::new() as WeakMailboxRef,
            BackpressureStrategy::Block,
            // Short ask timeout: no consumer replies in this unit test.
            Duration::from_millis(50),
            None,
        );

        // Initially dead
        assert!(!actor_ref.is_alive().await);
        assert!(actor_ref.send(Box::new("x") as BoxedMessage).await.is_err());

        // Install the mailbox afterwards
        actor_ref.set_mailbox(Arc::downgrade(&mailbox) as WeakMailboxRef);
        assert!(actor_ref.is_alive().await);
        // Envelope is enqueued but unanswered: bounded ask times out.
        assert!(
            actor_ref
                .send_with_timeout(
                    Box::new("x") as BoxedMessage,
                    Some(Duration::from_millis(50))
                )
                .await
                .is_err()
        );
        assert!(mailbox.pop().await.is_some());
    }
}
