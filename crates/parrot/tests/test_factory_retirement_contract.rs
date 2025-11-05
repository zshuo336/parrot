//! A stop receipt must include a constructor that is already executing.
use parrot::thread::{
    SupervisorStrategy, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Probe(Arc<AtomicUsize>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            assert!(!msg.is::<bool>(), "restart trigger");
            Ok(msg)
        })
    }
}
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    wake: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (_guard, result) = self
            .wake
            .wait_timeout_while(
                self.released.lock().unwrap(),
                Duration::from_secs(3),
                |released| !*released,
            )
            .unwrap();
        assert!(!result.timed_out(), "probe did not release constructor");
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
async fn blocked(gate: Arc<Gate>) -> (Arc<ThreadActorSystem>, BoxedActorRef, Arc<AtomicUsize>) {
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shutdown_timeout: Duration::from_millis(50),
        ..Default::default()
    });
    let drops = Arc::new(AtomicUsize::new(0));
    let disposed = drops.clone();
    let entered = gate.clone();
    let calls = AtomicUsize::new(0);
    let actor = sys
        .spawn_supervised(
            move || {
                if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                    gate.block();
                }
                Probe(disposed.clone())
            },
            "/factory",
            SupervisorStrategy::Restart {
                max_retries: 2,
                within: Duration::from_secs(30),
            },
        )
        .await
        .unwrap();
    actor.send(Box::new(())).await.unwrap();
    let _ = actor.send(Box::new(true)).await;
    tokio::time::timeout(Duration::from_secs(2), entered.entered.notified())
        .await
        .unwrap();
    (sys, actor.clone_boxed(), drops)
}
async fn disposed(sys: &ThreadActorSystem, drops: &AtomicUsize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while drops.load(Ordering::SeqCst) != 2 || sys.get_actor_ref("/factory").is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn stop_does_not_claim_constructor_retirement_early() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (sys, actor, drops) = blocked(gate.clone()).await;
    let result = actor.stop().await;
    let retained = sys.get_actor_ref("/factory").is_some();
    gate.release();
    disposed(&sys, &drops).await;
    sys.shutdown_internal().await.unwrap();
    assert!(
        result.is_err(),
        "stop returned success while constructor still executed"
    );
    assert!(retained, "path was released before factory output disposal");
}
#[tokio::test]
async fn shutdown_does_not_claim_constructor_retirement_early() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (sys, _, drops) = blocked(gate.clone()).await;
    let result = sys.shutdown_internal().await;
    let retained = sys.get_actor_ref("/factory").is_some();
    gate.release();
    disposed(&sys, &drops).await;
    assert!(
        result.is_err(),
        "shutdown returned success while constructor still executed"
    );
    assert!(retained, "shutdown forgot still-owned factory resources");
}
#[tokio::test]
async fn expired_restart_keeps_reservation_until_factory_output_disposal() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (sys, _, drops) = blocked(gate.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let retained = sys.get_actor_ref("/factory").is_some();
    gate.release();
    disposed(&sys, &drops).await;
    sys.shutdown_internal().await.unwrap();
    assert!(
        retained,
        "elapsed deadline is not a completed factory retirement"
    );
}

async fn gone(sys: &ThreadActorSystem, path: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.get_actor_ref(path).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn cancelled_initial_factory_retains_ownership_and_old_reference_cannot_stop_reuse() {
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    });
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let captured = gate.clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let disposed = drops.clone();
    let owner = sys.clone();
    let task = tokio::spawn(async move {
        owner
            .spawn_supervised(
                move || {
                    captured.block();
                    Probe(disposed.clone())
                },
                "/initial",
                SupervisorStrategy::Stop,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    let old = sys
        .get_actor_ref("/initial")
        .expect("pending factory must be registered");
    let duplicate_calls = Arc::new(AtomicUsize::new(0));
    let observed = duplicate_calls.clone();
    let duplicate = sys
        .spawn_supervised(
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                Probe(Arc::new(AtomicUsize::new(0)))
            },
            "/initial",
            SupervisorStrategy::Stop,
        )
        .await;
    assert!(matches!(
        duplicate,
        Err(parrot::thread::SpawnError::ActorPathAlreadyExists(_))
    ));
    assert_eq!(duplicate_calls.load(Ordering::SeqCst), 0);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(old.stop().await.is_err());
    let retained = sys.get_actor_ref("/initial").is_some();
    gate.release();
    gone(&sys, "/initial").await;
    assert!(retained);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let replacement = sys
        .spawn_at(Probe(drops.clone()), "/initial", None, Default::default())
        .await
        .unwrap();
    assert!(old.stop().await.is_err());
    replacement.send(Box::new(())).await.unwrap();
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_and_restart_constructors_share_a_system_concurrency_budget() {
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 2,
        shutdown_timeout: Duration::from_secs(2),
        ..Default::default()
    });
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    // Create the business actor before saturating creation capacity. The
    // assertion below concerns scheduling of admitted actors; a new instance
    // now correctly shares bounded publication with the queued factories.
    let neighbour = sys
        .spawn_at(
            Probe(Arc::new(AtomicUsize::new(0))),
            "/neighbour",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    // Occupy one permit with a restart and one with an initial constructor.
    let restarted = sys
        .spawn_supervised(
            {
                let gate = gate.clone();
                let calls = calls.clone();
                let drops = drops.clone();
                let generation = AtomicUsize::new(0);
                move || {
                    if generation.fetch_add(1, Ordering::SeqCst) > 0 {
                        calls.fetch_add(1, Ordering::SeqCst);
                        gate.block();
                    }
                    Probe(drops.clone())
                }
            },
            "/restart",
            SupervisorStrategy::Restart {
                max_retries: 2,
                within: Duration::from_secs(30),
            },
        )
        .await
        .unwrap();
    restarted.send(Box::new(())).await.unwrap();
    let _ = restarted.send(Box::new(true)).await;
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    for index in 0..5 {
        let owner = sys.clone();
        let gate = gate.clone();
        let calls = calls.clone();
        let drops = drops.clone();
        tasks.push(tokio::spawn(async move {
            owner
                .spawn_supervised(
                    move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        gate.block();
                        Probe(drops.clone())
                    },
                    &format!("/initial-{index}"),
                    SupervisorStrategy::Stop,
                )
                .await
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.actor_count() != 7 || calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Business scheduling remains live while all constructor permits are held.
    neighbour
        .send_with_timeout(Box::new(()), Some(Duration::from_millis(200)))
        .await
        .unwrap();
    let running = calls.load(Ordering::SeqCst);
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        assert!(task.await.unwrap_err().is_cancelled());
    }
    let stop = tokio::spawn(async move { restarted.stop().await });
    gate.release();
    stop.await.unwrap().unwrap();
    for index in 0..5 {
        gone(&sys, &format!("/initial-{index}")).await;
    }
    assert_eq!(running, 2, "constructors must share a system-wide bound");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "cancelled queued factories must never run"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    sys.shutdown_internal().await.unwrap();
}

struct CaptureDrop {
    gate: Option<Arc<Gate>>,
    panic: bool,
}
impl Drop for CaptureDrop {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        assert!(!self.panic, "factory capture destructor panic");
    }
}
#[tokio::test]
async fn accepted_factory_capture_disposal_is_part_of_stop_retirement() {
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    });
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let capture = CaptureDrop {
        gate: Some(gate.clone()),
        panic: false,
    };
    let drops = Arc::new(AtomicUsize::new(0));
    let disposed = drops.clone();
    let actor = sys
        .spawn_supervised(
            move || {
                let _ = &capture;
                Probe(disposed.clone())
            },
            "/capture",
            SupervisorStrategy::Stop,
        )
        .await
        .unwrap();
    actor.send(Box::new(())).await.unwrap();
    let stop = tokio::spawn(async move { actor.stop().await });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    let result = stop.await.unwrap();
    let retained = sys.get_actor_ref("/capture").is_some();
    gate.release();
    gone(&sys, "/capture").await;
    assert!(result.is_err());
    assert!(retained);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn factory_capture_destructor_panic_is_reported_and_does_not_strand_registration() {
    let sys = ThreadActorSystem::shared(Default::default());
    let capture = CaptureDrop {
        gate: None,
        panic: true,
    };
    let actor = sys
        .spawn_supervised(
            move || {
                let _ = &capture;
                Probe(Arc::new(AtomicUsize::new(0)))
            },
            "/capture",
            SupervisorStrategy::Stop,
        )
        .await
        .unwrap();
    actor.send(Box::new(())).await.unwrap();
    assert!(
        actor
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("factory capture disposal failed")
    );
    gone(&sys, "/capture").await;
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn rejected_capture_disposal_survives_waiter_cancellation_without_blocking_registry() {
    struct Captured {
        gate: Arc<Gate>,
        disposed: Arc<tokio::sync::Notify>,
    }
    impl Drop for Captured {
        fn drop(&mut self) {
            self.gate.block();
            self.disposed.notify_one();
        }
    }
    let sys = ThreadActorSystem::shared(Default::default());
    let actor = sys
        .spawn_at(Probe(Arc::default()), "/occupied", None, Default::default())
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let disposed = Arc::new(tokio::sync::Notify::new());
    let capture = Captured {
        gate: gate.clone(),
        disposed: disposed.clone(),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let owner = sys.clone();
    let waiter = tokio::spawn(async move {
        owner
            .spawn_supervised(
                move || {
                    let _ = &capture;
                    observed.fetch_add(1, Ordering::SeqCst);
                    Probe(Arc::default())
                },
                "/occupied",
                SupervisorStrategy::Stop,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert!(!waiter.is_finished(), "rejection preceded capture disposal");
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_eq!(sys.actor_paths(), vec!["/occupied"]);
    actor
        .send_with_timeout(Box::new(()), Some(Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), disposed.notified())
        .await
        .unwrap();
    sys.shutdown_internal().await.unwrap();
}

struct Output {
    drop_gate: Option<Arc<Gate>>,
    initialized: Arc<AtomicUsize>,
    disposed: Arc<AtomicUsize>,
    panic_drop: bool,
}
impl Drop for Output {
    fn drop(&mut self) {
        if let Some(gate) = &self.drop_gate {
            gate.block();
        }
        self.disposed.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panic_drop,
            "cancelled factory output destructor panic"
        );
    }
}
impl Actor for Output {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.initialized.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(msg) })
    }
}
#[tokio::test]
async fn cancelled_initial_output_is_disposed_before_reservation_release() {
    for panic_drop in [false, true] {
        let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shutdown_timeout: Duration::from_millis(100),
            ..Default::default()
        });
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let drop_gate = Arc::new(Gate::default());
        let _drop_release = Release(drop_gate.clone());
        let initialized = Arc::new(AtomicUsize::new(0));
        let disposed = Arc::new(AtomicUsize::new(0));
        let task = {
            let owner = sys.clone();
            let gate = gate.clone();
            let drop_gate = drop_gate.clone();
            let initialized = initialized.clone();
            let disposed = disposed.clone();
            tokio::spawn(async move {
                owner
                    .spawn_supervised(
                        move || {
                            gate.block();
                            Output {
                                drop_gate: if panic_drop {
                                    None
                                } else {
                                    Some(drop_gate.clone())
                                },
                                initialized: initialized.clone(),
                                disposed: disposed.clone(),
                                panic_drop,
                            }
                        },
                        "/output",
                        SupervisorStrategy::Stop,
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let stop = sys.stop_actor("/output");
        tokio::pin!(stop);
        assert!(futures::poll!(&mut stop).is_pending());
        gate.release();
        if !panic_drop {
            tokio::time::timeout(Duration::from_secs(2), drop_gate.entered.notified())
                .await
                .unwrap();
        }
        let result = stop.await;
        let retained = sys.get_actor_ref("/output").is_some();
        drop_gate.release();
        gone(&sys, "/output").await;
        assert_eq!(initialized.load(Ordering::SeqCst), 0);
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
        if panic_drop {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("output disposal panicked")
            );
        } else {
            assert!(result.is_err());
            assert!(retained);
        }
        sys.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn initial_factory_deadline_and_shutdown_retain_pending_construction() {
    for shutdown in [false, true] {
        let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shutdown_timeout: Duration::from_millis(100),
            ..Default::default()
        });
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let drops = Arc::new(AtomicUsize::new(0));
        let task = {
            let owner = sys.clone();
            let gate = gate.clone();
            let drops = drops.clone();
            tokio::spawn(async move {
                owner
                    .spawn_supervised(
                        move || {
                            gate.block();
                            Probe(drops.clone())
                        },
                        "/initial",
                        SupervisorStrategy::Stop,
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        if shutdown {
            assert!(sys.shutdown_internal().await.is_err());
        }
        let result = task.await.unwrap();
        let retained = sys.get_actor_ref("/initial").is_some();
        gate.release();
        gone(&sys, "/initial").await;
        assert!(matches!(
            result,
            Err(parrot::thread::SpawnError::Timeout(_))
        ));
        assert!(retained);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        if !shutdown {
            sys.shutdown_internal().await.unwrap();
        }
    }
}

#[tokio::test]
async fn pending_reference_stop_cancels_only_its_creation_not_the_system() {
    let sys = ThreadActorSystem::shared(Default::default());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let task = {
        let owner = sys.clone();
        let gate = gate.clone();
        tokio::spawn(async move {
            owner
                .spawn_supervised(
                    move || {
                        gate.block();
                        Probe(Arc::new(AtomicUsize::new(0)))
                    },
                    "/pending",
                    SupervisorStrategy::Stop,
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    let pending = sys.get_actor_ref("/pending").unwrap();
    let stop = pending.stop();
    tokio::pin!(stop);
    assert!(futures::poll!(&mut stop).is_pending());
    gate.release();
    stop.await.unwrap();
    assert!(matches!(
        task.await.unwrap(),
        Err(parrot::thread::SpawnError::Cancelled)
    ));
    let healthy = sys
        .spawn_at(
            Probe(Arc::new(AtomicUsize::new(0))),
            "/healthy",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    healthy.send(Box::new(())).await.unwrap();
    sys.shutdown_internal().await.unwrap();
}
