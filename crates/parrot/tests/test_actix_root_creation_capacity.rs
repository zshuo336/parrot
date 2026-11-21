//! Public business counterexample: pending initialization must bound root creation.
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext, ActixSystemSettings};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorConfig, ActorState},
    context::{ActorContext, ActorSpawnerExt},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

#[derive(Default)]
struct Counts {
    actors: AtomicUsize,
    configs: AtomicUsize,
    initialized: AtomicUsize,
}
struct Config(Arc<Counts>);
impl ActorConfig for Config {}
impl Drop for Config {
    fn drop(&mut self) {
        self.0.configs.fetch_add(1, Ordering::SeqCst);
    }
}
struct Probe {
    counts: Arc<Counts>,
    init: Arc<Semaphore>,
}
struct MakeChild(Arc<Counts>);

pub struct StaticRead;
impl parrot_api::message::Message for StaticRead {
    type Result = ();
}
#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(StaticRead))]
struct StaticProbe(Arc<Counts>);
impl Drop for StaticProbe {
    fn drop(&mut self) {
        self.0.actors.fetch_add(1, Ordering::SeqCst);
    }
}
impl parrot_api::typed::TypedReceive<StaticRead> for StaticProbe {
    fn receive_typed<'a>(&'a mut self, _: StaticRead) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for Probe {}
impl Actor for Probe {
    type Config = Config;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.initialized.fetch_add(1, Ordering::SeqCst);
            self.init.acquire().await.unwrap().forget();
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match message.downcast::<MakeChild>() {
                Ok(request) => {
                    let result = ctx
                        .spawner()
                        .spawn_typed(
                            Probe {
                                counts: request.0.clone(),
                                init: Arc::new(Semaphore::new(1)),
                            },
                            Config(request.0),
                        )
                        .await;
                    Ok(Box::new(result) as BoxedMessage)
                }
                Err(message) => Ok(message),
            }
        })
    }
}

#[actix::test]
async fn production_root_limit_bounds_initializing_instances_before_publication() {
    let system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 2,
        shutdown_timeout: Duration::from_secs(10),
        ..Default::default()
    })
    .await
    .unwrap();
    let counts = Arc::new(Counts::default());
    let init = Arc::new(Semaphore::new(0));
    let mut references = Vec::new();
    for _ in 0..1024 {
        references.push(
            system
                .spawn_root_typed(
                    Probe {
                        counts: counts.clone(),
                        init: init.clone(),
                    },
                    Config(counts.clone()),
                )
                .await
                .unwrap(),
        );
    }
    let mut pending = Box::pin(system.spawn_root_typed(
        Probe {
            counts: counts.clone(),
            init: init.clone(),
        },
        Config(counts.clone()),
    ));
    let first_poll = futures::poll!(&mut pending);
    let bounded = first_poll.is_pending();
    let registered = system.status().active_actors;
    // Release and retire everything before asserting, also on the old runtime
    // where this first poll has already published the overloaded root.
    init.add_permits(1025);
    let last = match first_poll {
        Poll::Ready(result) => result.unwrap(),
        Poll::Pending => pending.await.unwrap(),
    };
    last.send(Box::new(())).await.unwrap();
    references.push(last);
    for actor in references {
        actor.stop().await.unwrap();
    }
    system.clone().shutdown().await.unwrap();
    assert!(
        bounded,
        "1025th root bypassed the production initial-creation limit"
    );
    assert_eq!(
        registered, 1024,
        "overloaded root published before admission"
    );
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1025);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1025);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 1025);
}

#[actix::test]
async fn production_child_cannot_bypass_exhausted_root_creation_capacity() {
    use parrot_api::types::BoxedActorRef;
    let system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 2,
        shutdown_timeout: Duration::from_secs(10),
        ..Default::default()
    })
    .await
    .unwrap();
    let parent_counts = Arc::new(Counts::default());
    let parent = system
        .spawn_root_typed(
            Probe {
                counts: parent_counts.clone(),
                init: Arc::new(Semaphore::new(1)),
            },
            Config(parent_counts),
        )
        .await
        .unwrap();
    parent.send(Box::new(())).await.unwrap();
    let counts = Arc::new(Counts::default());
    let init = Arc::new(Semaphore::new(0));
    let mut roots = Vec::new();
    for _ in 0..1024 {
        roots.push(
            system
                .spawn_root_typed(
                    Probe {
                        counts: counts.clone(),
                        init: init.clone(),
                    },
                    Config(counts.clone()),
                )
                .await
                .unwrap(),
        );
    }
    let child_counts = Arc::new(Counts::default());
    let result = *parent
        .send(Box::new(MakeChild(child_counts.clone())))
        .await
        .unwrap()
        .downcast::<ActorResult<BoxedActorRef>>()
        .unwrap();
    let rejected = result.is_err();
    let registered = system.status().active_actors;
    init.add_permits(1024);
    match result {
        Ok(child) => child.stop().await.unwrap(),
        Err(error) => drop(error),
    }
    for actor in roots {
        actor.stop().await.unwrap();
    }
    parent.stop().await.unwrap();
    system.clone().shutdown().await.unwrap();
    assert_eq!(
        registered, 1025,
        "child published despite exhausted shared creation capacity"
    );
    assert!(
        rejected,
        "overloaded child request did not return its inputs"
    );
    assert_eq!(child_counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(child_counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(child_counts.configs.load(Ordering::SeqCst), 1);
}

async fn static_creation_capacity(factory: bool) {
    let system = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 2,
            shutdown_timeout: Duration::from_secs(10),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let counts = Arc::new(Counts::default());
    let init = Arc::new(Semaphore::new(0));
    let mut roots = Vec::new();
    for _ in 0..1024 {
        roots.push(
            system
                .spawn_root_typed(
                    Probe {
                        counts: counts.clone(),
                        init: init.clone(),
                    },
                    Config(counts.clone()),
                )
                .await
                .unwrap(),
        );
    }
    let static_counts = Arc::new(Counts::default());
    let make_actor = {
        let counts = static_counts.clone();
        move || StaticProbe(counts.clone())
    };
    let mut creation: BoxedFuture<'_, ActorResult<_>> = if factory {
        Box::pin(system.spawn_typed_factory::<_, StaticRead, _>(make_actor, "/static-capacity"))
    } else {
        Box::pin(system.spawn_typed::<_, StaticRead>(make_actor(), "/static-capacity"))
    };
    let first_poll = futures::poll!(&mut creation);
    let bounded = first_poll.is_pending();
    let registered = system.status().active_actors;
    init.add_permits(1024);
    let reference = match first_poll {
        Poll::Ready(result) => result.unwrap(),
        Poll::Pending => creation.await.unwrap(),
    };
    reference.ask(StaticRead).await.unwrap();
    reference.stop().await.unwrap();
    for actor in roots {
        actor.stop().await.unwrap();
    }
    system.as_ref().clone().shutdown().await.unwrap();
    assert_eq!(static_counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1024);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1024);
    assert_eq!(
        registered, 1024,
        "static root or factory published despite exhausted shared creation capacity (factory={factory})"
    );
    assert!(
        bounded,
        "static root bypassed admission (factory={factory})"
    );
}

#[actix::test]
async fn production_static_root_shares_dynamic_creation_capacity() {
    static_creation_capacity(false).await;
}

#[actix::test]
async fn production_static_factory_shares_dynamic_creation_capacity() {
    static_creation_capacity(true).await;
}

#[derive(Default)]
struct DisposalGate {
    entered: Notify,
    open: Mutex<bool>,
    changed: Condvar,
    disposed: AtomicUsize,
}
impl DisposalGate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct PolicyCapture(Arc<DisposalGate>);
impl Drop for PolicyCapture {
    fn drop(&mut self) {
        self.0.entered.notify_one();
        let (open, _) = self
            .0
            .changed
            .wait_timeout_while(
                self.0.open.lock().unwrap(),
                Duration::from_secs(10),
                |open| !*open,
            )
            .unwrap();
        assert!(*open, "test must release the policy destructor");
        self.0.disposed.fetch_add(1, Ordering::SeqCst);
    }
}
struct ReleasePolicy(Arc<DisposalGate>);
impl Drop for ReleasePolicy {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[actix::test]
async fn ignored_guardian_policy_cannot_extend_root_creation_deadline() {
    use parrot_api::{
        errors::ActorError,
        supervisor::{
            BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType,
        },
        system::{GuardianConfig, SystemError},
    };
    let system = ActixActorSystem::with_guardian(
        ActixSystemSettings {
            arbiter_count: 2,
            shutdown_timeout: Duration::from_millis(150),
            ..Default::default()
        },
        GuardianConfig::default(),
    )
    .await
    .unwrap();
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(DisposalGate::default());
    let release = ReleasePolicy(gate.clone());
    let capture = PolicyCapture(gate.clone());
    let strategy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(1),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    });
    let mut creation = Box::pin(system.spawn_root_typed_with_strategy(
        Probe {
            counts: counts.clone(),
            init: Arc::new(Semaphore::new(0)),
        },
        Config(counts.clone()),
        strategy,
    ));
    assert!(futures::poll!(&mut creation).is_pending());
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_millis(800), &mut creation).await;
    let returned_timeout = matches!(
        result,
        Ok(Err(SystemError::ActorError(ActorError::TimeoutDetail(_))))
    );
    let registered_before_release = system.status().active_actors;
    drop(creation);
    drop(release);
    tokio::time::timeout(Duration::from_secs(3), async {
        while counts.actors.load(Ordering::SeqCst) != 1
            || counts.configs.load(Ordering::SeqCst) != 1
            || gate.disposed.load(Ordering::SeqCst) != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let registered_after_release = system.status().active_actors;
    system.clone().shutdown().await.unwrap();
    assert!(
        returned_timeout,
        "ignored policy destructor extended the public creation deadline"
    );
    assert_eq!(registered_before_release, 0);
    assert_eq!(registered_after_release, 0);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
}
