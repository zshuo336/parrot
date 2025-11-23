use super::*;
use crate::{owned_payload, thread::ThreadActorSystem};
use parrot_api::engine::{
    AsyncEngineRuntime, AsyncFactoryRuntime, FactoryOptions, RuntimeActor, SpawnOptions,
};
use std::sync::Condvar;
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(8);

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: Notify,
    drops: AtomicUsize,
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.open.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
struct Capture(Arc<Gate>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.entered.notify_one();
        let (open, _) = self
            .0
            .changed
            .wait_timeout_while(self.0.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(*open, "test must release actual factory capture disposal");
        self.0.drops.fetch_add(1, Ordering::SeqCst);
        panic!("Actix local cleanup capture receipt");
    }
}
struct Echo;
impl ParrotActor for Echo {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}

fn check(host_exit: bool) {
    let host = actix::System::new();
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let capture = Capture(gate.clone());
    let (system, actor) = host.block_on(async {
        let system = ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: Duration::from_secs(20),
            ..Default::default()
        })
        .await
        .unwrap();
        let actor = system
            .spawn_factory(
                Box::new(move || {
                    std::hint::black_box(&capture);
                    Box::new(Echo) as RuntimeActor
                }),
                FactoryOptions {
                    spawn: SpawnOptions::at("/local-saturated-factory"),
                    max_restarts: 0,
                    within: WAIT,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            *actor
                .send(Box::new(31_u8))
                .await
                .unwrap()
                .downcast::<u8>()
                .unwrap(),
            31
        );
        (system, actor)
    });
    let slots = system.cleanup_slots.clone();
    let mut pending: Vec<_> = (0..owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT)
        .map(|_| owned_payload::try_pending_cleanup_permit(&slots).unwrap())
        .collect();
    assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
    // This is local exhaustion: a different pool still obtains global admission.
    let independent_slots = Arc::new(tokio::sync::Semaphore::new(1));
    assert!(owned_payload::try_pending_cleanup_permit(&independent_slots).is_some());
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    observer.block_on(async {
        let mut stopped = actor.stop();
        assert!(futures::poll!(&mut stopped).is_pending());
        drop(stopped);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), gate.entered.notified())
                .await
                .is_err(),
            "local pending exhaustion bypassed factory cleanup admission"
        );
    });
    let host = if host_exit {
        drop(host);
        None
    } else {
        Some(host)
    };
    observer.block_on(async {
        let other = ThreadActorSystem::shared(crate::thread::ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            ..Default::default()
        });
        let independent = AsyncEngineRuntime::spawn(
            other.as_ref(),
            Box::new(Echo),
            Box::new(()),
            SpawnOptions::at("/independent"),
        )
        .await
        .unwrap();
        independent.send(Box::new(43_u8)).await.unwrap();
        tokio::time::timeout(WAIT, independent.stop())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
        assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
        let mut first = actor.stop();
        let mut second = actor.stop();
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        drop(pending.pop());
        tokio::time::timeout(WAIT, gate.entered.notified())
            .await
            .unwrap();
        assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
        assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(
            tokio::time::timeout(WAIT, other.retirement_runtime().spawn(async { 17 }))
                .await
                .unwrap()
                .unwrap(),
            17,
        );
        drop(release);
        let (first, second) = tokio::time::timeout(WAIT, async { tokio::join!(first, second) })
            .await
            .unwrap();
        let error = first.unwrap_err().to_string();
        assert!(
            error.contains("Actix local cleanup capture receipt"),
            "{error}"
        );
        assert_eq!(error, second.unwrap_err().to_string());
        assert_eq!(error, actor.stop().await.unwrap_err().to_string());
        assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
        tokio::time::timeout(WAIT, async {
            while owned_payload::try_pending_cleanup_permit(&slots).is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(pending);
        AsyncEngineRuntime::shutdown(other.as_ref()).await.unwrap();
        let _ = system.clone().shutdown().await;
    });
    drop(host);
}

#[test]
fn local_pending_saturation_keeps_factory_stop_and_independent_driver_live() {
    crate::test_support::isolated(
        "actix::system::cleanup_saturation_contracts::local_pending_saturation_keeps_factory_stop_and_independent_driver_live",
        || check(false),
    );
}

#[test]
fn local_pending_saturation_keeps_factory_receipt_after_original_host_exit() {
    crate::test_support::isolated(
        "actix::system::cleanup_saturation_contracts::local_pending_saturation_keeps_factory_receipt_after_original_host_exit",
        || check(true),
    );
}
