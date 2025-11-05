use parrot::{
    actix::{ActixActor, ActixActorSystem, ActixContext},
    thread::{SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadContext},
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[derive(Clone)]
struct Marker {
    identity: Arc<()>,
    formats: Arc<AtomicUsize>,
}
impl std::fmt::Debug for Marker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Marker")
    }
}
impl std::fmt::Display for Marker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.formats.fetch_add(1, Ordering::SeqCst);
        f.write_str("marker stop error")
    }
}
impl std::error::Error for Marker {}
fn marker() -> Marker {
    Marker {
        identity: Arc::new(()),
        formats: Arc::new(AtomicUsize::new(0)),
    }
}
fn contains(error: &ActorError, marker: &Marker) -> bool {
    match error {
        ActorError::Other(error) => error.chain().any(|error| {
            error
                .downcast_ref::<Marker>()
                .is_some_and(|error| Arc::ptr_eq(&error.identity, &marker.identity))
        }),
        _ => false,
    }
}
struct ThreadProbe(Marker);
struct ActixProbe(Marker);
macro_rules! actor {
    ($name:ident,$ctx:ty) => {
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $ctx;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { Ok(message) })
            }
            fn before_stop<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                let error = self.0.clone();
                Box::pin(async move { Err(ActorError::Other(anyhow::Error::new(error))) })
            }
        }
    };
}
actor!(ThreadProbe, ThreadContext<Self>);
actor!(ActixProbe, ActixContext<ActixActor<Self>>);
impl ActixEngineExt for ActixProbe {}
#[test]
fn original_other_error_has_the_marker_identity() {
    let marker = marker();
    let error = ActorError::Other(anyhow::Error::new(marker.clone()));
    assert!(contains(&error, &marker));
    assert_eq!(marker.formats.load(Ordering::SeqCst), 0);
}
async fn thread(mode: SchedulingMode) {
    let marker = marker();
    let sys = ThreadActorSystem::shared(Default::default());
    let actor = sys
        .spawn_typed_with_config(
            ThreadProbe(marker.clone()),
            EmptyConfig,
            ThreadActorConfig {
                scheduling_mode: Some(mode),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), actor.stop())
        .await
        .expect("stop must complete");
    let preserved = matches!(result,Err(ref error) if contains(error,&marker));
    eprintln!(
        "marker preserved={preserved}, formatter calls={}",
        marker.formats.load(Ordering::SeqCst)
    );
    let _ = sys.shutdown_internal().await;
    assert!(
        preserved,
        "returned stop error lost the original business error type/source/identity"
    );
}
#[tokio::test]
async fn shared_stop_preserves_business_error_identity() {
    thread(SchedulingMode::SharedPool {
        max_messages_per_run: 8,
    })
    .await;
}
#[tokio::test]
async fn dedicated_stop_preserves_business_error_identity() {
    thread(SchedulingMode::DedicatedThread).await;
}
#[tokio::test]
async fn sharded_stop_preserves_business_error_identity() {
    thread(SchedulingMode::Sharded {
        affinity_key: "errors".into(),
    })
    .await;
}
#[actix::test]
async fn actix_stop_preserves_business_error_identity() {
    let marker = marker();
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let actor = sys
        .spawn_root_typed(ActixProbe(marker.clone()), EmptyConfig)
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), actor.stop())
        .await
        .expect("stop must complete");
    let preserved = matches!(result,Err(ref error) if contains(error,&marker));
    eprintln!(
        "marker preserved={preserved}, formatter calls={}",
        marker.formats.load(Ordering::SeqCst)
    );
    let _ = sys.shutdown().await;
    assert!(
        preserved,
        "returned stop error lost the original business error type/source/identity"
    );
}

struct FailureSpec {
    formats: AtomicUsize,
    drops: AtomicUsize,
    behavior: usize,
    hook: tokio::sync::Semaphore,
    drop_entered: tokio::sync::Notify,
    drop_gate: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}
struct BusinessFailure(Arc<FailureSpec>);
impl std::fmt::Debug for BusinessFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BusinessFailure")
    }
}
impl std::fmt::Display for BusinessFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.formats.fetch_add(1, Ordering::SeqCst);
        assert_ne!(
            self.0.behavior, 1,
            "business error formatter must not run in lifecycle code"
        );
        f.write_str("original business failure")
    }
}
impl std::error::Error for BusinessFailure {}
impl Drop for BusinessFailure {
    fn drop(&mut self) {
        let gate = { self.0.drop_gate.lock().unwrap().take() };
        if let Some(gate) = gate {
            self.0.drop_entered.notify_one();
            gate.recv_timeout(Duration::from_secs(5))
                .expect("final error destruction blocked the receiving caller");
        }
        self.0.drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            !matches!(self.0.behavior, 2 | 4),
            "business error destructor"
        );
    }
}
struct OwnedThreadFailure(Arc<FailureSpec>);
struct OwnedActixFailure(Arc<FailureSpec>);
macro_rules! failing_actor {
    ($actor:ident, $context:ty) => {
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
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async { Ok(msg) })
            }
            fn before_stop<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    let _permit = self.0.hook.acquire().await.unwrap();
                    if self.0.behavior == 6 {
                        return Err(ActorError::Timeout);
                    }
                    Err(ActorError::Other(anyhow::Error::new(BusinessFailure(
                        self.0.clone(),
                    ))))
                })
            }
        }
    };
}
failing_actor!(OwnedThreadFailure, ThreadContext<Self>);
failing_actor!(OwnedActixFailure, ActixContext<ActixActor<Self>>);
impl ActixEngineExt for OwnedActixFailure {}
fn retained_business(error: &ActorError, expected: &Arc<FailureSpec>) {
    use std::error::Error as _;
    let mut source = error.source();
    let mut found = false;
    while let Some(error) = source {
        if let Some(error) = error.downcast_ref::<BusinessFailure>() {
            assert!(Arc::ptr_eq(&error.0, expected));
            found = true;
            break;
        }
        source = error.source();
    }
    assert!(
        found,
        "standard Error::source lost the original business failure"
    );
}
async fn shared_receipt_contract(engine: usize, behavior: usize) {
    let (release, gate) = std::sync::mpsc::channel();
    let spec = Arc::new(FailureSpec {
        drop_entered: tokio::sync::Notify::new(),
        drop_gate: std::sync::Mutex::new((behavior == 5).then_some(gate)),
        formats: AtomicUsize::new(0),
        drops: AtomicUsize::new(0),
        behavior,
        hook: tokio::sync::Semaphore::new(0),
    });
    let mut thread = None;
    let mut actix = None;
    let actor: Box<dyn ActorRef> = if engine >= 8 {
        let made = spec.clone();
        if engine == 9 {
            let sys = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
            let actor = sys
                .spawn_root_factory(move || OwnedActixFailure(made.clone()))
                .await
                .unwrap();
            actix = Some(sys);
            actor
        } else {
            let sys = ThreadActorSystem::shared(Default::default());
            let actor = sys
                .spawn_supervised(
                    move || OwnedThreadFailure(made.clone()),
                    "/dynamic-factory-failure",
                    parrot::thread::SupervisorStrategy::Stop,
                )
                .await
                .unwrap();
            thread = Some(sys);
            Box::new(actor)
        }
    } else if engine >= 4 {
        if engine >= 6 {
            let sys = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
            let actor = if engine == 7 {
                let made = spec.clone();
                sys.spawn_typed_factory::<_, ReceiptPing, _>(
                    move || StaticFailure(made.clone()),
                    "/static-failure",
                )
                .await
                .unwrap()
            } else {
                sys.spawn_typed::<_, ReceiptPing>(StaticFailure(spec.clone()), "/static-failure")
                    .await
                    .unwrap()
            };
            actix = Some(sys);
            actor.into_dyn()
        } else {
            let sys = ThreadActorSystem::shared(Default::default());
            let actor = if engine == 5 {
                let made = spec.clone();
                sys.spawn_typed_factory::<_, ReceiptPing, _>(
                    move || StaticFailure(made.clone()),
                    "/static-failure",
                )
                .await
                .unwrap()
            } else {
                sys.spawn_typed::<_, ReceiptPing>(StaticFailure(spec.clone()), "/static-failure")
                    .await
                    .unwrap()
            };
            thread = Some(sys);
            actor.into_dyn()
        }
    } else if engine == 3 {
        let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let actor = sys
            .spawn_root_typed(OwnedActixFailure(spec.clone()), EmptyConfig)
            .await
            .unwrap();
        actix = Some(Arc::new(sys));
        actor
    } else {
        let sys = ThreadActorSystem::shared(Default::default());
        let mode = match engine {
            0 => SchedulingMode::SharedPool {
                max_messages_per_run: 8,
            },
            1 => SchedulingMode::DedicatedThread,
            _ => SchedulingMode::Sharded {
                affinity_key: "error-receipts".into(),
            },
        };
        let actor = sys
            .spawn_typed_with_config(
                OwnedThreadFailure(spec.clone()),
                EmptyConfig,
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        thread = Some(sys);
        Box::new(actor)
    };
    let message: BoxedMessage = if (4..8).contains(&engine) {
        Box::new(ReceiptPing(7))
    } else {
        Box::new(7u64)
    };
    actor.send(message).await.unwrap();
    let mut first = actor.stop();
    let mut second = actor.stop();
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    let first = if behavior == 3 {
        drop(first);
        None
    } else {
        Some(first)
    };
    spec.hook.add_permits(1);
    let (first, second) = tokio::time::timeout(Duration::from_secs(3), async {
        let first = match first {
            Some(first) => Some(first.await.unwrap_err()),
            None => None,
        };
        (first, second.await.unwrap_err())
    })
    .await
    .expect("stop receipts must survive cancellation and formatting hazards");
    if behavior == 4 {
        for error in first.iter().chain(std::iter::once(&second)) {
            assert!(
                matches!(error,ActorError::Panic(text) if text.contains("actor cleanup failure")),
                "actual cleanup failure must survive discarded business error destruction: {error:?}"
            );
        }
    } else {
        if let Some(error) = &first {
            retained_business(error, &spec);
        }
        retained_business(&second, &spec);
    }
    assert_eq!(
        spec.formats.load(Ordering::SeqCst),
        0,
        "framework formatted business error"
    );
    if behavior != 4 {
        assert_eq!(
            spec.drops.load(Ordering::SeqCst),
            0,
            "source disposed while borrowed by receipt"
        );
    }
    if let Some(sys) = thread {
        sys.shutdown_internal().await.unwrap();
    }
    if let Some(sys) = actix {
        sys.as_ref().clone().shutdown().await.unwrap();
    }
    if behavior == 0 {
        assert!(second.to_string().contains("original business failure"));
        assert_eq!(spec.formats.load(Ordering::SeqCst), 1);
    }
    drop(actor);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(first))).is_ok());
    if behavior != 4 {
        assert_eq!(
            spec.drops.load(Ordering::SeqCst),
            0,
            "one error receipt still owns the source"
        );
    }
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(second))).is_ok());
    if behavior == 5 {
        tokio::time::timeout(Duration::from_secs(3), spec.drop_entered.notified())
            .await
            .expect("error cleanup did not begin");
        assert_eq!(
            spec.drops.load(Ordering::SeqCst),
            0,
            "blocked destructor reported completion early"
        );
        release
            .send(())
            .expect("destructor must still be waiting for the caller's release");
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while spec.drops.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("final shared error source was not retired");
    assert_eq!(spec.drops.load(Ordering::SeqCst), 1);
}
macro_rules! receipt_test {
    ($name:ident,$engine:expr,$behavior:expr) => {
        #[actix::test]
        async fn $name() {
            shared_receipt_contract($engine, $behavior).await;
        }
    };
}
receipt_test!(shared_waiters_keep_exact_source, 0, 0);
receipt_test!(dedicated_waiters_keep_exact_source, 1, 0);
receipt_test!(sharded_waiters_keep_exact_source, 2, 0);
receipt_test!(actix_waiters_keep_exact_source, 3, 0);
receipt_test!(shared_stop_does_not_format_business_error, 0, 1);
receipt_test!(dedicated_stop_does_not_format_business_error, 1, 1);
receipt_test!(sharded_stop_does_not_format_business_error, 2, 1);
receipt_test!(actix_stop_does_not_format_business_error, 3, 1);
receipt_test!(shared_error_drop_is_isolated, 0, 2);
receipt_test!(dedicated_error_drop_is_isolated, 1, 2);
receipt_test!(sharded_error_drop_is_isolated, 2, 2);
receipt_test!(actix_error_drop_is_isolated, 3, 2);
receipt_test!(shared_cancelled_waiter_keeps_error_source, 0, 3);
receipt_test!(dedicated_cancelled_waiter_keeps_error_source, 1, 3);
receipt_test!(sharded_cancelled_waiter_keeps_error_source, 2, 3);
receipt_test!(actix_cancelled_waiter_keeps_error_source, 3, 3);

struct ReceiptPing(u64);
impl parrot_api::message::Message for ReceiptPing {
    type Result = u64;
}
struct StaticFailure(Arc<FailureSpec>);
impl parrot_api::typed::ParrotTypedDispatch for StaticFailure {
    type Msg = ReceiptPing;
    type Reply = u64;
    fn dispatch(&mut self, msg: ReceiptPing) -> BoxedFuture<'_, ActorResult<u64>> {
        Box::pin(async move { Ok(msg.0) })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let _permit = self.0.hook.acquire().await.unwrap();
            Err(ActorError::Other(anyhow::Error::new(BusinessFailure(
                self.0.clone(),
            ))))
        })
    }
}
impl parrot_api::typed::ParrotMsgVariant<StaticFailure> for ReceiptPing {
    fn inject(msg: Self) -> Self {
        msg
    }
    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}
receipt_test!(thread_static_waiters_keep_source, 4, 0);
receipt_test!(thread_static_does_not_format_error, 4, 1);
receipt_test!(thread_static_error_drop_is_isolated, 4, 2);
receipt_test!(thread_static_cancelled_waiter_keeps_source, 4, 3);
receipt_test!(thread_static_factory_waiters_keep_source, 5, 0);
receipt_test!(thread_static_factory_does_not_format_error, 5, 1);
receipt_test!(thread_static_factory_error_drop_is_isolated, 5, 2);
receipt_test!(thread_static_factory_cancelled_waiter_keeps_source, 5, 3);
receipt_test!(actix_static_waiters_keep_source, 6, 0);
receipt_test!(actix_static_does_not_format_error, 6, 1);
receipt_test!(actix_static_error_drop_is_isolated, 6, 2);
receipt_test!(actix_static_cancelled_waiter_keeps_source, 6, 3);
receipt_test!(actix_static_factory_waiters_keep_source, 7, 0);
receipt_test!(actix_static_factory_does_not_format_error, 7, 1);
receipt_test!(actix_static_factory_error_drop_is_isolated, 7, 2);
receipt_test!(actix_static_factory_cancelled_waiter_keeps_source, 7, 3);

macro_rules! compound_drop {
    ($actor:ident) => {
        impl Drop for $actor {
            fn drop(&mut self) {
                assert_ne!(self.0.behavior, 4, "actor cleanup failure");
            }
        }
    };
}
compound_drop!(OwnedThreadFailure);
compound_drop!(OwnedActixFailure);
compound_drop!(StaticFailure);
receipt_test!(shared_compound_cleanup, 0, 4);
receipt_test!(dedicated_compound_cleanup, 1, 4);
receipt_test!(sharded_compound_cleanup, 2, 4);
receipt_test!(actix_compound_cleanup, 3, 4);
receipt_test!(thread_static_compound_cleanup, 4, 4);
receipt_test!(thread_static_factory_compound_cleanup, 5, 4);
receipt_test!(actix_static_compound_cleanup, 6, 4);
receipt_test!(actix_static_factory_compound_cleanup, 7, 4);
receipt_test!(thread_dynamic_factory_waiters_keep_source, 8, 0);
receipt_test!(thread_dynamic_factory_does_not_format_error, 8, 1);
receipt_test!(thread_dynamic_factory_error_drop_is_isolated, 8, 2);
receipt_test!(thread_dynamic_factory_cancelled_waiter_keeps_source, 8, 3);
receipt_test!(thread_dynamic_factory_compound_cleanup, 8, 4);
receipt_test!(actix_dynamic_factory_waiters_keep_source, 9, 0);
receipt_test!(actix_dynamic_factory_does_not_format_error, 9, 1);
receipt_test!(actix_dynamic_factory_error_drop_is_isolated, 9, 2);
receipt_test!(actix_dynamic_factory_cancelled_waiter_keeps_source, 9, 3);
receipt_test!(actix_dynamic_factory_compound_cleanup, 9, 4);

receipt_test!(shared_blocking_error_drop_keeps_caller_progress, 0, 5);
receipt_test!(actix_blocking_error_drop_keeps_caller_progress, 3, 5);
#[test]
fn retained_error_outlives_host_and_final_drop_is_isolated() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let spec = Arc::new(FailureSpec {
        formats: AtomicUsize::new(0),
        drops: AtomicUsize::new(0),
        behavior: 2,
        hook: tokio::sync::Semaphore::new(1),
        drop_entered: tokio::sync::Notify::new(),
        drop_gate: std::sync::Mutex::new(None),
    });
    let error = runtime.block_on(async {
        let sys = ThreadActorSystem::shared(Default::default());
        let actor = sys
            .spawn_root_typed_thread(OwnedThreadFailure(spec.clone()), EmptyConfig)
            .await
            .unwrap();
        actor.send(Box::new(7u64)).await.unwrap();
        let error = actor.stop().await.unwrap_err();
        sys.shutdown_internal().await.unwrap();
        drop(actor);
        error
    });
    drop(runtime);
    retained_business(&error, &spec);
    assert_eq!(spec.drops.load(Ordering::SeqCst), 0);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(error))).is_ok());
    assert_eq!(spec.drops.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn actix_plain_timeout_keeps_existing_stop_error_category() {
    let spec = Arc::new(FailureSpec {
        formats: AtomicUsize::new(0),
        drops: AtomicUsize::new(0),
        behavior: 6,
        hook: tokio::sync::Semaphore::new(1),
        drop_entered: tokio::sync::Notify::new(),
        drop_gate: std::sync::Mutex::new(None),
    });
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let actor = sys
        .spawn_root_typed(OwnedActixFailure(spec), EmptyConfig)
        .await
        .unwrap();
    actor.send(Box::new(7u64)).await.unwrap();
    let result = actor.stop().await;
    sys.shutdown().await.unwrap();
    assert!(
        matches!(result, Err(ActorError::TimeoutDetail(_))),
        "existing Actix stop category must remain TimeoutDetail, observed {result:?}"
    );
}
