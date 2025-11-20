use super::*;
use parrot_api::{
    actor::{ActorState, EmptyConfig},
    supervisor::{BasicDecisionFn, OneForAllStrategy},
};
use std::sync::{Condvar, atomic::AtomicUsize};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: tokio::sync::Notify,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn block(&self) {
        self.entered.notify_one();
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(*open, "constructor must be released by the test");
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Output(Arc<AtomicUsize>);
impl Drop for Output {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for Output {}
impl Actor for Output {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
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
        Box::pin(async { Err(ActorError::InternalError("original root failure".into())) })
    }
}

#[actix::test]
async fn cohort_close_before_member_seal_is_successful_cancellation() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let cohort = system
        .root_cohort(SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 2,
            within: WAIT,
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
        }))
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let builds = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let next = builds.clone();
    let observed = drops.clone();
    let constructor = gate.clone();
    let actor = system
        .spawn_root_factory_in_cohort(
            move || {
                let generation = next.fetch_add(1, Ordering::SeqCst);
                if generation == 1 {
                    constructor.block();
                }
                Output(observed.clone())
            },
            "/close-before-seal",
            &cohort,
        )
        .await
        .unwrap();
    assert!(actor.send(Box::new(())).await.is_err());
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("recovery constructor did not start");
    let control = system
        .registry
        .read()
        .unwrap()
        .actors
        .get("/close-before-seal")
        .unwrap()
        .control
        .clone();
    // Hold the first phase of cohort.stop / system shutdown, before dispatch
    // can request_stop each member on the shared driver.
    cohort.scope.close();
    assert!(!control.channel.is_sealed());
    drop(release);
    tokio::time::timeout(WAIT, async {
        loop {
            if !system
                .registry
                .read()
                .unwrap()
                .actors
                .contains_key("/close-before-seal")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled recovery did not retire the logical actor");
    let stopped = actor.stop().await;
    let joined = cohort.stop().await;
    let shutdown = system.shutdown().await;
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(
        stopped.is_ok(),
        "parent close became a member failure: {stopped:?}"
    );
    joined.unwrap();
    shutdown.unwrap();
}

#[test]
fn cohort_cancellation_preserves_real_failures_and_deadlines() {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    host.block_on(async {
        let (control, _) = ActorControl::new(WAIT);
        let member = uuid::Uuid::new_v4();
        let recovery = crate::children::recovery::Recovery::new(
            uuid::Uuid::new_v4(),
            [member],
            Instant::now() + WAIT,
            Box::new(|_| {}),
        );
        let cancelled = GroupBuildError::stopped("cohort cancelled".into());
        assert!(!cancelled.is_normal_stop(&control, &recovery));
        recovery.cancel("parent close");
        assert!(cancelled.is_normal_stop(&control, &recovery));
        for error in [
            ConstructionError::ConstructorPanicked("constructor failure".into()),
            ConstructionError::DisposalPanicked("output disposal failure".into()),
            ConstructionError::WorkerFailed("worker failure".into()),
            ConstructionError::Publication(ActorError::InternalError("publication failure".into())),
        ] {
            assert!(!GroupBuildError::construction(error).is_normal_stop(&control, &recovery));
        }
        let cleanup = GroupBuildError {
            cancelled: true,
            outcome: ActorError::Stopped,
            retirement: Some(crate::error_receipt::ActorReceipt::from_error(
                ActorError::Panic("actual cleanup failure".into()),
            )),
        };
        assert!(!cleanup.is_normal_stop(&control, &recovery));
        control.request_stop(Instant::now());
        assert!(!cancelled.is_normal_stop(&control, &recovery));
    });
}
