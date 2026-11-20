use super::*;
use crate::thread::ThreadActorSystemConfig;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct Request;
impl Message for Request {
    type Result = usize;
}
pub(super) struct Probe {
    pub(super) initialized: Arc<AtomicUsize>,
    pub(super) init_gate: Option<Arc<tokio::sync::Notify>>,
}
impl ParrotTypedDispatch for Probe {
    type Msg = Request;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(gate) = &self.init_gate {
                gate.notified().await;
            }
            Ok(())
        })
    }
    fn dispatch(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async { Ok(42) })
    }
}
impl ParrotMsgVariant<Probe> for Request {
    fn inject(message: Self) -> Request {
        message
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}

pub(super) fn system() -> Arc<ThreadActorSystem> {
    system_with_timeout(Duration::ZERO)
}

pub(super) fn system_with_timeout(timeout: Duration) -> Arc<ThreadActorSystem> {
    let mut system = ThreadActorSystem::try_new(ThreadActorSystemConfig {
        shared_pool_size: 2,
        shared_burst_workers_max: 0,
        ..Default::default()
    })
    .unwrap();
    system.instance_admission = Arc::new(tokio::sync::Semaphore::new(1));
    system.operation_timeouts.actor_creation = timeout;
    let system = Arc::new(system);
    system.set_self_weak(Arc::downgrade(&system));
    system
}

#[tokio::test]
async fn static_root_waits_for_shared_capacity_before_registration() {
    let system = system();
    let held = system
        .instance_admission
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let initialized = Arc::new(AtomicUsize::new(0));
    let creation = system.spawn_typed::<_, Request>(
        Probe {
            initialized: initialized.clone(),
            init_gate: None,
        },
        "/static-wait",
    );
    tokio::pin!(creation);
    assert!(
        futures::poll!(&mut creation).is_pending(),
        "static root bypassed shared instance admission"
    );
    assert!(!system.registry.read().unwrap().contains_key("/static-wait"));
    assert_eq!(initialized.load(Ordering::SeqCst), 0);
    drop(held);
    let actor = tokio::time::timeout(Duration::from_secs(5), creation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actor.ask(Request).await.unwrap(), 42);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn static_capacity_is_held_until_init_then_released_while_actor_is_live() {
    let system = system();
    let initialized = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());
    let actor = system
        .spawn_typed::<_, Request>(
            Probe {
                initialized: initialized.clone(),
                init_gate: Some(gate.clone()),
            },
            "/static-init",
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while initialized.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        system.instance_admission.available_permits(),
        0,
        "reference publication is not initialization completion"
    );
    gate.notify_one();
    assert_eq!(actor.ask(Request).await.unwrap(), 42);
    assert_eq!(
        system.instance_admission.available_permits(),
        1,
        "live initialized instances must not retain creation capacity"
    );
    system.shutdown_internal().await.unwrap();
}
