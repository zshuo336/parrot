//! Live failures must select a policy before destroying actor state.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorSpawnerExt},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Fail(bool);
struct Make;
struct Hold {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
macro_rules! actors {
    ($child:ident,$parent:ident,$ctx:ty,$pctx:ty) => {
        struct $child {
            count: usize,
        }
        impl ActixEngineExt for $child {}
        impl Actor for $child {
            type Config = EmptyConfig;
            type Context = $ctx;
            fn can_batch_message(&self, msg: &BoxedMessage) -> bool {
                msg.is::<Fail>()
            }
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
                Box::pin(async move {
                    if let Some(hold) = msg.downcast_ref::<Hold>() {
                        hold.entered.notify_one();
                        hold.release.notified().await;
                        return Ok(Box::new(()) as BoxedMessage);
                    }
                    if msg.is::<Make>() {
                        return Ok(Box::new(
                            ctx.spawner()
                                .spawn_typed($child { count: 40 }, EmptyConfig)
                                .await?,
                        ) as BoxedMessage);
                    }
                    if let Some(f) = msg.downcast_ref::<Fail>() {
                        self.count += 1;
                        if f.0 {
                            panic!("live failure");
                        }
                        return Err(ActorError::MessageHandlingError("live failure".into()));
                    }
                    Ok(Box::new(self.count) as BoxedMessage)
                })
            }
        }
        struct $parent {
            calls: Arc<AtomicUsize>,
            decision: SupervisionDecision,
        }
        impl ActixEngineExt for $parent {}
        impl Actor for $parent {
            type Config = EmptyConfig;
            type Context = $pctx;
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
                    let calls = self.calls.clone();
                    let decision = self.decision;
                    ctx.set_supervisor_strategy(SupervisorStrategyType::OneForAll(
                        OneForAllStrategy {
                            max_restarts: 0,
                            within: Duration::from_secs(30),
                            decider: BasicDecisionFn::new(move |e| {
                                assert!(matches!(
                                    e,
                                    ActorError::Panic(_) | ActorError::MessageHandlingError(_)
                                ));
                                calls.fetch_add(1, Ordering::SeqCst);
                                decision
                            }),
                        },
                    ));
                    let mut children = Vec::<BoxedActorRef>::new();
                    for _ in 0..2 {
                        children.push(
                            ctx.spawner()
                                .spawn_typed($child { count: 0 }, EmptyConfig)
                                .await?,
                        );
                    }
                    Ok(Box::new(children) as BoxedMessage)
                })
            }
        }
    };
}
actors!(
    ThreadChild,
    ThreadParent,
    parrot::thread::ThreadContext<Self>,
    parrot::thread::ThreadContext<Self>
);
actors!(
    ActixChild,
    ActixParent,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
async fn contract(actix: bool, panic: bool, resume: bool) {
    let calls = Arc::new(AtomicUsize::new(0));
    let decision = if resume {
        SupervisionDecision::Resume
    } else {
        SupervisionDecision::Stop
    };
    let ts = parrot::thread::ThreadActorSystem::shared(Default::default());
    let axs = parrot::actix::ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
        arbiter_count: 1,
        ..Default::default()
    })
    .await
    .unwrap();
    let parent: BoxedActorRef = if actix {
        axs.spawn_root_typed(
            ActixParent {
                calls: calls.clone(),
                decision,
            },
            EmptyConfig,
        )
        .await
        .unwrap()
    } else {
        Box::new(
            ts.spawn_at(
                ThreadParent {
                    calls: calls.clone(),
                    decision,
                },
                "/parent",
                None,
                Default::default(),
            )
            .await
            .unwrap(),
        )
    };
    let children = *parent
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<Vec<BoxedActorRef>>()
        .unwrap();
    let grandchild = *children[0]
        .send(Box::new(Make))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    let failed = children[0].send(Box::new(Fail(panic))).await;
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        if resume {
            for expected in 1..=3 {
                assert_eq!(
                    *children[0]
                        .send(Box::new(()))
                        .await
                        .unwrap()
                        .downcast::<usize>()
                        .unwrap(),
                    expected
                );
                assert_eq!(
                    *children[1]
                        .send(Box::new(()))
                        .await
                        .unwrap()
                        .downcast::<usize>()
                        .unwrap(),
                    0
                );
                assert_eq!(
                    *grandchild
                        .send(Box::new(()))
                        .await
                        .unwrap()
                        .downcast::<usize>()
                        .unwrap(),
                    40
                );
                if expected < 3 {
                    assert!(children[0].send(Box::new(Fail(panic))).await.is_err());
                }
            }
        } else {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            loop {
                let child = children[0].send(Box::new(())).await;
                let sibling = children[1].send(Box::new(())).await;
                if child.is_err() && sibling.is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
    })
    .await;
    let _ = ts.shutdown_internal().await;
    let _ = axs.shutdown().await;
    assert!(
        matches!(
            failed,
            Err(ActorError::Panic(_)) | Err(ActorError::MessageHandlingError(_))
        ),
        "failure reply lost"
    );
    result.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), if resume { 3 } else { 1 });
}
macro_rules! case {
    ($name:ident,$a:expr,$p:expr,$r:expr) => {
        #[actix::test]
        async fn $name() {
            contract($a, $p, $r).await;
        }
    };
}
case!(thread_resume_panic, false, true, true);
case!(thread_resume_error, false, false, true);
case!(actix_resume_panic, true, true, true);
case!(actix_resume_error, true, false, true);
case!(thread_stop_error, false, false, false);
case!(actix_stop_error, true, false, false);

#[actix::test]
async fn root_factories_resume_without_reconstruction_and_restart_business_errors_once() {
    for actix in [false, true] {
        for resume in [false, true] {
            let made = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let ts = parrot::thread::ThreadActorSystem::shared(Default::default());
            let axs = parrot::actix::ActixActorSystem::with_settings(Default::default())
                .await
                .unwrap();
            let n = made.clone();
            let c = calls.clone();
            let actor: BoxedActorRef = if actix {
                let decider = BasicDecisionFn::new(move |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                    if resume {
                        SupervisionDecision::Resume
                    } else {
                        SupervisionDecision::Restart
                    }
                });
                axs.spawn_root_factory_with_strategy(
                    move || {
                        n.fetch_add(1, Ordering::SeqCst);
                        ActixChild { count: 0 }
                    },
                    parrot_api::supervisor::OneForOneStrategy {
                        max_restarts: 1,
                        within: Duration::from_secs(60),
                        decider,
                    },
                )
                .await
                .unwrap()
            } else {
                Box::new(
                    ts.spawn_supervised(
                        move || {
                            n.fetch_add(1, Ordering::SeqCst);
                            ThreadChild { count: 0 }
                        },
                        "/factory",
                        if resume {
                            parrot::thread::SupervisorStrategy::Resume
                        } else {
                            parrot::thread::SupervisorStrategy::Restart {
                                max_retries: 1,
                                within: Duration::from_secs(60),
                            }
                        },
                    )
                    .await
                    .unwrap(),
                )
            };
            actor.send(Box::new(())).await.unwrap();
            let identity = actor.death_watch().unwrap().id();
            assert!(actor.send(Box::new(Fail(false))).await.is_err());
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Ok(reply) = actor.send(Box::new(())).await {
                        let count = *reply.downcast::<usize>().unwrap();
                        if count == usize::from(resume) {
                            break;
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(made.load(Ordering::SeqCst), if resume { 1 } else { 2 });
            assert_eq!(actor.death_watch().unwrap().id(), identity);
            if actix {
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
            if !resume {
                assert!(actor.send(Box::new(Fail(false))).await.is_err());
                tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        let exists = if actix {
                            axs.get_actor(&actor.path()).await.is_some()
                        } else {
                            ts.get_actor_ref(&actor.path()).is_some()
                        };
                        if !exists {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(made.load(Ordering::SeqCst), 2);
                if actix {
                    assert_eq!(calls.load(Ordering::SeqCst), 2);
                }
            }
            ts.shutdown_internal().await.unwrap();
            axs.shutdown().await.unwrap();
        }
    }
}

struct SyncChild(usize);
impl ActixEngineExt for SyncChild {
    fn receive_message_with_engine(
        &mut self,
        msg: BoxedMessage,
        _: &mut Self::Context,
        _: parrot_api::actor::EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        if let Some(f) = msg.downcast_ref::<Fail>() {
            self.0 += 1;
            assert!(!f.0, "sync failure");
            return Some(Err(ActorError::MessageHandlingError("sync failure".into())));
        }
        Some(Ok(Box::new(self.0)))
    }
}
impl Actor for SyncChild {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { panic!("async path was not requested") })
    }
}
#[actix::test]
async fn actix_sync_resume_catches_panics_and_errors_without_changing_dispatch_path() {
    let sys = parrot::actix::ActixActorSystem::with_settings(Default::default())
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let actor = sys
        .spawn_root_factory_with_strategy(
            || SyncChild(0),
            parrot_api::supervisor::OneForOneStrategy {
                max_restarts: 0,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(move |_| {
                    n.fetch_add(1, Ordering::SeqCst);
                    SupervisionDecision::Resume
                }),
            },
        )
        .await
        .unwrap();
    for (count, panic) in [false, true, false].into_iter().enumerate() {
        assert!(actor.send(Box::new(Fail(panic))).await.is_err());
        assert_eq!(
            *actor
                .send(Box::new(()))
                .await
                .unwrap()
                .downcast::<usize>()
                .unwrap(),
            count + 1
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    sys.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_resume_preserves_admitted_queue_on_every_scheduler_and_batch_path() {
    use parrot::thread::{SchedulingMode, SupervisorStrategy, ThreadActorConfig};
    use parrot_api::address::ActorRef;
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 8,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "live".into(),
        },
    ] {
        for batch in [1, 4] {
            let sys = parrot::thread::ThreadActorSystem::shared(Default::default());
            let actor = sys
                .spawn_at(
                    ThreadChild { count: 0 },
                    "/queue",
                    None,
                    ThreadActorConfig {
                        scheduling_mode: Some(mode.clone()),
                        message_batch_size: Some(batch),
                        supervisor_strategy: Some(SupervisorStrategy::Resume),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let mut hold = actor.send(Box::new(Hold {
                entered: entered.clone(),
                release: release.clone(),
            }));
            assert!(futures::poll!(&mut hold).is_pending());
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            let mut requests: Vec<_> = (0..3).map(|_| actor.send(Box::new(Fail(false)))).collect();
            for request in &mut requests {
                assert!(futures::poll!(request).is_pending());
            }
            let mut barrier = actor.send(Box::new(()));
            assert!(futures::poll!(&mut barrier).is_pending());
            release.notify_one();
            hold.await.unwrap();
            for result in futures::future::join_all(requests).await {
                assert!(matches!(result, Err(ActorError::MessageHandlingError(_))));
            }
            assert_eq!(*barrier.await.unwrap().downcast::<usize>().unwrap(), 3);
            actor.deliver(Box::new(Fail(true))).await.unwrap();
            assert_eq!(
                *actor
                    .send(Box::new(()))
                    .await
                    .unwrap()
                    .downcast::<usize>()
                    .unwrap(),
                4
            );
            sys.shutdown_internal().await.unwrap();
        }
    }
}
#[actix::test]
async fn root_instance_resume_policy_is_available_without_a_factory() {
    let sys = parrot::actix::ActixActorSystem::with_settings(Default::default())
        .await
        .unwrap();
    let actor = sys
        .spawn_root_typed_with_strategy(
            ActixChild { count: 0 },
            EmptyConfig,
            SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    for (index, panic) in [false, true].into_iter().enumerate() {
        assert!(actor.send(Box::new(Fail(panic))).await.is_err());
        assert_eq!(
            *actor
                .send(Box::new(()))
                .await
                .unwrap()
                .downcast::<usize>()
                .unwrap(),
            index + 1
        );
    }
    sys.shutdown().await.unwrap();
}
#[derive(Default)]
struct DisposalGate {
    entered: tokio::sync::Notify,
    release: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
    disposed: std::sync::atomic::AtomicBool,
}
struct PolicyCapture(Arc<DisposalGate>);
impl Drop for PolicyCapture {
    fn drop(&mut self) {
        self.0.entered.notify_one();
        let mut open = self.0.release.lock().unwrap();
        while !*open {
            open = self.0.wake.wait(open).unwrap();
        }
        self.0.disposed.store(true, Ordering::SeqCst);
    }
}
struct OpenGate(Arc<DisposalGate>);
impl Drop for OpenGate {
    fn drop(&mut self) {
        *self.0.release.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}
#[actix::test]
async fn rejected_root_instance_retains_policy_disposal_after_waiter_cancellation() {
    let sys = parrot::actix::ActixActorSystem::with_settings(Default::default())
        .await
        .unwrap();
    sys.clone().shutdown().await.unwrap();
    let gate = Arc::new(DisposalGate::default());
    let release = OpenGate(gate.clone());
    let captured = PolicyCapture(gate.clone());
    let policy = SupervisorStrategyType::OneForOne(parrot_api::supervisor::OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(1),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &captured;
            SupervisionDecision::Resume
        }),
    });
    let mut creation =
        Box::pin(sys.spawn_root_typed_with_strategy(ActixChild { count: 0 }, EmptyConfig, policy));
    assert!(futures::poll!(&mut creation).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    drop(creation);
    assert!(!gate.disposed.load(Ordering::SeqCst));
    drop(release);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !gate.disposed.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
