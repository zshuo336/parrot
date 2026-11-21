//! Actual managed roots and children distinguish admission from initialization.
use super::*;
use crate::actix::{ActixSystemSettings, ChildInstanceAdmissionFailure};
use crate::typed_context::TypedContextExt;
use parrot_api::{
    message::Message,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    typed::TypedLifecycle,
    types::BoxedFuture,
};
use std::{
    cell::Cell,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

#[derive(Default)]
struct Counts {
    initialized: AtomicUsize,
    actors: AtomicUsize,
    captures: AtomicUsize,
    rejected: AtomicUsize,
}
#[derive(Default)]
struct Gate {
    entered: Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(10), |open| {
                !*open
            })
            .unwrap();
        assert!(*open, "test must release destructor");
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
struct Capture {
    counts: Arc<Counts>,
    gate: Option<Arc<Gate>>,
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.counts.captures.fetch_add(1, Ordering::SeqCst);
    }
}
pub struct Read;
impl Message for Read {
    type Result = usize;
}
#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(Read), lifecycle)]
struct Probe {
    counts: Arc<Counts>,
    init: Option<Arc<Semaphore>>,
    disposal: Option<Arc<Gate>>,
    value: Box<Cell<usize>>,
    fail_init: bool,
    panic_drop: bool,
    create_children: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(gate) = &self.disposal {
            gate.block();
        }
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "static actor destructor receipt");
    }
}
impl TypedLifecycle for Probe {
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.counts.initialized.fetch_add(1, Ordering::SeqCst);
            if let Some(init) = &self.init {
                init.acquire().await.unwrap().forget();
            }
            if self.fail_init {
                Err(ActorError::InitializationError(
                    "static init receipt".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
    fn init_with_context<'a>(
        &'a mut self,
        context: &'a parrot_api::typed_context::TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            TypedLifecycle::init(self).await?;
            if self.create_children {
                let result = context
                    .spawn_typed_child::<_, Read>(probe(&self.counts))
                    .await;
                let ActorError::Other(error) = result
                    .err()
                    .expect("child must reject without waiting for parent slot")
                else {
                    panic!("recoverable instance error expected")
                };
                assert_eq!(
                    error
                        .downcast::<TypedInstanceAdmissionError<Probe>>()
                        .unwrap()
                        .reason,
                    ChildInstanceAdmissionFailure::Full
                );
                self.counts.rejected.fetch_add(1, Ordering::SeqCst);
                let invoked = Arc::new(AtomicUsize::new(0));
                let make = {
                    let invoked = invoked.clone();
                    let counts = self.counts.clone();
                    move || {
                        invoked.fetch_add(1, Ordering::SeqCst);
                        probe(&counts)
                    }
                };
                assert!(
                    context
                        .spawn_typed_child_factory::<_, Read, _>(make)
                        .await
                        .is_err()
                );
                assert_eq!(invoked.load(Ordering::SeqCst), 0);
                self.counts.rejected.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
    }
}
impl TypedReceive<Read> for Probe {
    fn receive_typed<'a>(&'a mut self, _: Read) -> BoxedFuture<'a, ActorResult<usize>> {
        Box::pin(async move { Ok(self.value.get()) })
    }
}
fn probe(counts: &Arc<Counts>) -> Probe {
    Probe {
        counts: counts.clone(),
        init: None,
        disposal: None,
        value: Box::new(Cell::new(42)),
        fail_init: false,
        panic_drop: false,
        create_children: false,
    }
}
fn policy(counts: &Arc<Counts>, gate: Option<Arc<Gate>>) -> SupervisorStrategyType {
    let capture = Capture {
        counts: counts.clone(),
        gate,
    };
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(3),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    })
}
async fn system(capacity: usize) -> Arc<ActixActorSystem> {
    let mut system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 2,
        shutdown_timeout: Duration::from_secs(3),
        ..Default::default()
    })
    .await
    .unwrap();
    system.instance_admission = Arc::new(Semaphore::new(capacity));
    Arc::new(system)
}
async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn context(system: &ActixActorSystem, path: &str) -> parrot_api::typed_context::TypedContext {
    system
        .registry
        .read()
        .unwrap()
        .actors
        .get(path)
        .unwrap()
        .control
        .typed_tree()
        .context()
}

#[actix::test]
async fn static_entries_hold_shared_slots_until_successful_initialization() {
    for entry in 0..5 {
        let system = system(1).await;
        let counts = Arc::new(Counts::default());
        let init = Arc::new(Semaphore::new(0));
        let make = {
            let counts = counts.clone();
            let init = init.clone();
            move || {
                let mut actor = probe(&counts);
                actor.init = Some(init.clone());
                actor
            }
        };
        let cohort = system.root_cohort(policy(&counts, None)).await.unwrap();
        let first = match entry {
            0 => system.spawn_typed::<_, Read>(make(), "/first").await,
            1 => {
                system
                    .spawn_typed_with_strategy::<_, Read>(make(), "/first", policy(&counts, None))
                    .await
            }
            2 => {
                system
                    .spawn_typed_in_cohort::<_, Read>(make(), "/first", &cohort)
                    .await
            }
            3 => {
                system
                    .spawn_typed_factory::<_, Read, _>(make, "/first")
                    .await
            }
            _ => {
                system
                    .spawn_typed_factory_in_cohort::<_, Read, _>(make, "/first", &cohort)
                    .await
            }
        }
        .unwrap();
        until(|| counts.initialized.load(Ordering::SeqCst) == 1).await;
        assert_eq!(
            system.instance_admission.available_permits(),
            0,
            "entry={entry}"
        );
        let clone = Arc::new(system.downgrade().upgrade().unwrap());
        let second_counts = Arc::new(Counts::default());
        let mut second = Box::pin(clone.spawn_typed::<_, Read>(probe(&second_counts), "/second"));
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(system.status().active_actors, 1);
        init.add_permits(1);
        assert_eq!(first.ask(Read).await.unwrap(), 42);
        let second = second.await.unwrap();
        assert_eq!(second.ask(Read).await.unwrap(), 42);
        until(|| system.instance_admission.available_permits() == 1).await;
        assert_eq!(
            system.status().active_actors,
            2,
            "successful init releases capacity while actors stay live"
        );
        second.stop().await.unwrap();
        first.stop().await.unwrap();
        cohort.stop().await.unwrap();
        system.as_ref().clone().shutdown().await.unwrap();
    }
}

#[actix::test]
async fn overloaded_static_child_returns_exact_send_only_inputs_for_retry() {
    let system = system(1).await;
    let parent = system
        .spawn_typed::<_, Read>(probe(&Arc::new(Counts::default())), "/parent")
        .await
        .unwrap();
    parent.ask(Read).await.unwrap();
    let context = context(&system, "/parent");
    for closed in [false, true] {
        let occupied = system
            .instance_admission
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        if closed {
            system.instance_admission.close();
        }
        let counts = Arc::new(Counts::default());
        let actor = probe(&counts);
        let address = actor.value.as_ref() as *const Cell<usize> as usize;
        let failure = context
            .spawn_typed_child_with_strategy::<_, Read>(actor, policy(&counts, None))
            .await
            .err()
            .unwrap();
        let ActorError::Other(error) = failure else {
            panic!("recoverable error expected")
        };
        let error = error
            .downcast::<TypedInstanceAdmissionError<Probe>>()
            .unwrap();
        assert_eq!(
            error.reason,
            if closed {
                ChildInstanceAdmissionFailure::ShuttingDown
            } else {
                ChildInstanceAdmissionFailure::Full
            }
        );
        let inputs = error.into_inputs();
        assert_eq!(
            inputs.actor.value.as_ref() as *const Cell<usize> as usize,
            address
        );
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
        assert_eq!(system.status().active_actors, 1);
        drop(occupied);
        if closed {
            drop(inputs);
        } else {
            let child = context
                .spawn_typed_child_with_strategy::<_, Read>(inputs.actor, inputs.strategy.unwrap())
                .await
                .unwrap();
            assert_eq!(child.ask(Read).await.unwrap(), 42);
            child.stop().await.unwrap();
        }
        assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    }
    parent.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn overloaded_static_factory_returns_callable_and_policy_without_invocation() {
    type Make = Box<dyn Fn() -> Probe + Send + Sync>;
    let system = system(1).await;
    let parent = system
        .spawn_typed::<_, Read>(probe(&Arc::new(Counts::default())), "/parent")
        .await
        .unwrap();
    parent.ask(Read).await.unwrap();
    let context = context(&system, "/parent");
    let occupied = system
        .instance_admission
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let invoked = Arc::new(AtomicUsize::new(0));
    let make: Make = {
        let invoked = invoked.clone();
        let counts = counts.clone();
        Box::new(move || {
            invoked.fetch_add(1, Ordering::SeqCst);
            probe(&counts)
        })
    };
    let address = make.as_ref() as *const _ as *const () as usize;
    let result = context
        .spawn_typed_child_factory_with_strategy::<_, Read, _>(make, policy(&counts, None))
        .await;
    let ActorError::Other(error) = result.err().unwrap() else {
        panic!("recoverable error expected")
    };
    let inputs = error
        .downcast::<FactoryAdmissionError<Make>>()
        .unwrap()
        .into_inputs();
    assert_eq!(
        inputs.factory.as_ref() as *const _ as *const () as usize,
        address
    );
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
    drop(occupied);
    let child = context
        .spawn_typed_child_factory_with_strategy::<_, Read, _>(
            inputs.factory,
            inputs.strategy.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(child.ask(Read).await.unwrap(), 42);
    assert_eq!(invoked.load(Ordering::SeqCst), 1);
    child.stop().await.unwrap();
    parent.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn initializing_static_parent_rejects_children_without_waiting_on_its_own_slot() {
    for factory in [false, true] {
        let system = system(1).await;
        let counts = Arc::new(Counts::default());
        let make = {
            let counts = counts.clone();
            move || {
                let mut actor = probe(&counts);
                actor.create_children = true;
                actor
            }
        };
        let parent = if factory {
            system
                .spawn_typed_factory::<_, Read, _>(make, "/parent-init")
                .await
                .unwrap()
        } else {
            system
                .spawn_typed::<_, Read>(make(), "/parent-init")
                .await
                .unwrap()
        };
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), parent.ask(Read))
                .await
                .unwrap()
                .unwrap(),
            42
        );
        assert_eq!(counts.rejected.load(Ordering::SeqCst), 2);
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
        assert_eq!(system.status().active_actors, 1);
        parent.stop().await.unwrap();
        system.as_ref().clone().shutdown().await.unwrap();
    }
}

#[actix::test]
async fn cancelled_duplicate_rejection_retains_slot_through_actor_and_policy_disposal() {
    let system = system(1).await;
    let first = system
        .spawn_typed::<_, Read>(probe(&Arc::new(Counts::default())), "/duplicate")
        .await
        .unwrap();
    first.ask(Read).await.unwrap();
    let counts = Arc::new(Counts::default());
    let actor_gate = Arc::new(Gate::default());
    let release_actor = Release(actor_gate.clone());
    let policy_gate = Arc::new(Gate::default());
    let release_policy = Release(policy_gate.clone());
    let mut actor = probe(&counts);
    actor.disposal = Some(actor_gate.clone());
    let mut creation = Box::pin(system.spawn_typed_with_strategy::<_, Read>(
        actor,
        "/duplicate",
        policy(&counts, Some(policy_gate.clone())),
    ));
    assert!(futures::poll!(&mut creation).is_pending());
    tokio::time::timeout(Duration::from_secs(5), actor_gate.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), policy_gate.entered.notified())
        .await
        .unwrap();
    drop(creation);
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(release_actor);
    until(|| counts.actors.load(Ordering::SeqCst) == 1).await;
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(release_policy);
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    first.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn failed_static_init_keeps_capacity_until_real_actor_and_capture_retirement() {
    for factory in [false, true] {
        let system = system(1).await;
        let counts = Arc::new(Counts::default());
        let actor_gate = Arc::new(Gate::default());
        let release_actor = Release(actor_gate.clone());
        let capture_gate = Arc::new(Gate::default());
        let release_capture = Release(capture_gate.clone());
        let make = {
            let counts = counts.clone();
            let gate = actor_gate.clone();
            move || {
                let mut actor = probe(&counts);
                actor.fail_init = true;
                actor.panic_drop = true;
                actor.disposal = Some(gate.clone());
                actor
            }
        };
        let reference = if factory {
            let capture = Capture {
                counts: counts.clone(),
                gate: Some(capture_gate.clone()),
            };
            system
                .spawn_typed_factory_with_strategy::<_, Read, _>(
                    move || {
                        let _ = &capture;
                        make()
                    },
                    "/failure",
                    OneForOneStrategy {
                        max_restarts: 0,
                        within: Duration::from_secs(3),
                        decider: BasicDecisionFn::new(|_| SupervisionDecision::Stop),
                    },
                )
                .await
                .unwrap()
        } else {
            system
                .spawn_typed_with_strategy::<_, Read>(
                    make(),
                    "/failure",
                    policy(&counts, Some(capture_gate.clone())),
                )
                .await
                .unwrap()
        };
        tokio::time::timeout(Duration::from_secs(5), actor_gate.entered.notified())
            .await
            .unwrap();
        assert_eq!(
            system.instance_admission.available_permits(),
            0,
            "init failure must not release capacity (factory={factory})"
        );
        drop(release_actor);
        tokio::time::timeout(Duration::from_secs(5), capture_gate.entered.notified())
            .await
            .unwrap();
        assert_eq!(
            system.instance_admission.available_permits(),
            0,
            "actual capture retirement still owns slot"
        );
        drop(release_capture);
        let error = reference.stop().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("static actor destructor receipt"),
            "{error:?}"
        );
        until(|| system.instance_admission.available_permits() == 1).await;
        assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
        let _ = system.as_ref().clone().shutdown().await;
    }
}

#[actix::test]
async fn ignored_guardian_policy_obeys_static_first_poll_deadline_without_late_publication() {
    for factory in [false, true] {
        let mut system = ActixActorSystem::with_guardian(
            ActixSystemSettings {
                arbiter_count: 2,
                shutdown_timeout: Duration::from_millis(150),
                ..Default::default()
            },
            parrot_api::system::GuardianConfig::default(),
        )
        .await
        .unwrap();
        system.instance_admission = Arc::new(Semaphore::new(1));
        let system = Arc::new(system);
        let counts = Arc::new(Counts::default());
        let gate = Arc::new(Gate::default());
        let release = Release(gate.clone());
        let make = {
            let counts = counts.clone();
            move || probe(&counts)
        };
        let mut creation: BoxedFuture<'_, ActorResult<TypedActorRef<Probe, Read>>> = if factory {
            let SupervisorStrategyType::OneForOne(strategy) = policy(&counts, Some(gate.clone()))
            else {
                unreachable!()
            };
            Box::pin(system.spawn_typed_factory_with_strategy(make, "/deadline", strategy))
        } else {
            Box::pin(system.spawn_typed_with_strategy(
                make(),
                "/deadline",
                policy(&counts, Some(gate.clone())),
            ))
        };
        assert!(futures::poll!(&mut creation).is_pending());
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        let outcome = tokio::time::timeout(Duration::from_millis(800), &mut creation)
            .await
            .unwrap();
        assert!(matches!(outcome, Err(ActorError::TimeoutDetail(_))));
        drop(creation);
        assert_eq!(system.status().active_actors, 0);
        assert_eq!(system.instance_admission.available_permits(), 0);
        drop(release);
        until(|| system.instance_admission.available_permits() == 1).await;
        assert_eq!(system.status().active_actors, 0);
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(counts.actors.load(Ordering::SeqCst), usize::from(!factory));
        system.as_ref().clone().shutdown().await.unwrap();
    }
}
