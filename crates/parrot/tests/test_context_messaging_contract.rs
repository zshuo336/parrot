//! Exercise ActorContext through real handlers, including cross-engine calls.
use actix::Actor as _;
use parrot::actix::system::{ActixActorSystem, ActixSystemSettings};
use parrot::actix::{ActixActor, ActixActorRef, ActixContext};
use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{ActixEngineExt, Actor, ActorState, EmptyConfig};
use parrot_api::address::{ActorRef, ActorRefExt};
use parrot_api::context::ActorContext;
use parrot_api::errors::ActorError;
use parrot_api::message::{Message, MessageOptions};
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

enum Command {
    SendSelf,
    Increment,
    Read,
    Ask(BoxedActorRef),
    Wait(Arc<Notify>),
}

struct TightDeadline;

impl Message for TightDeadline {
    type Result = u64;

    fn message_options(&self) -> Option<MessageOptions> {
        Some(MessageOptions {
            timeout: Some(Duration::from_millis(20)),
            ..MessageOptions::default()
        })
    }
}

async fn handle(
    value: &mut u64,
    msg: BoxedMessage,
    ctx: &mut dyn ActorContext,
) -> ActorResult<BoxedMessage> {
    if let Some(n) = msg.downcast_ref::<u64>() {
        return Ok(Box::new(n + 1));
    }
    if msg.is::<TightDeadline>() {
        tokio::time::sleep(Duration::from_millis(100)).await;
        return Ok(Box::new(7_u64));
    }
    match *msg
        .downcast::<Command>()
        .map_err(|_| ActorError::MessageHandlingError("unexpected envelope".into()))?
    {
        Command::SendSelf => {
            ctx.send(ctx.get_self_ref(), Box::new(Command::Increment))
                .await?
        }
        Command::Increment => *value += 1,
        Command::Read => {}
        Command::Ask(target) => return ctx.ask(target, Box::new(41_u64)).await,
        Command::Wait(entered) => {
            entered.notify_one();
            std::future::pending::<()>().await;
        }
    }
    Ok(Box::new(*value))
}

struct ThreadDriver(u64);
struct ActixDriver(u64);
impl ActixEngineExt for ActixDriver {}
macro_rules! driver {
    ($actor:ty, $context:ty) => {
        impl Actor for $actor {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(handle(&mut self.0, msg, ctx))
            }
        }
    };
}
driver!(ThreadDriver, ThreadContext<Self>);
driver!(ActixDriver, ActixContext<ActixActor<Self>>);

fn system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        default_ask_timeout: Duration::from_millis(50),
        shutdown_timeout: Duration::from_millis(50),
        ..Default::default()
    })
}

async fn send_self(actor: &dyn ActorRef) -> ActorResult<BoxedMessage> {
    actor
        .send_with_timeout(
            Box::new(Command::SendSelf),
            Some(Duration::from_millis(500)),
        )
        .await?;
    actor
        .send_with_timeout(Box::new(Command::Read), Some(Duration::from_millis(500)))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_context_send_to_self_does_not_wait_for_handler() {
    let system = system();
    let actor = system
        .spawn_at(ThreadDriver(0), "/self", None, Default::default())
        .await
        .unwrap();
    let result = send_self(&actor).await;
    let _ = system.shutdown_internal().await;
    assert_eq!(
        *result
            .expect("send must only enqueue")
            .downcast::<u64>()
            .unwrap(),
        1
    );
}

#[test]
fn actix_context_send_to_self_does_not_wait_for_handler() {
    actix::System::new().block_on(async {
        let actor = ActixActorRef::new(ActixActor::new(ActixDriver(0)).start(), "/self".into());
        let result = send_self(&actor).await;
        assert_eq!(
            *result
                .expect("send must only enqueue")
                .downcast::<u64>()
                .unwrap(),
            1
        );
        actor.stop().await.unwrap();
    });
}

#[test]
fn context_ask_crosses_engines_without_private_envelopes() {
    actix::System::new().block_on(async {
        let system = system();
        let thread = system
            .spawn_at(ThreadDriver(0), "/thread", None, Default::default())
            .await
            .unwrap();
        let actix = ActixActorRef::new(ActixActor::new(ActixDriver(0)).start(), "/actix".into());
        let forward = thread
            .send_with_timeout(
                Box::new(Command::Ask(actix.clone_boxed())),
                Some(Duration::from_secs(1)),
            )
            .await;
        let reverse = actix
            .send_with_timeout(
                Box::new(Command::Ask(Box::new(thread))),
                Some(Duration::from_secs(1)),
            )
            .await;
        system.shutdown_internal().await.unwrap();
        actix.stop().await.unwrap();
        assert_eq!(
            *forward
                .expect("thread context to Actix")
                .downcast::<u64>()
                .unwrap(),
            42
        );
        assert_eq!(
            *reverse
                .expect("Actix context to thread")
                .downcast::<u64>()
                .unwrap(),
            42
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_context_ask_applies_default_timeout_to_the_exchange() {
    let system = system();
    let caller = system
        .spawn_at(ThreadDriver(0), "/caller", None, Default::default())
        .await
        .unwrap();
    let target = system
        .spawn_at(ThreadDriver(0), "/target", None, Default::default())
        .await
        .unwrap();
    let entered = Arc::new(Notify::new());
    target
        .deliver(Box::new(Command::Wait(entered.clone())))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        caller.send(Box::new(Command::Ask(Box::new(target)))),
    )
    .await;
    let _ = system.shutdown_internal().await;
    assert!(
        matches!(result, Ok(Err(ActorError::TimeoutDetail(_)))),
        "context timeout must expire: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_typed_ask_preserves_message_timeout_options() {
    let system = system();
    let actor = system
        .spawn_at(ThreadDriver(0), "/typed-timeout", None, Default::default())
        .await
        .unwrap();
    let result = ActorRefExt::ask(&actor, TightDeadline).await;
    let _ = system.shutdown_internal().await;
    assert!(
        matches!(result, Err(ActorError::TimeoutDetail(_))),
        "typed message timeout must survive the ActorRefExt boundary: {result:?}"
    );
}

#[test]
fn actix_typed_ask_preserves_message_timeout_options() {
    actix::System::new().block_on(async {
        let actor = ActixActorRef::new(
            ActixActor::new(ActixDriver(0)).start(),
            "/typed-timeout".into(),
        );
        let result = actor.ask(TightDeadline).await;
        assert!(
            matches!(result, Err(ActorError::TimeoutDetail(_))),
            "Actix typed message timeout must survive the ActorRefExt boundary: {result:?}"
        );
        actor.stop().await.unwrap();
    });
}

#[test]
fn managed_actix_context_ask_applies_configured_default_timeout() {
    actix::System::new().block_on(async {
        let system = ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            ask_timeout: Duration::from_millis(50),
            shutdown_timeout: Duration::from_millis(500),
            ..Default::default()
        })
        .await
        .unwrap();
        let caller = system
            .spawn_root_typed(ActixDriver(0), EmptyConfig)
            .await
            .unwrap();
        let target = system
            .spawn_root_typed(ActixDriver(0), EmptyConfig)
            .await
            .unwrap();
        let entered = Arc::new(Notify::new());
        target
            .deliver(Box::new(Command::Wait(entered.clone())))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            caller.send(Box::new(Command::Ask(target.clone_boxed()))),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(ActorError::TimeoutDetail(_)))),
            "managed Actix context must enforce configured timeout: {result:?}"
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
        system.shutdown().await.unwrap();
    });
}

async fn full_mailbox_timeout(inline: bool) {
    let system = system();
    let actor = system
        .spawn_at(
            ThreadDriver(0),
            "/full",
            None,
            ThreadActorConfig {
                mailbox_capacity: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let entered = Arc::new(Notify::new());
    actor
        .deliver(Box::new(Command::Wait(entered.clone())))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    actor.deliver(Box::new(Command::Increment)).await.unwrap();
    let ask = async {
        if inline {
            actor.ask_inline(41_u64, Duration::from_millis(50)).await
        } else {
            actor
                .send_with_timeout(Box::new(41_u64), Some(Duration::from_millis(50)))
                .await
        }
    };
    let result = tokio::time::timeout(Duration::from_millis(500), ask).await;
    let _ = system.shutdown_internal().await;
    assert!(
        matches!(result, Ok(Err(ActorError::TimeoutDetail(_)))),
        "mailbox enqueue must share the ask deadline: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boxed_ask_timeout_includes_full_mailbox_wait() {
    full_mailbox_timeout(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_ask_timeout_includes_full_mailbox_wait() {
    full_mailbox_timeout(true).await;
}

#[test]
fn actix_ask_timeout_with_full_mailbox_preserves_timeout_error() {
    actix::System::new().block_on(async {
        let actor = ActixActorRef::new(ActixActor::new(ActixDriver(0)).start(), "/full".into());
        let entered = Arc::new(Notify::new());
        actor
            .deliver(Box::new(Command::Wait(entered.clone())))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        // Actix defaults to 16; do_send can accept beyond that limit.
        for _ in 0..32 {
            actor.deliver(Box::new(Command::Increment)).await.unwrap();
        }
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            actor.send_with_timeout(Box::new(41_u64), Some(Duration::from_millis(50))),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(ActorError::TimeoutDetail(_)))),
            "Actix must return the unified timeout error: {result:?}"
        );
    });
}

// Unpolled public context calls must retire each still-owned input independently.
mod context_input_cancellation {
    use super::*;
    use parrot_api::address::ActorPath;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Default)]
    struct Counts {
        messages: AtomicUsize,
        targets: AtomicUsize,
        operations: AtomicUsize,
        panic_payloads: AtomicUsize,
    }
    struct RecursivePanic(Arc<Counts>);
    impl Drop for RecursivePanic {
        fn drop(&mut self) {
            self.0.panic_payloads.fetch_add(1, Ordering::SeqCst);
            panic!("recursive context input panic payload");
        }
    }
    struct Payload(Arc<Counts>, bool);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.messages.fetch_add(1, Ordering::SeqCst);
            if self.1 {
                std::panic::panic_any(RecursivePanic(self.0.clone()));
            }
        }
    }
    #[derive(Debug)]
    struct Target(Arc<Counts>, bool);
    impl Drop for Target {
        fn drop(&mut self) {
            self.0.targets.fetch_add(1, Ordering::SeqCst);
            if self.1 {
                std::panic::panic_any(RecursivePanic(self.0.clone()));
            }
        }
    }
    impl ActorRef for Target {
        fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.0.operations.fetch_add(1, Ordering::SeqCst);
            panic!("unpolled context must not call a target");
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(msg)
        }
        fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            self.0.operations.fetch_add(1, Ordering::SeqCst);
            panic!("unpolled context must not call a target");
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "/context-input-target".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            panic!("context need not clone its owned target");
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    struct LocalProbe;
    impl actix::Actor for LocalProbe {
        type Context = actix::Context<Self>;
    }
    async fn cancel(context: &dyn ActorContext, ask: bool, hostile: bool) {
        let counts = Arc::new(Counts::default());
        let target = Box::new(Target(counts.clone(), hostile));
        let message = Box::new(Payload(counts.clone(), hostile));
        let cancelled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if ask {
                drop(context.ask(target, message));
            } else {
                drop(context.send(target, message));
            }
        }));
        assert!(cancelled.is_ok(), "unpolled context cancellation unwound");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if counts.messages.load(Ordering::SeqCst) == 1
                    && counts.targets.load(Ordering::SeqCst) == 1
                    && counts.panic_payloads.load(Ordering::SeqCst) == if hostile { 2 } else { 0 }
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled inputs did not both retire");
        assert_eq!(counts.operations.load(Ordering::SeqCst), 0);
    }
    fn isolated(case: &str) {
        const FLAG: &str = "PARROT_CONTEXT_INPUT_CANCELLATION";
        if std::env::var(FLAG).as_deref() == Ok(case) {
            actix::System::new().block_on(async {
                if case.starts_with("thread") {
                    let context = ThreadContext::<ThreadDriver>::new_for_test("/context-input");
                    cancel(&context, case.contains("ask"), case.ends_with("hostile")).await;
                } else {
                    let context = ActixContext::new(
                        LocalProbe.start(),
                        ActorPath::for_test("/context-input"),
                    );
                    cancel(&context, case.contains("ask"), case.ends_with("hostile")).await;
                }
            });
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("context_input_cancellation::{case}"),
                "--nocapture",
            ])
            .env(FLAG, case)
            .env("RUST_BACKTRACE", "0")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{case}: isolated input retirement failed: {status}"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{case}: input retirement exceeded deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    macro_rules! case {
        ($name:ident) => {
            #[test]
            fn $name() {
                isolated(stringify!($name));
            }
        };
    }
    case!(thread_send_hostile);
    case!(thread_ask_hostile);
    case!(actix_send_hostile);
    case!(actix_ask_hostile);
    case!(thread_send_healthy);
    case!(thread_ask_healthy);
    case!(actix_send_healthy);
    case!(actix_ask_healthy);
}

mod context_delegate_ownership {
    use super::*;
    use parrot_api::address::ActorPath;
    use std::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
    };

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Mode {
        Cancel,
        Poll,
        Construct,
        ReadyDrop,
        OriginalError,
        TargetDrop,
        Success,
    }
    #[derive(Debug, Default)]
    struct Counts {
        operations: AtomicUsize,
        polls: AtomicUsize,
        messages: AtomicUsize,
        futures: AtomicUsize,
        targets: AtomicUsize,
        replies: AtomicUsize,
        errors: AtomicUsize,
        formats: AtomicUsize,
        panic_payloads: AtomicUsize,
    }
    struct PanicPayload(Arc<Counts>);
    impl Drop for PanicPayload {
        fn drop(&mut self) {
            self.0.panic_payloads.fetch_add(1, Ordering::SeqCst);
            panic!("recursive delegated panic payload");
        }
    }
    fn hostile(counts: &Arc<Counts>) -> ! {
        std::panic::panic_any(PanicPayload(counts.clone()));
    }
    struct Input(Arc<Counts>);
    impl Drop for Input {
        fn drop(&mut self) {
            self.0.messages.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Reply(Arc<Counts>, bool);
    impl Drop for Reply {
        fn drop(&mut self) {
            self.0.replies.fetch_add(1, Ordering::SeqCst);
            if self.1 {
                hostile(&self.0);
            }
        }
    }
    #[derive(Debug)]
    struct OriginalError(Arc<Counts>);
    impl std::fmt::Display for OriginalError {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            panic!("error identity must not be replaced by formatting");
        }
    }
    impl std::error::Error for OriginalError {}
    impl Drop for OriginalError {
        fn drop(&mut self) {
            self.0.errors.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[derive(Debug)]
    struct Target {
        counts: Arc<Counts>,
        mode: Mode,
    }
    impl Drop for Target {
        fn drop(&mut self) {
            self.counts.targets.fetch_add(1, Ordering::SeqCst);
            if self.mode != Mode::Success {
                hostile(&self.counts);
            }
        }
    }
    struct Delegate<'a, T> {
        target: &'a Target,
        input: Option<BoxedMessage>,
        output: fn(&Target) -> T,
    }
    impl<T> Future for Delegate<'_, T> {
        type Output = ActorResult<T>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            let target = self.target;
            target.counts.polls.fetch_add(1, Ordering::SeqCst);
            match target.mode {
                Mode::Cancel => Poll::Pending,
                Mode::Poll => hostile(&target.counts),
                Mode::OriginalError => Poll::Ready(Err(ActorError::Other(anyhow::Error::new(
                    OriginalError(target.counts.clone()),
                )))),
                _ => Poll::Ready(Ok((self.output)(target))),
            }
        }
    }
    impl<T> Drop for Delegate<'_, T> {
        fn drop(&mut self) {
            drop(self.input.take());
            self.target.counts.futures.fetch_add(1, Ordering::SeqCst);
            if matches!(
                self.target.mode,
                Mode::Cancel | Mode::Poll | Mode::ReadyDrop | Mode::OriginalError
            ) {
                hostile(&self.target.counts);
            }
        }
    }
    impl Target {
        fn invoke<T: Send + 'static>(
            &self,
            input: BoxedMessage,
            output: fn(&Target) -> T,
        ) -> BoxedFuture<'_, ActorResult<T>> {
            self.counts.operations.fetch_add(1, Ordering::SeqCst);
            assert!(
                input.downcast_ref::<Input>().is_some(),
                "context changed message framing"
            );
            if self.mode == Mode::Construct {
                drop(input);
                hostile(&self.counts);
            }
            Box::pin(Delegate {
                target: self,
                input: Some(input),
                output,
            })
        }
    }
    impl ActorRef for Target {
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.invoke(msg, |t| {
                Box::new(Reply(
                    t.counts.clone(),
                    matches!(t.mode, Mode::ReadyDrop | Mode::TargetDrop),
                )) as BoxedMessage
            })
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            timeout: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            assert_eq!(
                timeout,
                Some(Duration::from_secs(5)),
                "context changed ask timeout forwarding"
            );
            self.send(msg)
        }
        fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            self.invoke(msg, |_| ())
        }
        fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            panic!("context must not call target metadata")
        }
        fn is_alive(&self) -> BoxedFuture<'_, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            panic!("context must not clone target")
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    struct LocalProbe;
    impl actix::Actor for LocalProbe {
        type Context = actix::Context<Self>;
    }
    fn discard_panic(payload: Box<dyn std::any::Any + Send>) {
        if let Err(secondary) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
        {
            std::mem::forget(secondary);
        }
    }
    fn exercise(context: &dyn ActorContext, ask: bool, mode: Mode) {
        let counts = Arc::new(Counts::default());
        let target = Box::new(Target {
            counts: counts.clone(),
            mode,
        });
        let input = Box::new(Input(counts.clone()));
        let mut call: BoxedFuture<'_, ActorResult<BoxedMessage>> = if ask {
            context.ask(target, input)
        } else {
            let call = context.send(target, input);
            Box::pin(async { call.await.map(|()| Box::new(()) as BoxedMessage) })
        };
        let waker = futures::task::noop_waker();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            call.as_mut().poll(&mut Context::from_waker(&waker))
        }));
        let escaped = outcome.is_err();
        let result = match outcome {
            Ok(poll) => Some(poll),
            Err(payload) => {
                discard_panic(payload);
                None
            }
        };
        let disposal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(call)));
        let drop_escaped = disposal.is_err();
        if let Err(payload) = disposal {
            discard_panic(payload);
        }
        assert!(
            !escaped && !drop_escaped,
            "context delegated poll or cancellation escaped its boundary"
        );
        match (mode, result.unwrap()) {
            (Mode::Cancel, Poll::Pending) => {}
            (Mode::OriginalError, Poll::Ready(Err(ActorError::Other(error)))) => {
                let original = error
                    .downcast_ref::<OriginalError>()
                    .expect("original error type lost");
                assert!(Arc::ptr_eq(&original.0, &counts));
                drop(error);
            }
            (Mode::Success, Poll::Ready(Ok(reply))) => {
                if ask {
                    assert!(reply.is::<Reply>(), "reply framing changed");
                }
                assert_eq!(
                    counts.targets.load(Ordering::SeqCst),
                    1,
                    "target must retire before result publication"
                );
                drop(reply);
            }
            (
                Mode::Construct | Mode::Poll | Mode::ReadyDrop | Mode::TargetDrop,
                Poll::Ready(Err(ActorError::Panic(message))),
            ) => {
                let phase = match mode {
                    Mode::Construct => "construction",
                    Mode::Poll => "poll",
                    Mode::ReadyDrop => "future disposal",
                    _ => "target disposal",
                };
                assert!(message.contains(phase), "wrong first failure: {message}");
            }
            _ => panic!("unexpected context outcome for {mode:?}"),
        }
        assert_eq!(counts.operations.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.polls.load(Ordering::SeqCst),
            usize::from(mode != Mode::Construct)
        );
        assert_eq!(counts.messages.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.futures.load(Ordering::SeqCst),
            usize::from(mode != Mode::Construct)
        );
        assert_eq!(counts.targets.load(Ordering::SeqCst), 1);
        let has_reply = ask && matches!(mode, Mode::ReadyDrop | Mode::TargetDrop | Mode::Success);
        assert_eq!(
            counts.replies.load(Ordering::SeqCst),
            usize::from(has_reply)
        );
        assert_eq!(
            counts.errors.load(Ordering::SeqCst),
            usize::from(mode == Mode::OriginalError)
        );
        assert_eq!(counts.formats.load(Ordering::SeqCst), 0);
        let panics = match mode {
            Mode::Success => 0,
            Mode::Poll => 3,
            Mode::TargetDrop => 1 + usize::from(ask),
            Mode::ReadyDrop => 2 + usize::from(ask),
            _ => 2,
        };
        assert_eq!(counts.panic_payloads.load(Ordering::SeqCst), panics);
    }
    fn isolated(case: &str, ask: bool, mode: Mode) {
        const FLAG: &str = "PARROT_CONTEXT_DELEGATE_OWNERSHIP";
        if std::env::var(FLAG).as_deref() == Ok(case) {
            actix::System::new().block_on(async {
                if case.starts_with("thread") {
                    exercise(
                        &ThreadContext::<ThreadDriver>::new_for_test("/delegate"),
                        ask,
                        mode,
                    );
                } else {
                    exercise(
                        &ActixContext::new(LocalProbe.start(), ActorPath::for_test("/delegate")),
                        ask,
                        mode,
                    );
                }
            });
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("context_delegate_ownership::{case}"),
                "--nocapture",
            ])
            .env(FLAG, case)
            .env("RUST_BACKTRACE", "0")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{case}: isolated delegate contract failed: {status}"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{case}: delegate deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    macro_rules! case {
        ($name:ident, $ask:literal, $mode:ident) => {
            #[test]
            fn $name() {
                isolated(stringify!($name), $ask, Mode::$mode);
            }
        };
    }
    case!(thread_send_cancel, false, Cancel);
    case!(thread_send_poll, false, Poll);
    case!(thread_send_construct, false, Construct);
    case!(thread_send_ready_drop, false, ReadyDrop);
    case!(thread_send_original_error, false, OriginalError);
    case!(thread_send_target_drop, false, TargetDrop);
    case!(thread_send_success, false, Success);
    case!(thread_ask_cancel, true, Cancel);
    case!(thread_ask_poll, true, Poll);
    case!(thread_ask_construct, true, Construct);
    case!(thread_ask_ready_drop, true, ReadyDrop);
    case!(thread_ask_original_error, true, OriginalError);
    case!(thread_ask_target_drop, true, TargetDrop);
    case!(thread_ask_success, true, Success);
    case!(actix_send_cancel, false, Cancel);
    case!(actix_send_poll, false, Poll);
    case!(actix_send_construct, false, Construct);
    case!(actix_send_ready_drop, false, ReadyDrop);
    case!(actix_send_original_error, false, OriginalError);
    case!(actix_send_target_drop, false, TargetDrop);
    case!(actix_send_success, false, Success);
    case!(actix_ask_cancel, true, Cancel);
    case!(actix_ask_poll, true, Poll);
    case!(actix_ask_construct, true, Construct);
    case!(actix_ask_ready_drop, true, ReadyDrop);
    case!(actix_ask_original_error, true, OriginalError);
    case!(actix_ask_target_drop, true, TargetDrop);
    case!(actix_ask_success, true, Success);
}

mod context_cancellation_receipt {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
    };
    #[derive(Debug)]
    struct Target {
        entered: Arc<Notify>,
        drops: Arc<AtomicUsize>,
        hostile: bool,
    }
    struct Pending<'a, T> {
        target: &'a Target,
        _output: std::marker::PhantomData<fn() -> T>,
    }
    impl<T> Future for Pending<'_, T> {
        type Output = ActorResult<T>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.target.entered.notify_one();
            Poll::Pending
        }
    }
    impl<T> Drop for Pending<'_, T> {
        fn drop(&mut self) {
            self.target.drops.fetch_add(1, Ordering::SeqCst);
            if self.target.hostile {
                panic!("cancelled delegated future cleanup failed");
            }
        }
    }
    impl ActorRef for Target {
        fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(Pending {
                target: self,
                _output: std::marker::PhantomData,
            })
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(msg)
        }
        fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(Pending {
                target: self,
                _output: std::marker::PhantomData,
            })
        }
        fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "/receipt-probe".into()
        }
        fn is_alive(&self) -> BoxedFuture<'_, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            panic!("no target clone needed")
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    struct ThreadCaller {
        ask: bool,
        entered: Arc<Notify>,
        drops: Arc<AtomicUsize>,
        hostile: bool,
    }
    struct ActixCaller {
        ask: bool,
        entered: Arc<Notify>,
        drops: Arc<AtomicUsize>,
        hostile: bool,
    }
    impl ActixEngineExt for ActixCaller {}
    macro_rules! caller {
        ($actor:ty, $context:ty, $cancel:literal) => {
            impl Actor for $actor {
                type Config = EmptyConfig;
                type Context = $context;
                fn state(&self) -> ActorState {
                    ActorState::Running
                }
                fn use_async_handler(&self) -> bool {
                    true
                }
                fn receive_message<'a>(
                    &'a mut self,
                    _: BoxedMessage,
                    ctx: &'a mut Self::Context,
                ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                    Box::pin(async move {
                        let target = Box::new(Target {
                            entered: self.entered.clone(),
                            drops: self.drops.clone(),
                            hostile: self.hostile,
                        });
                        let call: BoxedFuture<'_, ActorResult<BoxedMessage>> = if self.ask {
                            ctx.ask(target, Box::new(()))
                        } else {
                            let call = ctx.send(target, Box::new(()));
                            Box::pin(async { call.await.map(|()| Box::new(()) as BoxedMessage) })
                        };
                        if $cancel {
                            let mut call = call;
                            assert!(futures::poll!(&mut call).is_pending());
                            drop(call);
                            Ok(Box::new(()) as BoxedMessage)
                        } else {
                            call.await
                        }
                    })
                }
            }
        };
    }
    caller!(ThreadCaller, ThreadContext<Self>, true);
    caller!(ActixCaller, ActixContext<ActixActor<Self>>, false);
    async fn exercise(
        actor: &dyn ActorRef,
        entered: &Notify,
        drops: &AtomicUsize,
        completed_handler: bool,
        hostile: bool,
    ) {
        if completed_handler {
            actor
                .send_with_timeout(Box::new(()), Some(Duration::from_secs(3)))
                .await
                .expect("handler failed instead of completing cancellation");
        } else {
            actor.deliver(Box::new(())).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .expect("delegate was not polled");
        let stopped = tokio::time::timeout(Duration::from_secs(4), actor.stop())
            .await
            .expect("stop lost progress");
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "stop returned before delegate disposal"
        );
        if hostile {
            let error = stopped.expect_err("stop masked the delegated cleanup failure");
            assert!(
                error
                    .to_string()
                    .contains("cancelled delegated future cleanup failed"),
                "wrong stop error: {error}"
            );
        } else {
            stopped.expect("healthy cancellation poisoned the generation");
        }
    }
    async fn thread(ask: bool, hostile: bool) {
        let entered = Arc::new(Notify::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            shutdown_timeout: Duration::from_secs(2),
            ..Default::default()
        });
        let actor = system
            .spawn_at(
                ThreadCaller {
                    ask,
                    entered: entered.clone(),
                    drops: drops.clone(),
                    hostile,
                },
                "/context-receipt",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let result =
            std::panic::AssertUnwindSafe(exercise(&actor, &entered, &drops, true, hostile));
        use futures::FutureExt;
        let result = result.catch_unwind().await;
        let _ = system.shutdown_internal().await;
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
    fn actix(ask: bool, hostile: bool) {
        actix::System::new().block_on(async {
            let entered = Arc::new(Notify::new());
            let drops = Arc::new(AtomicUsize::new(0));
            let system = ActixActorSystem::with_settings(ActixSystemSettings {
                arbiter_count: 1,
                shutdown_timeout: Duration::from_secs(2),
                ..Default::default()
            })
            .await
            .unwrap();
            let actor = system
                .spawn_root_typed(
                    ActixCaller {
                        ask,
                        entered: entered.clone(),
                        drops: drops.clone(),
                        hostile,
                    },
                    EmptyConfig,
                )
                .await
                .unwrap();
            use futures::FutureExt;
            let result = std::panic::AssertUnwindSafe(exercise(
                actor.as_ref(),
                &entered,
                &drops,
                false,
                hostile,
            ))
            .catch_unwind()
            .await;
            let _ = system.shutdown().await;
            if let Err(payload) = result {
                std::panic::resume_unwind(payload);
            }
        });
    }
    fn isolated(case: &str, is_actix: bool, ask: bool, hostile: bool) {
        const FLAG: &str = "PARROT_CONTEXT_CANCELLATION_RECEIPT";
        if std::env::var(FLAG).as_deref() == Ok(case) {
            if is_actix {
                actix(ask, hostile);
            } else {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(thread(ask, hostile));
            }
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("context_cancellation_receipt::{case}"),
                "--nocapture",
            ])
            .env(FLAG, case)
            .env("RUST_BACKTRACE", "0")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{case}: managed cancellation receipt failed: {status}"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{case}: cancellation receipt deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    macro_rules! case {
        ($name:ident, $actix:literal, $ask:literal, $hostile:literal) => {
            #[test]
            fn $name() {
                isolated(stringify!($name), $actix, $ask, $hostile);
            }
        };
    }
    case!(
        thread_send_cancel_reports_cleanup_failure,
        false,
        false,
        true
    );
    case!(
        thread_send_healthy_cancellation_stops_cleanly,
        false,
        false,
        false
    );
    case!(thread_ask_cancel_reports_cleanup_failure, false, true, true);
    case!(
        thread_ask_healthy_cancellation_stops_cleanly,
        false,
        true,
        false
    );
    case!(actix_send_cancel_reports_cleanup_failure, true, false, true);
    case!(
        actix_send_healthy_cancellation_stops_cleanly,
        true,
        false,
        false
    );
    case!(actix_ask_cancel_reports_cleanup_failure, true, true, true);
    case!(
        actix_ask_healthy_cancellation_stops_cleanly,
        true,
        true,
        false
    );
}

mod context_watch_ownership {
    use super::*;
    use parrot_api::address::ActorPath;
    use std::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
    };

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Mode {
        Unpolled,
        UnpolledHealthy,
        Cancel,
        Poll,
        Construct,
        ReadyDrop,
        OriginalError,
        TargetDrop,
        Success,
    }
    #[derive(Debug, Default)]
    struct Counts {
        operations: AtomicUsize,
        polls: AtomicUsize,
        watchers: AtomicUsize,
        futures: AtomicUsize,
        targets: AtomicUsize,
        errors: AtomicUsize,
        formats: AtomicUsize,
        payloads: AtomicUsize,
    }
    struct Payload(Arc<Counts>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.payloads.fetch_add(1, Ordering::SeqCst);
            panic!("recursive watch panic payload");
        }
    }
    fn hostile(c: &Arc<Counts>) -> ! {
        std::panic::panic_any(Payload(c.clone()))
    }
    #[derive(Debug)]
    struct OriginalError(Arc<Counts>);
    impl std::fmt::Display for OriginalError {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            panic!("must preserve original error identity");
        }
    }
    impl std::error::Error for OriginalError {}
    impl Drop for OriginalError {
        fn drop(&mut self) {
            self.0.errors.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[derive(Debug)]
    struct Watcher {
        counts: Arc<Counts>,
        mode: Mode,
        owned: bool,
    }
    impl Drop for Watcher {
        fn drop(&mut self) {
            if self.owned {
                self.counts.watchers.fetch_add(1, Ordering::SeqCst);
                if self.mode == Mode::Unpolled {
                    hostile(&self.counts);
                }
            }
        }
    }
    impl ActorRef for Watcher {
        fn send_with_timeout<'a>(
            &'a self,
            _: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            panic!("must not send")
        }
        fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            panic!("must not deliver")
        }

        fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            panic!("watcher send")
        }
        fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
            panic!("watcher stop")
        }
        fn path(&self) -> String {
            "/watch-self".into()
        }
        fn is_alive(&self) -> BoxedFuture<'_, bool> {
            panic!("watcher alive")
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(Self {
                counts: self.counts.clone(),
                mode: self.mode,
                owned: true,
            })
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    #[derive(Debug)]
    struct Target {
        counts: Arc<Counts>,
        mode: Mode,
        remove: bool,
    }
    impl Drop for Target {
        fn drop(&mut self) {
            self.counts.targets.fetch_add(1, Ordering::SeqCst);
            if !matches!(self.mode, Mode::Success | Mode::UnpolledHealthy) {
                hostile(&self.counts);
            }
        }
    }
    struct Delegate<'a> {
        target: &'a Target,
        watcher: Option<BoxedActorRef>,
    }
    impl Future for Delegate<'_> {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            let t = self.target;
            t.counts.polls.fetch_add(1, Ordering::SeqCst);
            match t.mode {
                Mode::Cancel => Poll::Pending,
                Mode::Poll => hostile(&t.counts),
                Mode::OriginalError => Poll::Ready(Err(ActorError::Other(anyhow::Error::new(
                    OriginalError(t.counts.clone()),
                )))),
                _ => Poll::Ready(Ok(())),
            }
        }
    }
    impl Drop for Delegate<'_> {
        fn drop(&mut self) {
            drop(self.watcher.take());
            self.target.counts.futures.fetch_add(1, Ordering::SeqCst);
            if matches!(
                self.target.mode,
                Mode::Cancel | Mode::Poll | Mode::ReadyDrop | Mode::OriginalError
            ) {
                hostile(&self.target.counts);
            }
        }
    }
    impl Target {
        fn invoke(&self, watcher: BoxedActorRef, remove: bool) -> BoxedFuture<'_, ActorResult<()>> {
            self.counts.operations.fetch_add(1, Ordering::SeqCst);
            assert_eq!(remove, self.remove, "watch and unwatch routing changed");
            let identity = watcher
                .as_any()
                .downcast_ref::<Watcher>()
                .expect("self reference replaced");
            assert!(Arc::ptr_eq(&identity.counts, &self.counts));
            if self.mode == Mode::Construct {
                drop(watcher);
                hostile(&self.counts);
            }
            Box::pin(Delegate {
                target: self,
                watcher: Some(watcher),
            })
        }
    }
    impl ActorRef for Target {
        fn send_with_timeout<'a>(
            &'a self,
            _: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            panic!("must not send")
        }
        fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            panic!("must not deliver")
        }

        fn watch<'a>(&'a self, watcher: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
            self.invoke(watcher, false)
        }
        fn unwatch<'a>(&'a self, watcher: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
            self.invoke(watcher, true)
        }
        fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            panic!("must use watch delegate")
        }
        fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
            panic!("must not stop target")
        }
        fn path(&self) -> String {
            panic!("must not query target metadata")
        }
        fn is_alive(&self) -> BoxedFuture<'_, bool> {
            panic!("must not query target metadata")
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            panic!("must not clone owned target")
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    struct LocalProbe;
    impl actix::Actor for LocalProbe {
        type Context = actix::Context<Self>;
    }
    fn discard(payload: Box<dyn std::any::Any + Send>) {
        if let Err(second) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
        {
            std::mem::forget(second);
        }
    }
    async fn exercise(
        context: &mut dyn ActorContext,
        counts: Arc<Counts>,
        remove: bool,
        mode: Mode,
    ) {
        let target = Box::new(Target {
            counts: counts.clone(),
            mode,
            remove,
        });
        let mut call = if remove {
            context.unwatch(target)
        } else {
            context.watch(target)
        };
        assert_eq!(
            counts.operations.load(Ordering::SeqCst),
            0,
            "delegate must remain lazy"
        );
        let unpolled = matches!(mode, Mode::Unpolled | Mode::UnpolledHealthy);
        let result = if unpolled {
            None
        } else {
            let waker = futures::task::noop_waker();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                call.as_mut().poll(&mut Context::from_waker(&waker))
            })) {
                Ok(result) => Some(result),
                Err(payload) => {
                    discard(payload);
                    None
                }
            }
        };
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(call)));
        let escaped = dropped.is_err();
        if let Err(payload) = dropped {
            discard(payload);
        }
        assert!(!escaped, "context watch cancellation escaped");
        match (mode, result) {
            (Mode::Unpolled | Mode::UnpolledHealthy, None)
            | (Mode::Cancel, Some(Poll::Pending)) => {}
            (Mode::Success, Some(Poll::Ready(Ok(())))) => {}
            (Mode::OriginalError, Some(Poll::Ready(Err(ActorError::Other(error))))) => {
                assert!(Arc::ptr_eq(
                    &error
                        .downcast_ref::<OriginalError>()
                        .expect("original error type")
                        .0,
                    &counts
                ));
                drop(error);
            }
            (
                Mode::Construct | Mode::Poll | Mode::ReadyDrop | Mode::TargetDrop,
                Some(Poll::Ready(Err(ActorError::Panic(message)))),
            ) => {
                let phase = match mode {
                    Mode::Construct => "construction",
                    Mode::Poll => "poll",
                    Mode::ReadyDrop => "future disposal",
                    _ => "target disposal",
                };
                assert!(message.contains(phase), "wrong first error: {message}");
            }
            _ => panic!("unexpected outcome for {mode:?}"),
        }
        let panics = match mode {
            Mode::Success | Mode::UnpolledHealthy => 0,
            Mode::TargetDrop => 1,
            Mode::Poll => 3,
            _ => 2,
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while counts.targets.load(Ordering::SeqCst) != 1
                || counts.watchers.load(Ordering::SeqCst) != 1
                || counts.payloads.load(Ordering::SeqCst) != panics
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both inputs and panic payloads must retire exactly once");
        assert_eq!(
            counts.operations.load(Ordering::SeqCst),
            usize::from(!unpolled)
        );
        assert_eq!(
            counts.polls.load(Ordering::SeqCst),
            usize::from(!unpolled && mode != Mode::Construct)
        );
        assert_eq!(
            counts.futures.load(Ordering::SeqCst),
            usize::from(!unpolled && mode != Mode::Construct)
        );
        assert_eq!(
            counts.errors.load(Ordering::SeqCst),
            usize::from(mode == Mode::OriginalError)
        );
        assert_eq!(counts.formats.load(Ordering::SeqCst), 0);
    }
    fn isolated(case: &str, remove: bool, mode: Mode) {
        const FLAG: &str = "PARROT_CONTEXT_WATCH_OWNERSHIP";
        if std::env::var(FLAG).as_deref() == Ok(case) {
            actix::System::new().block_on(async {
                let counts = Arc::new(Counts::default());
                let watcher = Box::new(Watcher {
                    counts: counts.clone(),
                    mode,
                    owned: false,
                });
                if case.starts_with("thread") {
                    let mut context = ThreadContext::<ThreadDriver>::new_for_test("/watch-self");
                    context.set_self_ref(watcher);
                    exercise(&mut context, counts, remove, mode).await;
                } else {
                    let mut path = ActorPath::for_test("/watch-self");
                    path.target = Arc::from(watcher as BoxedActorRef);
                    let mut context = ActixContext::new(LocalProbe.start(), path);
                    exercise(&mut context, counts, remove, mode).await;
                }
            });
            return;
        }
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                }
                let _ = self.0.wait();
            }
        }
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("context_watch_ownership::{case}"),
                    "--nocapture",
                ])
                .env(FLAG, case)
                .env("RUST_BACKTRACE", "0")
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success(), "{case}: {status}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "watch subprocess deadline: {case}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    macro_rules! case {
        ($name:ident, $remove:literal, $mode:ident) => {
            #[test]
            fn $name() {
                isolated(stringify!($name), $remove, Mode::$mode);
            }
        };
    }
    case!(thread_watch_unpolled, false, Unpolled);
    case!(thread_watch_unpolled_healthy, false, UnpolledHealthy);
    case!(thread_watch_cancel, false, Cancel);
    case!(thread_watch_poll, false, Poll);
    case!(thread_watch_construct, false, Construct);
    case!(thread_watch_ready_drop, false, ReadyDrop);
    case!(thread_watch_original_error, false, OriginalError);
    case!(thread_watch_target_drop, false, TargetDrop);
    case!(thread_watch_success, false, Success);
    case!(thread_unwatch_unpolled, true, Unpolled);
    case!(thread_unwatch_unpolled_healthy, true, UnpolledHealthy);
    case!(thread_unwatch_cancel, true, Cancel);
    case!(thread_unwatch_poll, true, Poll);
    case!(thread_unwatch_construct, true, Construct);
    case!(thread_unwatch_ready_drop, true, ReadyDrop);
    case!(thread_unwatch_original_error, true, OriginalError);
    case!(thread_unwatch_target_drop, true, TargetDrop);
    case!(thread_unwatch_success, true, Success);
    case!(actix_watch_unpolled, false, Unpolled);
    case!(actix_watch_unpolled_healthy, false, UnpolledHealthy);
    case!(actix_watch_cancel, false, Cancel);
    case!(actix_watch_poll, false, Poll);
    case!(actix_watch_construct, false, Construct);
    case!(actix_watch_ready_drop, false, ReadyDrop);
    case!(actix_watch_original_error, false, OriginalError);
    case!(actix_watch_target_drop, false, TargetDrop);
    case!(actix_watch_success, false, Success);
    case!(actix_unwatch_unpolled, true, Unpolled);
    case!(actix_unwatch_unpolled_healthy, true, UnpolledHealthy);
    case!(actix_unwatch_cancel, true, Cancel);
    case!(actix_unwatch_poll, true, Poll);
    case!(actix_unwatch_construct, true, Construct);
    case!(actix_unwatch_ready_drop, true, ReadyDrop);
    case!(actix_unwatch_original_error, true, OriginalError);
    case!(actix_unwatch_target_drop, true, TargetDrop);
    case!(actix_unwatch_success, true, Success);
}

mod context_watch_receipt {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
    };
    #[derive(Debug)]
    struct Target {
        entered: Arc<Notify>,
        drops: Arc<AtomicUsize>,
        hostile: bool,
    }
    struct Pending<'a, T> {
        target: &'a Target,
        _output: std::marker::PhantomData<fn() -> T>,
    }
    impl<T> Future for Pending<'_, T> {
        type Output = ActorResult<T>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.target.entered.notify_one();
            Poll::Pending
        }
    }
    impl<T> Drop for Pending<'_, T> {
        fn drop(&mut self) {
            self.target.drops.fetch_add(1, Ordering::SeqCst);
            if self.target.hostile {
                panic!("cancelled delegated future cleanup failed");
            }
        }
    }
    impl ActorRef for Target {
        fn watch<'a>(&'a self, _: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
            self.deliver(Box::new(()))
        }
        fn unwatch<'a>(&'a self, _: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
            self.deliver(Box::new(()))
        }

        fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(Pending {
                target: self,
                _output: std::marker::PhantomData,
            })
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(msg)
        }
        fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(Pending {
                target: self,
                _output: std::marker::PhantomData,
            })
        }
        fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "/receipt-probe".into()
        }
        fn is_alive(&self) -> BoxedFuture<'_, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            panic!("no target clone needed")
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    struct ThreadCaller {
        ask: bool,
        entered: Arc<Notify>,
        drops: Arc<AtomicUsize>,
        hostile: bool,
    }
    struct ActixCaller {
        ask: bool,
        entered: Arc<Notify>,
        drops: Arc<AtomicUsize>,
        hostile: bool,
    }
    impl ActixEngineExt for ActixCaller {}
    macro_rules! caller {
        ($actor:ty, $context:ty, $cancel:literal) => {
            impl Actor for $actor {
                type Config = EmptyConfig;
                type Context = $context;
                fn state(&self) -> ActorState {
                    ActorState::Running
                }
                fn use_async_handler(&self) -> bool {
                    true
                }
                fn receive_message<'a>(
                    &'a mut self,
                    _: BoxedMessage,
                    ctx: &'a mut Self::Context,
                ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                    Box::pin(async move {
                        let target = Box::new(Target {
                            entered: self.entered.clone(),
                            drops: self.drops.clone(),
                            hostile: self.hostile,
                        });
                        let call = if self.ask {
                            ctx.unwatch(target)
                        } else {
                            ctx.watch(target)
                        };
                        let call: BoxedFuture<'_, ActorResult<BoxedMessage>> =
                            Box::pin(async { call.await.map(|()| Box::new(()) as BoxedMessage) });
                        if $cancel {
                            let mut call = call;
                            assert!(futures::poll!(&mut call).is_pending());
                            drop(call);
                            Ok(Box::new(()) as BoxedMessage)
                        } else {
                            call.await
                        }
                    })
                }
            }
        };
    }
    caller!(ThreadCaller, ThreadContext<Self>, true);
    caller!(ActixCaller, ActixContext<ActixActor<Self>>, false);
    async fn exercise(
        actor: &dyn ActorRef,
        entered: &Notify,
        drops: &AtomicUsize,
        completed_handler: bool,
        hostile: bool,
    ) {
        if completed_handler {
            actor
                .send_with_timeout(Box::new(()), Some(Duration::from_secs(3)))
                .await
                .expect("handler failed instead of completing cancellation");
        } else {
            actor.deliver(Box::new(())).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .expect("delegate was not polled");
        let stopped = tokio::time::timeout(Duration::from_secs(4), actor.stop())
            .await
            .expect("stop lost progress");
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "stop returned before delegate disposal"
        );
        if hostile {
            let error = stopped.expect_err("stop masked the delegated cleanup failure");
            assert!(
                error
                    .to_string()
                    .contains("cancelled delegated future cleanup failed"),
                "wrong stop error: {error}"
            );
        } else {
            stopped.expect("healthy cancellation poisoned the generation");
        }
    }
    async fn thread(ask: bool, hostile: bool) {
        let entered = Arc::new(Notify::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            shutdown_timeout: Duration::from_secs(2),
            ..Default::default()
        });
        let actor = system
            .spawn_at(
                ThreadCaller {
                    ask,
                    entered: entered.clone(),
                    drops: drops.clone(),
                    hostile,
                },
                "/context-receipt",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let result =
            std::panic::AssertUnwindSafe(exercise(&actor, &entered, &drops, true, hostile));
        use futures::FutureExt;
        let result = result.catch_unwind().await;
        let _ = system.shutdown_internal().await;
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
    fn actix(ask: bool, hostile: bool) {
        actix::System::new().block_on(async {
            let entered = Arc::new(Notify::new());
            let drops = Arc::new(AtomicUsize::new(0));
            let system = ActixActorSystem::with_settings(ActixSystemSettings {
                arbiter_count: 1,
                shutdown_timeout: Duration::from_secs(2),
                ..Default::default()
            })
            .await
            .unwrap();
            let actor = system
                .spawn_root_typed(
                    ActixCaller {
                        ask,
                        entered: entered.clone(),
                        drops: drops.clone(),
                        hostile,
                    },
                    EmptyConfig,
                )
                .await
                .unwrap();
            use futures::FutureExt;
            let result = std::panic::AssertUnwindSafe(exercise(
                actor.as_ref(),
                &entered,
                &drops,
                false,
                hostile,
            ))
            .catch_unwind()
            .await;
            let _ = system.shutdown().await;
            if let Err(payload) = result {
                std::panic::resume_unwind(payload);
            }
        });
    }
    fn isolated(case: &str, is_actix: bool, ask: bool, hostile: bool) {
        const FLAG: &str = "PARROT_CONTEXT_WATCH_RECEIPT";
        if std::env::var(FLAG).as_deref() == Ok(case) {
            if is_actix {
                actix(ask, hostile);
            } else {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(thread(ask, hostile));
            }
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("context_watch_receipt::{case}"),
                "--nocapture",
            ])
            .env(FLAG, case)
            .env("RUST_BACKTRACE", "0")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{case}: managed cancellation receipt failed: {status}"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{case}: cancellation receipt deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    macro_rules! case {
        ($name:ident, $actix:literal, $ask:literal, $hostile:literal) => {
            #[test]
            fn $name() {
                isolated(stringify!($name), $actix, $ask, $hostile);
            }
        };
    }
    case!(
        thread_watch_cancel_reports_cleanup_failure,
        false,
        false,
        true
    );
    case!(
        thread_watch_healthy_cancellation_stops_cleanly,
        false,
        false,
        false
    );
    case!(
        thread_unwatch_cancel_reports_cleanup_failure,
        false,
        true,
        true
    );
    case!(
        thread_unwatch_healthy_cancellation_stops_cleanly,
        false,
        true,
        false
    );
    case!(
        actix_watch_cancel_reports_cleanup_failure,
        true,
        false,
        true
    );
    case!(
        actix_watch_healthy_cancellation_stops_cleanly,
        true,
        false,
        false
    );
    case!(
        actix_unwatch_cancel_reports_cleanup_failure,
        true,
        true,
        true
    );
    case!(
        actix_unwatch_healthy_cancellation_stops_cleanly,
        true,
        true,
        false
    );
}
