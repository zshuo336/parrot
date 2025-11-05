//! Explicit path requests must not silently create an actor at another address.
use parrot::thread::{
    ThreadActorConfig, ThreadActorSystem, ThreadContext,
    system::{ErasedSpawnable, TypedSpawnPayload},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}

struct AutomaticPayload(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl ErasedSpawnable for AutomaticPayload {
    async fn spawn_on(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        _: BoxedMessage,
        engine: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::new(TypedSpawnPayload::new(Echo))
            .spawn_on(system, Box::new(EmptyConfig), engine)
            .await
    }
}

struct ExplicitPayload;
#[async_trait::async_trait]
impl ErasedSpawnable for ExplicitPayload {
    async fn spawn_on(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        config: BoxedMessage,
        engine: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        Box::new(TypedSpawnPayload::new(Echo))
            .spawn_on(system, config, engine)
            .await
    }
    fn spawn_on_path<'async_trait>(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        path: String,
        engine: ThreadActorConfig,
    ) -> BoxedFuture<'async_trait, ActorResult<BoxedActorRef>>
    where
        Self: 'async_trait,
    {
        Box::new(TypedSpawnPayload::new(Echo)).spawn_on_path(system, path, engine)
    }
}

#[tokio::test]
async fn default_explicit_path_rejects_before_automatic_spawn() {
    let system = ThreadActorSystem::shared(Default::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let result = Box::new(AutomaticPayload(calls.clone()))
        .spawn_on_path(system.clone(), "/requested".into(), Default::default())
        .await;
    let rejected = matches!(&result, Err(ActorError::InternalError(message)) if message.contains("explicit path"));
    let invoked = calls.load(Ordering::SeqCst);
    let published = system.actor_count();
    // Close a wrongly published actor before asserting, so the baseline failure
    // cannot be mistaken for a runtime shutdown or leaked-worker failure.
    if let Ok(reference) = result {
        reference.stop().await.unwrap();
    }
    system.shutdown_internal().await.unwrap();
    assert!(
        rejected,
        "explicit path requires support; automatic spawning is not success"
    );
    assert_eq!(
        invoked, 0,
        "unsupported operation must not invoke user spawn code"
    );
    assert_eq!(
        published, 0,
        "unsupported operation must not publish an actor"
    );
}

#[tokio::test]
async fn automatic_spawn_remains_available() {
    let system = ThreadActorSystem::shared(Default::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let reference = Box::new(AutomaticPayload(calls.clone()))
        .spawn_on(system.clone(), Box::new(()), Default::default())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(system.get_actor_ref(&reference.path()).is_some());
    assert_eq!(
        *reference
            .send(Box::new(73u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        73
    );
    reference.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn custom_explicit_path_support_preserves_registration_and_collision() {
    let system = ThreadActorSystem::shared(Default::default());
    let reference = Box::new(ExplicitPayload)
        .spawn_on_path(system.clone(), "/requested".into(), Default::default())
        .await
        .unwrap();
    assert_eq!(reference.path(), "/requested");
    assert!(system.get_actor_ref("/requested").is_some());
    let collision = Box::new(ExplicitPayload)
        .spawn_on_path(system.clone(), "/requested".into(), Default::default())
        .await;
    assert!(collision.is_err(), "duplicate path must be rejected");
    assert_eq!(system.actor_count(), 1);
    assert_eq!(
        *reference
            .send(Box::new(91u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        91
    );
    reference.stop().await.unwrap();
    assert!(system.get_actor_ref("/requested").is_none());
    system.shutdown_internal().await.unwrap();
}

#[derive(Debug)]
struct Input {
    drops: Arc<AtomicUsize>,
    panic_on_drop: bool,
    gate: Option<(
        tokio::sync::oneshot::Sender<()>,
        std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    )>,
}
impl Drop for Input {
    fn drop(&mut self) {
        if let Some((entered, release)) = self.gate.take() {
            let _ = entered.send(());
            release
                .into_inner()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("release input destructor");
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panic_on_drop,
            "explicit path rejected input destructor"
        );
    }
}
impl parrot::thread::mailbox::admission::MessageAdmission for Input {
    fn prepare(
        &self,
        message: BoxedMessage,
    ) -> Result<BoxedMessage, parrot::thread::error::MailboxError> {
        Ok(message)
    }
}
struct TrackedPayload {
    input: Input,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl ErasedSpawnable for TrackedPayload {
    async fn spawn_on(
        self: Box<Self>,
        _: Arc<ThreadActorSystem>,
        _: BoxedMessage,
        _: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        let _ = &self.input;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ActorError::Stopped)
    }
}

#[tokio::test]
async fn default_rejection_isolates_both_input_destructor_panics() {
    use futures::FutureExt;
    let system = ThreadActorSystem::shared(Default::default());
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let input = || Input {
        drops: drops.clone(),
        panic_on_drop: true,
        gate: None,
    };
    let result = std::panic::AssertUnwindSafe(
        Box::new(TrackedPayload {
            input: input(),
            calls: calls.clone(),
        })
        .spawn_on_path(
            system.clone(),
            "/unsupported".into(),
            ThreadActorConfig {
                message_admission: Some(Arc::new(input())),
                ..Default::default()
            },
        ),
    )
    .catch_unwind()
    .await;
    assert!(
        matches!(result, Ok(Err(ActorError::InternalError(ref message))) if message.contains("disposal failed")),
        "cleanup failure must precede unsupported-path error"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}

async fn blocked_rejection(cancel: bool) {
    let system = ThreadActorSystem::shared(Default::default());
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let payload = TrackedPayload {
        input: Input {
            drops: drops.clone(),
            panic_on_drop: false,
            gate: Some((entered, std::sync::Mutex::new(released))),
        },
        calls: calls.clone(),
    };
    let engine = ThreadActorConfig {
        message_admission: Some(Arc::new(Input {
            drops: drops.clone(),
            panic_on_drop: false,
            gate: None,
        })),
        ..Default::default()
    };
    let mut pending =
        Box::new(payload).spawn_on_path(system.clone(), "/unsupported".into(), engine);
    assert!(futures::poll!(&mut pending).is_pending());
    tokio::time::timeout(std::time::Duration::from_secs(3), started)
        .await
        .unwrap()
        .unwrap();
    assert!(
        futures::poll!(&mut pending).is_pending(),
        "rejection must wait for actual retirement"
    );
    if cancel {
        drop(pending);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while drops.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelling waiter must not cancel cleanup");
    } else {
        release.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), pending)
            .await
            .unwrap();
        assert!(
            matches!(result, Err(ActorError::InternalError(ref message)) if message.contains("explicit path"))
        );
    }
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn default_rejection_waits_for_actual_input_retirement() {
    blocked_rejection(false).await;
}
#[tokio::test]
async fn cancelled_rejection_keeps_its_cleanup_owner() {
    blocked_rejection(true).await;
}
