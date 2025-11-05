use parrot::{
    mirror::{MirrorCopier, MirrorPolicy},
    system::ParrotActorSystem,
    thread::{config::ThreadActorConfig, context::ThreadContext, system::ThreadActorSystem},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    system::ActorSystem,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[derive(Debug, Clone, Default)]
struct Seen {
    values: Arc<Mutex<Vec<u64>>>,
    changed: Arc<tokio::sync::Notify>,
}
struct Observe(Seen);
impl Actor for Observe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let n = *msg.downcast::<u64>().unwrap();
            self.0.values.lock().unwrap().push(n);
            self.0.changed.notify_one();
            Ok(Box::new(n * 2) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
async fn wait_values(seen: &Seen, n: usize) -> bool {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if seen.values.lock().unwrap().len() >= n {
                return;
            }
            seen.changed.notified().await;
        }
    })
    .await
    .is_ok()
}
async fn setup() -> (ParrotActorSystem, Arc<ThreadActorSystem>, Seen, Seen) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    facade
        .register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
        .unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    let source = Seen::default();
    let shadow = Seen::default();
    engine
        .spawn_at(
            Observe(source.clone()),
            "/source/deep",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    engine
        .spawn_at(
            Observe(shadow.clone()),
            "/shadow",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    facade
        .register_thread_system("thread".into(), engine.clone(), true)
        .await
        .unwrap();
    (facade, engine, source, shadow)
}
#[tokio::test]
async fn facade_ask_and_tell_really_deliver_independent_shadow_messages() {
    let (facade, _, source, shadow) = setup().await;
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/shadow"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    let answer = actor.send(Box::new(3u64)).await.unwrap();
    assert_eq!(*answer.downcast::<u64>().unwrap(), 6);
    actor.deliver(Box::new(4u64)).await.unwrap();
    let received = wait_values(&shadow, 2).await;
    let processed = wait_values(&source, 2).await;
    if received {
        wait_for_stats(&handle, |s| s.admitted == 2).await;
    }
    facade.internal_shutdown().await.unwrap();
    assert!(processed, "primary TELL was not processed before shutdown");
    assert_eq!(*source.values.lock().unwrap(), vec![3, 4]);
    assert!(received, "registered mirror never received real traffic");
    assert_eq!(*shadow.values.lock().unwrap(), vec![3, 4]);
    assert_eq!(handle.stats().admitted, 2);
    assert!(handle.is_finished());
}
#[tokio::test]
async fn most_specific_policy_wins_independently_of_registration_order() {
    let (facade, _, _, _) = setup().await;
    facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/broad-shadow"))
        .unwrap();
    facade
        .register_mirror_policy(MirrorPolicy::new("/source/deep", "/shadow"))
        .unwrap();
    let selected = facade.mirror_policy_for("/source/deep").unwrap();
    facade.internal_shutdown().await.unwrap();
    assert_eq!(selected.mirror_path, "/shadow");
}

#[tokio::test]
async fn unpolled_calls_do_not_copy_or_execute_and_cloned_refs_share_the_worker() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (facade, _, source, shadow) = setup().await;
    let copies = Arc::new(AtomicUsize::new(0));
    let count = copies.clone();
    facade
        .register_mirror_copier(MirrorCopier::new(
            |_: &u64| 8,
            move |v: &u64| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(*v)
            },
        ))
        .unwrap();
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/shadow"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    drop(actor.send(Box::new(1u64)));
    drop(actor.deliver(Box::new(2u64)));
    drop(actor.send_with_timeout(Box::new(3u64), Some(Duration::from_secs(1))));
    assert_eq!(copies.load(Ordering::SeqCst), 0);
    assert_eq!(handle.stats().attempted, 0);
    assert!(source.values.lock().unwrap().is_empty());
    let cloned = actor.clone_boxed();
    let reply = cloned
        .send_with_timeout(Box::new(7u64), Some(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 14);
    assert!(wait_values(&shadow, 1).await);
    facade.internal_shutdown().await.unwrap();
    assert_eq!(copies.load(Ordering::SeqCst), 1);
    assert_eq!(*source.values.lock().unwrap(), vec![7]);
    assert_eq!(*shadow.values.lock().unwrap(), vec![7]);
}
#[tokio::test]
async fn expired_copy_budget_prevents_primary_admission() {
    use parrot_api::{errors::ActorError, message::MessageOptions};
    let (facade, _, source, _) = setup().await;
    facade
        .register_mirror_copier(MirrorCopier::new(
            |_: &u64| 8,
            |value: &u64| {
                std::thread::sleep(Duration::from_millis(25));
                Ok(*value)
            },
        ))
        .unwrap();
    facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/shadow"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    let budget = Duration::from_millis(1);
    let options = MessageOptions {
        timeout: Some(budget),
        ..Default::default()
    };
    let raw_ask = actor.send_with_timeout(Box::new(1u64), Some(budget)).await;
    let option_ask = actor
        .send_with_options(Box::new(2u64), options.clone())
        .await;
    let tell = actor.deliver_with_options(Box::new(3u64), options).await;
    // A normal-lane barrier proves no timed-out primary send was left queued.
    actor.send(Box::new(99u64)).await.unwrap();
    let seen = source.values.lock().unwrap().clone();
    facade.internal_shutdown().await.unwrap();
    assert!(matches!(
        raw_ask,
        Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
    ));
    assert!(matches!(
        option_ask,
        Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
    ));
    assert!(matches!(
        tell,
        Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
    ));
    assert_eq!(
        seen,
        [99],
        "expired copy must not admit primary business work"
    );
}

#[tokio::test]
async fn remove_and_replace_cancel_old_refs_without_stopping_primary_actors() {
    let (facade, engine, source, shadow) = setup().await;
    let other = Seen::default();
    engine
        .spawn_at(
            Observe(other.clone()),
            "/other",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    let old = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/shadow"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    actor.send(Box::new(1u64)).await.unwrap();
    assert!(wait_values(&shadow, 1).await);
    let new = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/other"))
        .unwrap();
    old.shutdown().await;
    actor.send(Box::new(2u64)).await.unwrap();
    let fresh = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    fresh.send(Box::new(3u64)).await.unwrap();
    assert!(wait_values(&other, 1).await);
    facade
        .remove_mirror_policy("/source/")
        .unwrap()
        .shutdown()
        .await;
    fresh.send(Box::new(4u64)).await.unwrap();
    assert!(facade.mirror_policy_for("/source/deep").is_none());
    facade.internal_shutdown().await.unwrap();
    assert!(old.is_finished() && new.is_finished());
    assert_eq!(*source.values.lock().unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(*shadow.values.lock().unwrap(), vec![1]);
    assert_eq!(*other.values.lock().unwrap(), vec![3]);
}
#[tokio::test]
async fn missing_target_copy_error_panic_and_oversize_preserve_the_primary_result() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (facade, _, source, shadow) = setup().await;
    let missing = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/missing"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    assert_eq!(
        *actor
            .send(Box::new(1u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        2
    );
    wait_for_stats(&missing, |s| s.missing_target == 1).await;
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/shadow"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    facade
        .register_mirror_copier(MirrorCopier::new(
            |_: &u64| 8,
            |_| Err("copy refused".into()),
        ))
        .unwrap();
    actor.send(Box::new(2u64)).await.unwrap();
    facade
        .register_mirror_copier(MirrorCopier::new(|_: &u64| 8, |_| panic!("copier failure")))
        .unwrap();
    actor.send(Box::new(3u64)).await.unwrap();
    let copied = Arc::new(AtomicUsize::new(0));
    let observe = copied.clone();
    facade
        .register_mirror_copier(MirrorCopier::new(
            |_: &u64| usize::MAX,
            move |v: &u64| {
                observe.fetch_add(1, Ordering::SeqCst);
                Ok(*v)
            },
        ))
        .unwrap();
    actor.send(Box::new(4u64)).await.unwrap();
    facade.internal_shutdown().await.unwrap();
    assert_eq!(handle.stats().copy_failed, 3);
    assert_eq!(
        copied.load(Ordering::SeqCst),
        0,
        "oversized copy allocated before checking its budget"
    );
    assert_eq!(*source.values.lock().unwrap(), vec![1, 2, 3, 4]);
    assert!(shadow.values.lock().unwrap().is_empty());
}

// Fault injection at the ActorRef admission boundary. The source is still a
// real Thread actor; this reference deliberately withholds the shadow receipt.
#[derive(Debug, Clone)]
struct GateRef {
    entered: Arc<tokio::sync::Notify>,
    mode: u8,
}
#[async_trait::async_trait]
impl parrot_api::address::ActorRef for GateRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(msg)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.entered.notify_one();
            match self.mode {
                0 => std::future::pending().await,
                1 => Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "admission refused".into(),
                )),
                _ => panic!("admission panic"),
            }
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/gate/shadow".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> parrot_api::types::BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
async fn gate(facade: &ParrotActorSystem, mode: u8) -> Arc<tokio::sync::Notify> {
    let entered = Arc::new(tokio::sync::Notify::new());
    facade
        .register_prefix_handler(
            "/gate/",
            Arc::new(GateRef {
                entered: entered.clone(),
                mode,
            }),
        )
        .unwrap();
    entered
}
#[tokio::test]
async fn blocked_shadow_has_bounded_copies_and_shutdown_cancels_every_pending_job() {
    use parrot::mirror::MirrorOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (facade, _, source, _) = setup().await;
    let entered = gate(&facade, 0).await;
    let copies = Arc::new(AtomicUsize::new(0));
    let copied = copies.clone();
    facade
        .register_mirror_copier(MirrorCopier::new(
            |_: &u64| 8,
            move |v: &u64| {
                copied.fetch_add(1, Ordering::SeqCst);
                Ok(*v)
            },
        ))
        .unwrap();
    let handle = facade
        .register_mirror_policy_with_options(
            MirrorPolicy::new("/source/", "/gate/shadow"),
            MirrorOptions {
                capacity: 2,
                delivery_timeout: Duration::from_secs(60),
                ..Default::default()
            },
        )
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    for n in 1..102u64 {
        actor.send(Box::new(n)).await.unwrap();
    }
    assert_eq!(handle.stats().queued, 2);
    assert_eq!(handle.stats().full, 100);
    assert_eq!(
        copies.load(Ordering::SeqCst),
        2,
        "saturated worker still copied messages"
    );
    tokio::time::timeout(Duration::from_secs(2), handle.shutdown())
        .await
        .unwrap();
    assert_eq!(handle.stats().cancelled, 2);
    assert_eq!(handle.stats().admitted, 0);
    assert_eq!(source.values.lock().unwrap().len(), 102);
    facade.internal_shutdown().await.unwrap();
}
#[tokio::test]
async fn pending_byte_budget_is_reserved_before_copying() {
    use parrot::mirror::MirrorOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (facade, _, _, _) = setup().await;
    let entered = gate(&facade, 0).await;
    let copies = Arc::new(AtomicUsize::new(0));
    let copied = copies.clone();
    facade
        .register_mirror_copier(MirrorCopier::new(
            |_: &u64| 8,
            move |v: &u64| {
                copied.fetch_add(1, Ordering::SeqCst);
                Ok(*v)
            },
        ))
        .unwrap();
    let handle = facade
        .register_mirror_policy_with_options(
            MirrorPolicy::new("/source/", "/gate/shadow"),
            MirrorOptions {
                capacity: 4,
                max_message_bytes: 8,
                max_pending_bytes: 8,
                delivery_timeout: Duration::from_secs(60),
            },
        )
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    actor.send(Box::new(1u64)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    actor.send(Box::new(2u64)).await.unwrap();
    assert_eq!(copies.load(Ordering::SeqCst), 1);
    assert_eq!(handle.stats().copy_failed, 1);
    facade.internal_shutdown().await.unwrap();
    assert_eq!(handle.stats().cancelled, 1);
}
#[tokio::test]
async fn admission_timeout_error_and_panic_are_observable_without_failing_primary() {
    use parrot::mirror::MirrorOptions;
    for mode in 0..3 {
        let (facade, _, source, _) = setup().await;
        gate(&facade, mode).await;
        let handle = facade
            .register_mirror_policy_with_options(
                MirrorPolicy::new("/source/", "/gate/shadow"),
                MirrorOptions {
                    delivery_timeout: Duration::from_millis(50),
                    ..Default::default()
                },
            )
            .unwrap();
        let actor = facade
            .get_actor(&ActorPath::placeholder("/source/deep"))
            .await
            .unwrap();
        assert_eq!(
            *actor
                .send(Box::new(5u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            10
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while handle.stats().delivery_failed == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        facade.internal_shutdown().await.unwrap();
        assert_eq!(handle.stats().admitted, 0);
        assert_eq!(handle.stats().delivery_failed, 1);
        assert_eq!(*source.values.lock().unwrap(), vec![5]);
    }
}
#[tokio::test]
async fn facade_drop_cancels_worker_even_when_external_refs_and_handle_survive() {
    let (facade, engine, _, _) = setup().await;
    let entered = gate(&facade, 0).await;
    let handle = facade
        .register_mirror_policy_with_options(
            MirrorPolicy::new("/source/", "/gate/shadow"),
            parrot::mirror::MirrorOptions {
                delivery_timeout: Duration::from_secs(60),
                ..Default::default()
            },
        )
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    actor.send(Box::new(1u64)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    drop(facade);
    assert!(handle.is_closed());
    tokio::time::timeout(Duration::from_secs(2), async {
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(handle.stats().cancelled, 1);
    // Dropping the facade closes the mirror, not a still-owned engine.
    actor.send(Box::new(2u64)).await.unwrap();
    assert_eq!(handle.stats().cancelled, 2);
    engine.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn invalid_and_unconfigured_registrations_fail_and_worker_count_is_bounded() {
    use parrot::mirror::{MirrorError, MirrorOptions};
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    assert!(matches!(
        facade.register_mirror_policy(MirrorPolicy::new("/source/", "/shadow")),
        Err(MirrorError::NoCopier)
    ));
    facade
        .register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
        .unwrap();
    for (src, dst) in [
        ("", "/shadow"),
        ("/source/", "parrot://node/x"),
        ("/source/", "//shadow"),
        ("/x", "/x"),
        ("/source/", "/x\n"),
    ] {
        assert!(matches!(
            facade.register_mirror_policy(MirrorPolicy::new(src, dst)),
            Err(MirrorError::InvalidPolicy)
        ));
    }
    for options in [
        MirrorOptions {
            capacity: 0,
            ..Default::default()
        },
        MirrorOptions {
            max_pending_bytes: usize::MAX,
            ..Default::default()
        },
        MirrorOptions {
            delivery_timeout: Duration::ZERO,
            ..Default::default()
        },
    ] {
        assert!(matches!(
            facade.register_mirror_policy_with_options(
                MirrorPolicy::new("/source/", "/shadow"),
                options
            ),
            Err(MirrorError::InvalidOptions)
        ));
    }
    // No awaits: workers cannot finish while registrations consume the bound.
    for n in 0..64 {
        facade
            .register_mirror_policy(MirrorPolicy::new(format!("/source/{n}/"), "/shadow"))
            .unwrap();
    }
    assert!(matches!(
        facade.register_mirror_policy(MirrorPolicy::new("/overflow/", "/shadow")),
        Err(MirrorError::RegistryFull)
    ));
    facade.internal_shutdown().await.unwrap();
}

struct EchoAll;
impl Actor for EchoAll {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
#[tokio::test]
async fn unsupported_message_is_counted_and_original_owned_payload_is_unchanged() {
    let (facade, engine, _, shadow) = setup().await;
    engine
        .spawn_at(EchoAll, "/source/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/shadow"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/echo"))
        .await
        .unwrap();
    let value = String::from("a local-only message with no copier");
    let reply = actor.send(Box::new(value.clone())).await.unwrap();
    assert_eq!(*reply.downcast::<String>().unwrap(), value);
    facade.internal_shutdown().await.unwrap();
    assert_eq!(handle.stats().copy_failed, 1);
    assert_eq!(handle.stats().queued, 0);
    assert!(shadow.values.lock().unwrap().is_empty());
}
struct MutateVec(Arc<Mutex<Vec<u64>>>);
impl Actor for MutateVec {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let mut owned = *msg.downcast::<Vec<u64>>().unwrap();
            owned.push(99);
            *self.0.lock().unwrap() = owned;
            Ok(Box::new(()) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
#[tokio::test]
async fn owned_vector_copy_can_be_mutated_by_shadow_without_changing_primary_reply() {
    let (facade, engine, _, _) = setup().await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    engine
        .spawn_at(EchoAll, "/source/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();
    engine
        .spawn_at(
            MutateVec(observed.clone()),
            "/vec-shadow",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    facade
        .register_mirror_copier(MirrorCopier::cloned::<Vec<u64>>(|v| {
            std::mem::size_of::<Vec<u64>>() + v.len() * 8
        }))
        .unwrap();
    facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/vec-shadow"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/echo"))
        .await
        .unwrap();
    let reply = actor.send(Box::new(vec![1u64, 2])).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while observed.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*reply.downcast::<Vec<u64>>().unwrap(), vec![1, 2]);
    assert_eq!(*observed.lock().unwrap(), vec![1, 2, 99]);
    facade.internal_shutdown().await.unwrap();
}
#[test]
fn policy_registration_without_a_runtime_fails_explicitly() {
    use parrot::mirror::MirrorError;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let facade = rt
        .block_on(ParrotActorSystem::new(Default::default()))
        .unwrap();
    drop(rt);
    facade
        .register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
        .unwrap();
    assert!(matches!(
        facade.register_mirror_policy(MirrorPolicy::new("/source/", "/shadow")),
        Err(MirrorError::NoRuntime)
    ));
}
mod actix_shadow {
    use super::*;
    use parrot::actix as __parrot_engine;
    use parrot_api::actor::EngineContextHandle;
    #[derive(Debug, parrot_api_derive::ParrotActor)]
    #[ParrotActor(engine = "actix", async_handler = true)]
    struct ObserveActix {
        seen: Seen,
    }
    impl ObserveActix {
        async fn handle_message(
            &mut self,
            msg: BoxedMessage,
            _: &mut <Self as Actor>::Context,
        ) -> ActorResult<BoxedMessage> {
            let n = *msg.downcast::<u64>().unwrap();
            self.seen.values.lock().unwrap().push(n);
            self.seen.changed.notify_one();
            Ok(Box::new(n * 2))
        }
        fn handle_message_engine(
            &mut self,
            _: BoxedMessage,
            _: &mut <Self as Actor>::Context,
            _: EngineContextHandle,
        ) -> Option<ActorResult<BoxedMessage>> {
            None
        }
    }
    #[actix::test]
    async fn actix_primary_and_thread_shadow_share_the_same_mirror_contract() {
        let (facade, _, _, shadow) = setup().await;
        let source = Seen::default();
        let actix = __parrot_engine::ActixActorSystem::new().await.unwrap();
        let raw = actix
            .spawn_root_typed(
                ObserveActix {
                    seen: source.clone(),
                },
                EmptyConfig,
            )
            .await
            .unwrap();
        facade
            .register_actix_system("actix".into(), actix, false)
            .await
            .unwrap();
        let handle = facade
            .register_mirror_policy(MirrorPolicy::new("actix://", "/shadow"))
            .unwrap();
        let actor = facade
            .get_actor(&ActorPath::placeholder(raw.path()))
            .await
            .unwrap();
        assert_eq!(
            *actor
                .send(Box::new(8u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            16
        );
        assert!(wait_values(&shadow, 1).await);
        actor.stop().await.unwrap();
        assert!(!raw.is_alive().await);
        let target = facade
            .get_actor(&ActorPath::placeholder("/shadow"))
            .await
            .unwrap();
        assert_eq!(
            *target
                .send(Box::new(9u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            18
        );
        assert_eq!(handle.stats().attempted, 1);
        facade.internal_shutdown().await.unwrap();
        assert_eq!(*source.values.lock().unwrap(), vec![8]);
        assert_eq!(*shadow.values.lock().unwrap(), vec![8, 9]);
    }
    #[actix::test]
    async fn thread_primary_and_actix_shadow_deliver_once_and_stop_does_not_mirror() {
        let (facade, _, source, _) = setup().await;
        let shadow = Seen::default();
        let actix = __parrot_engine::ActixActorSystem::new().await.unwrap();
        let raw = actix
            .spawn_root_typed(
                ObserveActix {
                    seen: shadow.clone(),
                },
                EmptyConfig,
            )
            .await
            .unwrap();
        let path = raw.path();
        facade
            .register_actix_system("actix".into(), actix, false)
            .await
            .unwrap();
        let handle = facade
            .register_mirror_policy(MirrorPolicy::new("/source/", &path))
            .unwrap();
        let actor = facade
            .get_actor(&ActorPath::placeholder("/source/deep"))
            .await
            .unwrap();
        assert_eq!(
            *actor
                .send(Box::new(6u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            12
        );
        assert!(wait_values(&shadow, 1).await);
        actor.stop().await.unwrap();
        assert!(raw.is_alive().await);
        assert_eq!(handle.stats().attempted, 1);
        facade.internal_shutdown().await.unwrap();
        assert_eq!(*source.values.lock().unwrap(), vec![6]);
        assert_eq!(*shadow.values.lock().unwrap(), vec![6]);
    }
}

async fn wait_for_stats(
    handle: &parrot::mirror::MirrorHandle,
    condition: impl Fn(parrot::mirror::MirrorStats) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !condition(handle.stats()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn existing_source_reference_resolves_late_and_replaced_shadow_generations() {
    let (facade, engine, source, _) = setup().await;
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/late"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    actor.send(Box::new(1u64)).await.unwrap();
    wait_for_stats(&handle, |s| s.missing_target == 1).await;
    let first = Seen::default();
    engine
        .spawn_at(
            Observe(first.clone()),
            "/late",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    actor.send(Box::new(2u64)).await.unwrap();
    assert!(wait_values(&first, 1).await);
    engine.stop_actor("/late").await.unwrap();
    let second = Seen::default();
    engine
        .spawn_at(
            Observe(second.clone()),
            "/late",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    actor.send(Box::new(3u64)).await.unwrap();
    assert!(wait_values(&second, 1).await);
    facade.internal_shutdown().await.unwrap();
    assert_eq!(*source.values.lock().unwrap(), vec![1, 2, 3]);
    assert_eq!(*first.values.lock().unwrap(), vec![2]);
    assert_eq!(*second.values.lock().unwrap(), vec![3]);
}
#[tokio::test]
async fn shadow_inside_source_prefix_is_not_recursively_mirrored() {
    let (facade, _, source, shadow) = setup().await;
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/", "/shadow"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/deep"))
        .await
        .unwrap();
    actor.send(Box::new(1u64)).await.unwrap();
    assert!(wait_values(&shadow, 1).await);
    let target = facade
        .get_actor(&ActorPath::placeholder("/shadow"))
        .await
        .unwrap();
    target.send(Box::new(2u64)).await.unwrap();
    facade.internal_shutdown().await.unwrap();
    assert_eq!(handle.stats().attempted, 1);
    assert_eq!(*source.values.lock().unwrap(), vec![1]);
    assert_eq!(*shadow.values.lock().unwrap(), vec![1, 2]);
}
