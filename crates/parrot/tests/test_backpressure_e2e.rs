//! Local facade-to-Thread mailbox backpressure contracts.
//! Network/transport-window backpressure requires separate remote tests.

use futures::FutureExt;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{BackpressureStrategy, ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, EmptyConfig};
use parrot_api::address::ActorPath;
use parrot_api::errors::ActorError;
use parrot_api::system::{ActorSystem, ActorSystemConfig};
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

const DEADLINE: Duration = Duration::from_secs(5);
struct BpTell(u64);
struct BpGetValues;
struct Gate {
    entered: oneshot::Sender<()>,
    released: oneshot::Receiver<()>,
}
struct Sink {
    values: Vec<u64>,
    destroyed: Arc<AtomicUsize>,
    handled: Arc<AtomicUsize>,
    progress: Arc<tokio::sync::Notify>,
}
impl Drop for Sink {
    fn drop(&mut self) {
        self.destroyed.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Sink {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<Gate>() {
                let Gate { entered, released } = *msg.downcast::<Gate>().unwrap();
                let _ = entered.send(());
                // Dropping the release sender also opens the gate during unwind.
                let _ = released.await;
                return Ok(Box::new(()) as BoxedMessage);
            }
            if let Some(BpTell(value)) = msg.downcast_ref::<BpTell>() {
                self.values.push(*value);
                self.handled.fetch_add(1, Ordering::SeqCst);
                self.progress.notify_one();
                return Ok(Box::new(*value) as BoxedMessage);
            }
            if msg.is::<BpGetValues>() {
                return Ok(Box::new(self.values.clone()) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError("unhandled".into()))
        })
    }
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
}

struct Fixture {
    facade: ParrotActorSystem,
    sink: BoxedActorRef,
    destroyed: Arc<AtomicUsize>,
    handled: Arc<AtomicUsize>,
    progress: Arc<tokio::sync::Notify>,
    gates: tokio::task::JoinSet<ActorResult<BoxedMessage>>,
}
impl Fixture {
    async fn new(capacity: usize, strategy: BackpressureStrategy) -> Self {
        let facade = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            default_mailbox_capacity: capacity,
            default_backpressure_strategy: strategy.clone(),
            ..Default::default()
        });
        facade
            .register_thread_system("eng".into(), system.clone(), true)
            .await
            .unwrap();
        let destroyed = Arc::new(AtomicUsize::new(0));
        let handled = Arc::new(AtomicUsize::new(0));
        let progress = Arc::new(tokio::sync::Notify::new());
        system
            .spawn_at(
                Sink {
                    values: Vec::new(),
                    destroyed: destroyed.clone(),
                    handled: handled.clone(),
                    progress: progress.clone(),
                },
                "/user/slow",
                None,
                ThreadActorConfig {
                    mailbox_capacity: Some(capacity),
                    backpressure_strategy: Some(strategy),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let sink = facade
            .get_actor(&ActorPath::placeholder("/user/slow"))
            .await
            .unwrap();
        Self {
            facade,
            sink,
            destroyed,
            handled,
            progress,
            gates: tokio::task::JoinSet::new(),
        }
    }

    async fn gate(&mut self) -> oneshot::Sender<()> {
        let (entered, receipt) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let actor = self.sink.clone_boxed();
        self.gates.spawn(async move {
            actor
                .send_with_timeout(Box::new(Gate { entered, released }), Some(DEADLINE))
                .await
        });
        tokio::time::timeout(DEADLINE, receipt)
            .await
            .unwrap()
            .unwrap();
        release
    }

    async fn values(&self, expected: usize) -> Vec<u64> {
        tokio::time::timeout(DEADLINE, async {
            while self.handled.load(Ordering::SeqCst) < expected {
                self.progress.notified().await;
            }
        })
        .await
        .expect("admitted messages must drain before the read request");
        let result = self
            .sink
            .send_with_timeout(Box::new(BpGetValues), Some(DEADLINE))
            .await
            .unwrap();
        *result.downcast::<Vec<u64>>().unwrap()
    }

    async fn finish(mut self) {
        let mut joined = true;
        while !self.gates.is_empty() {
            match tokio::time::timeout(DEADLINE, self.gates.join_next()).await {
                Ok(Some(Ok(Ok(_)))) => {}
                Ok(_) => joined = false,
                Err(_) => {
                    joined = false;
                    self.gates.abort_all();
                    while self.gates.join_next().await.is_some() {}
                }
            }
        }
        let stopped = tokio::time::timeout(DEADLINE, self.sink.stop()).await;
        let shutdown = tokio::time::timeout(DEADLINE, self.facade.shutdown()).await;
        assert!(joined, "all admitted gate requests must complete and join");
        stopped.unwrap().unwrap();
        shutdown.unwrap().unwrap();
        assert!(!self.sink.is_alive().await);
        assert_eq!(
            self.destroyed.load(Ordering::SeqCst),
            1,
            "actual actor destruction"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_e2e_block_eventual_delivery() {
    const CAPACITY: u64 = 16;
    let mut fixture = Fixture::new(CAPACITY as usize, BackpressureStrategy::Block).await;
    let result = std::panic::AssertUnwindSafe(async {
        let mut next = 0;
        while next < 200 {
            let release = fixture.gate().await;
            let full_at = (next + CAPACITY).min(200);
            while next < full_at {
                tokio::time::timeout(DEADLINE, fixture.sink.deliver(Box::new(BpTell(next))))
                    .await
                    .unwrap()
                    .unwrap();
                next += 1;
            }
            if next < 200 {
                let pending = fixture.sink.deliver(Box::new(BpTell(next)));
                tokio::pin!(pending);
                assert!(
                    futures::poll!(&mut pending).is_pending(),
                    "full mailbox must block admission"
                );
                assert!(
                    tokio::time::timeout(Duration::from_millis(25), &mut pending)
                        .await
                        .is_err(),
                    "admission must remain blocked while no capacity is released"
                );
                drop(release);
                tokio::time::timeout(DEADLINE, pending)
                    .await
                    .unwrap()
                    .unwrap();
                next += 1;
            } else {
                drop(release);
            }
            assert_eq!(
                fixture.values(next as usize).await,
                (0..next).collect::<Vec<_>>(),
                "every admitted value must arrive exactly once and in order"
            );
        }
        assert_eq!(next, 200);
    })
    .catch_unwind()
    .await;
    fixture.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_e2e_error_fast_fail() {
    let mut fixture = Fixture::new(4, BackpressureStrategy::Error).await;
    let result = std::panic::AssertUnwindSafe(async {
        let release = fixture.gate().await;
        for value in 0..4 {
            tokio::time::timeout(DEADLINE, fixture.sink.deliver(Box::new(BpTell(value))))
                .await
                .unwrap()
                .unwrap();
        }
        for value in 4..64 {
            let rejected =
                tokio::time::timeout(DEADLINE, fixture.sink.deliver(Box::new(BpTell(value))))
                    .await
                    .expect("Error strategy must reject while the gate remains closed");
            assert!(
                matches!(rejected, Err(ActorError::InternalError(ref detail))
                            if detail.contains("Full { capacity: 4 }")),
                "only the configured full-capacity error qualifies: {rejected:?}"
            );
        }
        drop(release);
        assert_eq!(
            fixture.values(4).await,
            vec![0, 1, 2, 3],
            "rejected inputs must not execute"
        );
        fixture.sink.deliver(Box::new(BpTell(99))).await.unwrap();
        assert_eq!(
            fixture.values(5).await,
            vec![0, 1, 2, 3, 99],
            "admission recovers after drain"
        );
    })
    .catch_unwind()
    .await;
    fixture.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
