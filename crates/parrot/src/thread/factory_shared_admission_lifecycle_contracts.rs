use super::*;
use parrot_api::supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision};
use std::sync::Condvar;

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(15), |open| {
                !*open
            })
            .unwrap();
        assert!(!timeout.timed_out(), "test must release destructor gate");
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
struct Tracked {
    gate: Option<Arc<Gate>>,
    drops: Arc<AtomicUsize>,
    panic: bool,
}
impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.block();
        }
        assert!(!self.panic, "shared factory capture destructor failed");
    }
}

#[tokio::test]
async fn cancelled_dynamic_factory_retains_capacity_until_real_capture_retirement() {
    for panic in [false, true] {
        let system = system();
        let worker = system.factory_slots.clone().acquire_owned().await.unwrap();
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Tracked {
            gate: Some(gate.clone()),
            drops: drops.clone(),
            panic,
        };
        let mut creation = Box::pin(system.spawn_supervised_with_config(
            move || {
                std::hint::black_box(&capture);
                Dynamic
            },
            "/cancelled-factory",
            Default::default(),
        ));
        assert!(futures::poll!(&mut creation).is_pending());
        let pending = match system
            .registry
            .read()
            .unwrap()
            .get("/cancelled-factory")
            .unwrap()
        {
            ActorRegistryEntry::Pending(pending) => pending.clone(),
            _ => panic!("factory was not pending"),
        };
        drop(creation);
        within(gate.entered.notified()).await;
        assert_eq!(system.instance_admission.available_permits(), 0);
        let mut next = Box::pin(system.spawn_supervised_with_config(
            || Dynamic,
            "/after-cancel",
            Default::default(),
        ));
        assert!(futures::poll!(&mut next).is_pending());
        assert!(
            !system
                .registry
                .read()
                .unwrap()
                .contains_key("/after-cancel")
        );
        gate.release();
        drop(worker);
        within(next).await.unwrap();
        recovered(&system).await;
        let status = within(crate::thread::system::await_stop(
            pending.retired.subscribe(),
        ))
        .await;
        if panic {
            assert!(
                status
                    .unwrap_err()
                    .to_string()
                    .contains("shared factory capture destructor failed")
            );
        } else {
            status.unwrap();
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        within(system.shutdown_internal()).await.unwrap();
    }
}

struct Failing(Tracked);
impl ParrotTypedDispatch for Failing {
    type Msg = Request;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        std::hint::black_box(&self.0);
        Box::pin(async {
            Err(ActorError::InitializationError(
                "shared factory init failed".into(),
            ))
        })
    }
    fn dispatch(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async { panic!("failed initialization must not dispatch") })
    }
}
impl ParrotMsgVariant<Failing> for Request {
    fn inject(message: Self) -> Request {
        message
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}

#[tokio::test]
async fn failed_static_factory_keeps_capacity_through_actor_capture_and_policy_disposal() {
    for stage in 0..3 {
        for panic in [false, true] {
            let system = system();
            let gate = Arc::new(Gate::default());
            let _release = Release(gate.clone());
            let actor_drops = Arc::new(AtomicUsize::new(0));
            let factory_drops = Arc::new(AtomicUsize::new(0));
            let policy_drops = Arc::new(AtomicUsize::new(0));
            let actor_gate = (stage == 0).then(|| gate.clone());
            let observed_actor_drops = actor_drops.clone();
            let capture = Tracked {
                gate: (stage == 1).then(|| gate.clone()),
                drops: factory_drops.clone(),
                panic: panic && stage == 1,
            };
            let policy = Tracked {
                gate: (stage == 2).then(|| gate.clone()),
                drops: policy_drops.clone(),
                panic: panic && stage == 2,
            };
            let reference = system
                .spawn_typed_factory_with_strategy::<_, Request, _>(
                    move || {
                        std::hint::black_box(&capture);
                        Failing(Tracked {
                            gate: actor_gate.clone(),
                            drops: observed_actor_drops.clone(),
                            panic: panic && stage == 0,
                        })
                    },
                    "/failed-static-factory",
                    OneForOneStrategy {
                        max_restarts: 0,
                        within: Duration::from_secs(60),
                        decider: BasicDecisionFn::new(move |_| {
                            std::hint::black_box(&policy);
                            SupervisionDecision::Stop
                        }),
                    },
                )
                .await
                .unwrap();
            within(gate.entered.notified()).await;
            let held = system.instance_admission.available_permits() == 0;
            let mut next = Box::pin(
                system
                    .spawn_typed_factory::<Static, Request, _>(|| Static, "/after-static-failure"),
            );
            let deferred = futures::poll!(&mut next).is_pending();
            gate.release();
            if deferred {
                assert_eq!(within(next).await.unwrap().ask(Request).await.unwrap(), 42);
            }
            recovered(&system).await;
            let error = within(reference.stop()).await.unwrap_err();
            let expected = if panic {
                "shared factory capture destructor failed"
            } else {
                "shared factory init failed"
            };
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(actor_drops.load(Ordering::SeqCst), 1);
            assert_eq!(factory_drops.load(Ordering::SeqCst), 1);
            assert_eq!(policy_drops.load(Ordering::SeqCst), 1);
            within(system.shutdown_internal()).await.unwrap();
            assert!(
                held && deferred,
                "failed init released capacity before disposal stage {stage}"
            );
        }
    }
}

#[tokio::test]
async fn static_child_factory_shutdown_returns_original_inputs_without_cleanup() {
    type Source = Box<dyn Fn() -> Static + Send + Sync>;
    let system = super::system();
    let context = parent(&system).await;
    within(system.shutdown_internal()).await.unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture(drops.clone());
    let factory: Source = Box::new(move || {
        std::hint::black_box(&capture);
        Static
    });
    let error = match context
        .spawn_typed_child_factory::<_, Request, _>(factory)
        .await
    {
        Err(ActorError::Other(error)) => error
            .downcast::<crate::thread::FactoryAdmissionError<Source>>()
            .unwrap(),
        _ => panic!("closed system capacity must reject before registering a child"),
    };
    assert_eq!(
        error.reason,
        crate::thread::InstanceAdmissionFailure::ShuttingDown
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(context.children().is_empty());
    drop(error.into_inputs());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    within(system.shutdown_internal()).await.unwrap();
}
