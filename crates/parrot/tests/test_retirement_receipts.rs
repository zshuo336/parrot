use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorRef, RetirementStatus, stop_for_cleanup},
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalTypedDispatch},
    message::Message,
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Gate {
    release: (Mutex<bool>, Condvar),
    entered: AtomicBool,
    exited: AtomicUsize,
    expired: AtomicBool,
    fail: bool,
}
impl Gate {
    fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            release: (Mutex::new(false), Condvar::new()),
            entered: AtomicBool::new(false),
            exited: AtomicUsize::new(0),
            expired: AtomicBool::new(false),
            fail,
        })
    }
    fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
    fn dispose(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let (guard, timeout) = self
            .release
            .1
            .wait_timeout_while(
                self.release.0.lock().unwrap(),
                Duration::from_secs(15),
                |ready| !*ready,
            )
            .unwrap();
        drop(guard);
        self.expired.store(timeout.timed_out(), Ordering::SeqCst);
        self.exited.fetch_add(1, Ordering::SeqCst);
        assert!(!self.fail, "late runtime actor disposal failure");
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Ready;
impl Message for Ready {
    type Result = u64;
}
struct ThreadProbe(Arc<Gate>);
impl Actor for ThreadProbe {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(Box::new(37u64) as BoxedMessage) })
    }
}
impl Drop for ThreadProbe {
    fn drop(&mut self) {
        self.0.dispose();
    }
}
struct ActixProbe(Arc<Gate>);
impl Actor for ActixProbe {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(Box::new(37u64) as BoxedMessage) })
    }
}
impl parrot_api::actor::ActixEngineExt for ActixProbe {}
impl Drop for ActixProbe {
    fn drop(&mut self) {
        self.0.dispose();
    }
}
struct TypedProbe(Arc<Gate>);
impl ParrotTypedDispatch for TypedProbe {
    type Msg = Ready;
    type Reply = u64;
    fn dispatch(&mut self, _: Ready) -> BoxedFuture<'_, ActorResult<u64>> {
        Box::pin(async { Ok(37) })
    }
}
impl ParrotMsgVariant<TypedProbe> for Ready {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}
impl Drop for TypedProbe {
    fn drop(&mut self) {
        self.0.dispose();
    }
}
struct LocalProbe(Arc<Gate>, Rc<()>);
impl LocalTypedDispatch for LocalProbe {
    type Msg = Ready;
    type Reply = u64;
    fn dispatch<'a>(
        &'a mut self,
        _: Ready,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<u64>> {
        assert_eq!(Rc::strong_count(&self.1), 1);
        Box::pin(async { Ok(37) })
    }
}
impl ParrotMsgVariant<LocalProtocol<LocalProbe>> for Ready {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}
impl Drop for LocalProbe {
    fn drop(&mut self) {
        self.0.dispose();
    }
}

async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("actual retirement did not advance");
}
async fn contract(actor: BoxedActorRef, lookup: BoxedActorRef, gate: Arc<Gate>) {
    assert!(matches!(
        actor.retirement_status(),
        RetirementStatus::Pending
    ));
    assert!(matches!(
        lookup.retirement_status(),
        RetirementStatus::Pending
    ));
    let clone = actor.clone_boxed();
    let reply = actor.send(Box::new(Ready)).await.unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 37);
    let stopped = tokio::time::timeout(Duration::from_secs(5), actor.stop())
        .await
        .unwrap();
    assert!(
        matches!(
            stopped,
            Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
        ),
        "{stopped:?}"
    );
    wait_until(|| gate.entered.load(Ordering::SeqCst)).await;
    assert!(matches!(
        clone.retirement_status(),
        RetirementStatus::Pending
    ));
    assert!(matches!(
        lookup.retirement_status(),
        RetirementStatus::Pending
    ));
    assert!(stop_for_cleanup(&*actor).await.is_err());
    assert_eq!(gate.exited.load(Ordering::SeqCst), 0);
    gate.release();
    wait_until(|| matches!(clone.retirement_status(), RetirementStatus::Retired(_))).await;
    // Only this contract and its RAII release guard may retain the probe now.
    wait_until(|| Arc::strong_count(&gate) == 2).await;
    assert_eq!(gate.exited.load(Ordering::SeqCst), 1);
    assert!(
        !gate.expired.load(Ordering::SeqCst),
        "fixture watchdog cannot stand in for retirement"
    );
    for observer in [&*actor, &*clone, &*lookup] {
        let RetirementStatus::Retired(result) = observer.retirement_status() else {
            panic!("missing original receipt")
        };
        if gate.fail {
            let error = result.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("late runtime actor disposal failure"),
                "{error}"
            );
            assert!(stop_for_cleanup(observer).await.is_err());
        } else {
            result.unwrap();
            stop_for_cleanup(observer).await.unwrap();
        }
        assert!(matches!(
            observer.stop().await,
            Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
        ));
    }
}
async fn thread_contract(fail: bool) {
    let system =
        parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
            shutdown_timeout: Duration::from_millis(150),
            ..Default::default()
        });
    for kind in 0..6 {
        let gate = Gate::new(fail);
        let _release = Release(gate.clone());
        let path = format!("/user/retirement-{kind}");
        let actor: BoxedActorRef = match kind {
            0..=2 => Box::new(
                system
                    .spawn_at(
                        ThreadProbe(gate.clone()),
                        &path,
                        None,
                        parrot::thread::ThreadActorConfig {
                            scheduling_mode: Some(match kind {
                                0 => parrot::thread::SchedulingMode::SharedPool {
                                    max_messages_per_run: 8,
                                },
                                1 => parrot::thread::SchedulingMode::DedicatedThread,
                                _ => parrot::thread::SchedulingMode::Sharded {
                                    affinity_key: "retirement".into(),
                                },
                            }),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap(),
            ),
            3 => {
                let held = gate.clone();
                Box::new(
                    system
                        .spawn_supervised_with_config(
                            move || ThreadProbe(held.clone()),
                            &path,
                            Default::default(),
                        )
                        .await
                        .unwrap(),
                )
            }
            4 => system
                .spawn_typed::<TypedProbe, Ready>(TypedProbe(gate.clone()), &path)
                .await
                .unwrap()
                .into_dyn(),
            5 => {
                let held = gate.clone();
                system
                    .spawn_typed_factory::<TypedProbe, Ready, _>(
                        move || TypedProbe(held.clone()),
                        &path,
                    )
                    .await
                    .unwrap()
                    .into_dyn()
            }
            _ => unreachable!(),
        };
        let lookup = system.resolve_actor(&path).unwrap();
        contract(actor, lookup, gate).await;
    }
    let shutdown = tokio::time::timeout(Duration::from_secs(5), system.shutdown_internal())
        .await
        .unwrap();
    if !fail {
        shutdown.unwrap();
    }
}
async fn actix_contract(fail: bool) {
    let system = Arc::new(
        parrot::actix::ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: Duration::from_millis(150),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    for kind in 0..6 {
        let gate = Gate::new(fail);
        let _release = Release(gate.clone());
        let path = format!("/user/retirement-{kind}");
        let actor: BoxedActorRef = match kind {
            0 => system
                .spawn_root_typed(ActixProbe(gate.clone()), EmptyConfig)
                .await
                .unwrap(),
            1 => {
                let held = gate.clone();
                system
                    .spawn_root_factory(move || ActixProbe(held.clone()))
                    .await
                    .unwrap()
            }
            2 => system
                .spawn_typed::<TypedProbe, Ready>(TypedProbe(gate.clone()), &path)
                .await
                .unwrap()
                .into_dyn(),
            3 => {
                let held = gate.clone();
                system
                    .spawn_typed_factory::<TypedProbe, Ready, _>(
                        move || TypedProbe(held.clone()),
                        &path,
                    )
                    .await
                    .unwrap()
                    .into_dyn()
            }
            4 => {
                let held = gate.clone();
                system
                    .spawn_local_typed::<LocalProbe, Ready, _>(
                        move || LocalProbe(held.clone(), Rc::new(())),
                        &path,
                    )
                    .await
                    .unwrap()
                    .into_dyn()
            }
            5 => {
                let held = gate.clone();
                system
                    .spawn_local_typed_factory::<LocalProbe, Ready, _>(
                        move || LocalProbe(held.clone(), Rc::new(())),
                        &path,
                    )
                    .await
                    .unwrap()
                    .into_dyn()
            }
            _ => unreachable!(),
        };
        let lookup = system.resolve_actor(&actor.path()).unwrap();
        contract(actor, lookup, gate).await;
    }
    let shutdown = tokio::time::timeout(Duration::from_secs(5), system.as_ref().clone().shutdown())
        .await
        .unwrap();
    if !fail {
        shutdown.unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_actual_retirement_survives_public_timeout_across_all_local_views() {
    thread_contract(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_late_cleanup_failure_survives_public_timeout_across_all_local_views() {
    thread_contract(true).await;
}
#[test]
fn actix_actual_retirement_survives_public_timeout_across_all_local_views() {
    actix::System::new().block_on(actix_contract(false));
}
#[test]
fn actix_late_cleanup_failure_survives_public_timeout_across_all_local_views() {
    actix::System::new().block_on(actix_contract(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_factory_pending_lookup_keeps_original_receipt_after_publication() {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let construction = Gate::new(false);
    let _release = Release(construction.clone());
    let actor_gate = Gate::new(false);
    actor_gate.release();
    let spawn_system = system.clone();
    let held = construction.clone();
    let actor_held = actor_gate.clone();
    let spawn = tokio::spawn(async move {
        spawn_system
            .spawn_supervised_with_config(
                move || {
                    held.dispose();
                    ThreadProbe(actor_held.clone())
                },
                "/user/constructing-retirement",
                Default::default(),
            )
            .await
    });
    wait_until(|| construction.entered.load(Ordering::SeqCst)).await;
    let lookup = system
        .resolve_actor("/user/constructing-retirement")
        .unwrap();
    let clone = lookup.clone_boxed();
    assert!(matches!(
        lookup.retirement_status(),
        RetirementStatus::Pending
    ));
    construction.release();
    let actor = spawn.await.unwrap().unwrap();
    assert!(!construction.expired.load(Ordering::SeqCst));
    for observer in [&*lookup, &*clone] {
        assert!(matches!(
            observer.retirement_status(),
            RetirementStatus::Pending
        ));
    }
    let reply = actor.send(Box::new(Ready)).await.unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 37);
    actor.stop().await.unwrap();
    for observer in [&*lookup, &*clone] {
        assert!(matches!(
            observer.retirement_status(),
            RetirementStatus::Retired(Ok(()))
        ));
    }
    wait_until(|| Arc::strong_count(&construction) == 2).await;
    assert_eq!(actor_gate.exited.load(Ordering::SeqCst), 1);
    system.shutdown_internal().await.unwrap();
}

async fn facade_mirror_contract(fail: bool) {
    use parrot::{
        mirror::{MirrorCopier, MirrorPolicy},
        system::ParrotActorSystem,
    };
    use parrot_api::{address::ActorPath, system::ActorSystem};
    let system =
        parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
            shutdown_timeout: Duration::from_millis(150),
            ..Default::default()
        });
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    facade
        .register_thread_system("retirement-mirror".into(), system.clone(), true)
        .await
        .unwrap();
    facade
        .register_mirror_copier(MirrorCopier::new(|_: &Ready| 0, |_| Ok(Ready)))
        .unwrap();
    facade
        .register_mirror_policy(MirrorPolicy::new("/user/mirrored", "/user/shadow"))
        .unwrap();
    let gate = Gate::new(fail);
    let _release = Release(gate.clone());
    let original = system
        .spawn_at(
            ThreadProbe(gate.clone()),
            "/user/mirrored",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let mirrored = facade
        .get_actor(&ActorPath::placeholder("/user/mirrored"))
        .await
        .unwrap();
    contract(mirrored, Box::new(original), gate).await;
    let shutdown = facade.internal_shutdown().await;
    if !fail {
        shutdown.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mirrored_facade_reference_confirms_late_success_without_rewriting_timeout() {
    facade_mirror_contract(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mirrored_facade_reference_preserves_late_disposal_failure() {
    facade_mirror_contract(true).await;
}
