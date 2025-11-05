//! Probe outer creation futures independently from the fixed native entry.
use parrot::{
    system::{ActorSystemImpl, ParrotActorSystem},
    thread::{ThreadActorSystem, ThreadContext},
};
use parrot_api::{
    actor::{Actor, ActorConfig, ActorState},
    system::ActorSystem,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Debug)]
struct Input {
    drops: Arc<AtomicUsize>,
    hostile: bool,
}
impl ActorConfig for Input {}
impl Drop for Input {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.hostile, "outer input destructor");
    }
}
struct Probe(Input);
impl Actor for Probe {
    type Config = Input;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let _ = &self.0;
        Box::pin(async { Ok(message) })
    }
}
fn inputs() -> (Arc<AtomicUsize>, Probe, Input) {
    let count = Arc::new(AtomicUsize::new(0));
    (
        count.clone(),
        Probe(Input {
            drops: count.clone(),
            hostile: true,
        }),
        Input {
            drops: count,
            hostile: false,
        },
    )
}
async fn cancelled(future: impl std::future::Future + Send, count: &AtomicUsize) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future)));
    assert!(
        result.is_ok(),
        "unpolled outer future ran a user destructor on its caller"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while count.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both inputs retired independently");
}
#[tokio::test]
async fn native_thread_positive_control() {
    let sys = ThreadActorSystem::shared(Default::default());
    let (count, actor, config) = inputs();
    cancelled(sys.spawn_root_typed_thread(actor, config), &count).await;
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn implementation_generic_unpolled_inputs() {
    let sys = ThreadActorSystem::shared(Default::default());
    let outer = ActorSystemImpl::Thread(sys.clone());
    let (count, actor, config) = inputs();
    cancelled(outer.spawn_root_typed(actor, config), &count).await;
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn facade_native_unpolled_inputs() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let (count, actor, config) = inputs();
    cancelled(facade.spawn_root_thread(actor, config), &count).await;
    facade.internal_shutdown().await.unwrap();
}
#[tokio::test]
async fn facade_named_unpolled_inputs() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let (count, actor, config) = inputs();
    cancelled(
        facade.spawn_root_thread_in_system("missing", actor, config),
        &count,
    )
    .await;
    facade.internal_shutdown().await.unwrap();
}
#[tokio::test]
async fn facade_trait_unpolled_inputs() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let (count, actor, config) = inputs();
    cancelled(
        ActorSystem::spawn_root_typed(&facade, actor, config),
        &count,
    )
    .await;
    facade.internal_shutdown().await.unwrap();
}

use futures::FutureExt;
use parrot_api::{context::ActorContext, errors::ActorError, system::SystemError};

async fn rejection(
    future: impl std::future::Future<
        Output = Result<Box<dyn parrot_api::address::ActorRef>, SystemError>,
    >,
    count: &AtomicUsize,
) {
    let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
    assert!(matches!(
        result,
        Ok(Err(SystemError::ActorError(ActorError::InternalError(_))))
    ));
    assert_eq!(count.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn unsupported_generic_retires_both_panicking_inputs() {
    let sys = ThreadActorSystem::shared(Default::default());
    let outer = ActorSystemImpl::Thread(sys.clone());
    let (count, actor, mut config) = inputs();
    config.hostile = true;
    rejection(outer.spawn_root_typed(actor, config), &count).await;
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn missing_named_engine_retires_both_panicking_inputs() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let (count, actor, mut config) = inputs();
    config.hostile = true;
    rejection(
        facade.spawn_root_thread_in_system("missing", actor, config),
        &count,
    )
    .await;
    facade.internal_shutdown().await.unwrap();
}
#[tokio::test]
async fn closed_facade_preserves_normal_error_after_disposal() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    parrot_api::engine::AsyncEngineRuntime::shutdown(&facade)
        .await
        .unwrap();
    let (count, mut actor, config) = inputs();
    actor.0.hostile = false;
    let result = facade.spawn_root_thread(actor, config).await;
    assert!(matches!(result, Err(SystemError::ShuttingDown)));
    assert_eq!(count.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn legacy_default_and_named_selection_still_happen_at_first_poll() {
    for named in [false, true] {
        let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
        let sys = ThreadActorSystem::shared(Default::default());
        let (count, mut actor, config) = inputs();
        actor.0.hostile = false;
        let future: BoxedFuture<'_, Result<Box<dyn parrot_api::address::ActorRef>, SystemError>> =
            if named {
                Box::pin(facade.spawn_root_thread_in_system("late", actor, config))
            } else {
                Box::pin(facade.spawn_root_thread(actor, config))
            };
        assert_eq!(count.load(Ordering::SeqCst), 0);
        facade
            .register_thread_system("late".into(), sys.clone(), true)
            .await
            .unwrap();
        let reference = future.await.expect("legacy selection remains lazy");
        let reply = reference.send(Box::new(42u64)).await.unwrap();
        assert_eq!(*reply.downcast::<u64>().unwrap(), 42);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        reference.stop().await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(sys.actor_count(), 0);
        facade.internal_shutdown().await.unwrap();
    }
}
struct ActixProbe(Input);
impl Actor for ActixProbe {
    fn use_async_handler(&self) -> bool {
        true
    }
    type Config = Input;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let _ = &self.0;
        Box::pin(async { Ok(message) })
    }
}
impl parrot_api::actor::ActixEngineExt for ActixProbe {}
struct BoxedProbe(Input);
impl Actor for BoxedProbe {
    type Config = Box<dyn std::any::Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let _ = &self.0;
        Box::pin(async { Ok(message) })
    }
}
#[tokio::test]
async fn implementation_wrong_actix_backend_retires_independent_inputs() {
    let sys = ThreadActorSystem::shared(Default::default());
    let outer = ActorSystemImpl::Thread(sys.clone());
    let (count, Probe(actor), mut config) = inputs();
    config.hostile = true;
    rejection(
        outer.spawn_root_typed_actix(ActixProbe(actor), config),
        &count,
    )
    .await;
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn facade_wrong_actix_backend_retires_independent_inputs() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let sys = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("thread".into(), sys, true)
        .await
        .unwrap();
    let (count, Probe(actor), mut config) = inputs();
    config.hostile = true;
    rejection(facade.spawn_root_actix(ActixProbe(actor), config), &count).await;
    facade.internal_shutdown().await.unwrap();
}
#[tokio::test]
async fn facade_actix_unpolled_retires_both_inputs() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let (count, Probe(actor), mut config) = inputs();
    config.hostile = true;
    cancelled(facade.spawn_root_actix(ActixProbe(actor), config), &count).await;
    facade.internal_shutdown().await.unwrap();
}
#[tokio::test]
async fn implementation_boxed_unpolled_retires_both_inputs() {
    let sys = ThreadActorSystem::shared(Default::default());
    let outer = ActorSystemImpl::Thread(sys.clone());
    let (count, Probe(actor), mut config) = inputs();
    config.hostile = true;
    cancelled(
        outer.spawn_root_boxed(Box::new(BoxedProbe(actor)), Box::new(config)),
        &count,
    )
    .await;
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn facade_trait_boxed_unpolled_retires_both_inputs() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let (count, Probe(actor), mut config) = inputs();
    config.hostile = true;
    cancelled(
        ActorSystem::spawn_root_boxed(&facade, Box::new(BoxedProbe(actor)), Box::new(config)),
        &count,
    )
    .await;
    facade.internal_shutdown().await.unwrap();
}
#[test]
fn facade_unpolled_cleanup_survives_host_exit() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let facade = runtime
        .block_on(ParrotActorSystem::new(Default::default()))
        .unwrap();
    let (count, actor, mut config) = inputs();
    config.hostile = true;
    let future = facade.spawn_root_thread(actor, config);
    drop(runtime);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future)));
    assert!(outcome.is_ok());
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[actix::test]
async fn actix_implementation_unpolled_typed_and_boxed_inputs() {
    let system = parrot::actix::ActixActorSystem::with_arbiter_count(1)
        .await
        .unwrap();
    let outer = ActorSystemImpl::Actix(system.clone());
    let (count, Probe(actor), mut config) = inputs();
    config.hostile = true;
    cancelled(
        outer.spawn_root_typed_actix(ActixProbe(actor), config),
        &count,
    )
    .await;
    let (count, Probe(actor), mut config) = inputs();
    config.hostile = true;
    cancelled(
        outer.spawn_root_boxed(Box::new(BoxedProbe(actor)), Box::new(config)),
        &count,
    )
    .await;
    system.shutdown().await.unwrap();
}
#[actix::test]
async fn facade_successful_actix_and_boxed_dispatch_retire_inputs_once() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let system = parrot::actix::ActixActorSystem::with_arbiter_count(1)
        .await
        .unwrap();
    facade
        .register_actix_system("actix".into(), system, true)
        .await
        .unwrap();
    let (count, Probe(mut actor), config) = inputs();
    actor.hostile = false;
    let actor = facade
        .spawn_root_actix(ActixProbe(actor), config)
        .await
        .unwrap();
    assert_eq!(
        *actor
            .send(Box::new(42u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    actor.stop().await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    for thread in [false, true] {
        if thread {
            facade
                .register_thread_system(
                    "thread".into(),
                    ThreadActorSystem::shared(Default::default()),
                    true,
                )
                .await
                .unwrap();
        }
        let (count, Probe(mut actor), config) = inputs();
        actor.hostile = false;
        let actor =
            ActorSystem::spawn_root_boxed(&facade, Box::new(BoxedProbe(actor)), Box::new(config))
                .await
                .unwrap();
        assert_eq!(
            *actor
                .send(Box::new(84u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            84
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        actor.stop().await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
    facade.internal_shutdown().await.unwrap();
}

use parrot::thread::{
    ThreadActorConfig,
    system::{ErasedSpawnBox, ErasedSpawnable, TypedSpawnPayload},
};
impl parrot::thread::mailbox::admission::MessageAdmission for Input {
    fn prepare(
        &self,
        message: BoxedMessage,
    ) -> Result<BoxedMessage, parrot::thread::error::MailboxError> {
        Ok(message)
    }
}
fn engine(input: Input) -> ThreadActorConfig {
    ThreadActorConfig {
        message_admission: Some(Arc::new(input)),
        ..Default::default()
    }
}
#[tokio::test]
async fn erased_system_entry_unpolled_owns_inputs() {
    let sys = ThreadActorSystem::shared(Default::default());
    let (count, actor, config) = inputs();
    cancelled(
        sys.spawn_erased_actor(
            TypedSpawnPayload::new(actor).into_boxed(),
            Box::new(config),
            None,
        ),
        &count,
    )
    .await;
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn erased_box_path_unpolled_owns_payload_and_engine() {
    let sys = ThreadActorSystem::shared(Default::default());
    let (count, actor, config) = inputs();
    cancelled(
        ErasedSpawnBox::from_typed(actor).spawn_at_path(
            sys.clone(),
            "/unpolled".into(),
            engine(config),
        ),
        &count,
    )
    .await;
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_erased_trait_unpolled_owns_payload_and_config() {
    let sys = ThreadActorSystem::shared(Default::default());
    let (count, actor, config) = inputs();
    cancelled(
        Box::new(TypedSpawnPayload::new(actor)).spawn_on(
            sys.clone(),
            Box::new(config),
            Default::default(),
        ),
        &count,
    )
    .await;
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_erased_trait_path_unpolled_owns_payload_and_engine() {
    let sys = ThreadActorSystem::shared(Default::default());
    let (count, actor, config) = inputs();
    cancelled(
        Box::new(TypedSpawnPayload::new(actor)).spawn_on_path(
            sys.clone(),
            "/unpolled".into(),
            engine(config),
        ),
        &count,
    )
    .await;
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
struct CustomPayload(Input);
#[async_trait::async_trait]
impl ErasedSpawnable for CustomPayload {
    async fn spawn_on(
        self: Box<Self>,
        _: Arc<ThreadActorSystem>,
        _: BoxedMessage,
        _: ThreadActorConfig,
    ) -> ActorResult<Box<dyn parrot_api::address::ActorRef>> {
        let _ = &self.0;
        panic!("unpolled default path wrapper must not call the custom implementation");
    }
}
#[tokio::test]
async fn default_custom_erased_path_unpolled_owns_inputs() {
    let sys = ThreadActorSystem::shared(Default::default());
    let (count, Probe(actor), config) = inputs();
    cancelled(
        Box::new(CustomPayload(actor)).spawn_on_path(
            sys.clone(),
            "/unpolled".into(),
            engine(config),
        ),
        &count,
    )
    .await;
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn erased_config_mismatch_retires_all_three_panicking_inputs() {
    let sys = ThreadActorSystem::shared(Default::default());
    let (count, actor, mut config) = inputs();
    config.hostile = true;
    let policy = Input {
        drops: count.clone(),
        hostile: true,
    };
    let result = std::panic::AssertUnwindSafe(Box::new(TypedSpawnPayload::new(actor)).spawn_on(
        sys.clone(),
        Box::new(vec![config]),
        engine(policy),
    ))
    .catch_unwind()
    .await;
    assert!(matches!(result, Ok(Err(ActorError::InternalError(_)))));
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn erased_path_keeps_requested_registration_and_actual_retirement() {
    let sys = ThreadActorSystem::shared(Default::default());
    let (count, mut actor, config) = inputs();
    actor.0.hostile = false;
    let reference = ErasedSpawnBox::from_typed(actor)
        .spawn_at_path(sys.clone(), "/erased-kept".into(), engine(config))
        .await
        .unwrap();
    assert_eq!(reference.path(), "/erased-kept");
    assert!(sys.get_actor_ref("/erased-kept").is_some());
    assert_eq!(
        *reference
            .send(Box::new(42u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    reference.stop().await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn erased_system_entry_dispatches_and_retires_once() {
    let sys = ThreadActorSystem::shared(Default::default());
    let (count, mut actor, config) = inputs();
    actor.0.hostile = false;
    let reference = sys
        .spawn_erased_actor(
            TypedSpawnPayload::new(actor).into_boxed(),
            Box::new(config),
            None,
        )
        .await
        .unwrap();
    assert!(sys.get_actor_ref(&reference.path()).is_some());
    assert_eq!(
        *reference
            .send(Box::new(42u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    reference.stop().await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
