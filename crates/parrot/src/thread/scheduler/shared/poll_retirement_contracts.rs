use super::*;
use crate::thread::{
    actor::ThreadActor, config::BackpressureStrategy, context::ThreadContext,
    mailbox::mpsc::MpscMailbox, processor::ActorProcessor,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{Condvar, Mutex},
    task::{Context, Poll},
};
use tokio::sync::oneshot;

const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Gate(Mutex<bool>, Condvar);
impl Gate {
    fn release(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
    fn block(&self) {
        let (open, _) = self
            .1
            .wait_timeout_while(self.0.lock().unwrap(), WAIT * 3, |open| !*open)
            .unwrap();
        assert!(
            *open,
            "release the actual actor future before joining its host"
        );
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Work {
    gate: Arc<Gate>,
    polled: Option<oneshot::Sender<()>>,
    dropping: Option<oneshot::Sender<()>>,
    drops: Arc<AtomicUsize>,
    block_poll: bool,
}
impl Future for Work {
    type Output = ActorResult<BoxedMessage>;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(polled) = self.polled.take() {
            let _ = polled.send(());
        }
        if self.block_poll {
            self.gate.block();
            Poll::Ready(Ok(Box::new(())))
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        if let Some(dropping) = self.dropping.take() {
            let _ = dropping.send(());
        }
        self.gate.block();
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct ProbeActor(Option<Work>);
impl Actor for ProbeActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(self.0.take().expect("one real message per actor"))
    }
}

async fn schedule(
    pool: &SharedThreadPool,
    path: &str,
    block_poll: bool,
) -> (
    Release,
    Arc<AtomicUsize>,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
) {
    let gate = Arc::new(Gate::default());
    let drops = Arc::new(AtomicUsize::new(0));
    let (polled, receive_polled) = oneshot::channel();
    let (dropping, receive_dropping) = oneshot::channel();
    let actor = ProbeActor(Some(Work {
        gate: gate.clone(),
        polled: Some(polled),
        dropping: Some(dropping),
        drops: drops.clone(),
        block_poll,
    }));
    let mailbox = Arc::new(MpscMailbox::new(1, ActorPath::placeholder(path)));
    mailbox.set_processor(Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(actor),
        ThreadContext::new_for_test(path),
        path.to_owned(),
        ThreadActorConfig::default(),
    )));
    mailbox
        .push(Box::new(()), BackpressureStrategy::Block)
        .await
        .unwrap();
    pool.schedule_now(path, mailbox, None).unwrap();
    (Release(gate), drops, receive_polled, receive_dropping)
}

fn check(burst: bool, last_owner: bool) {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
        .unwrap();
    host.block_on(async {
        let pool = Arc::new(SharedThreadPool::new(
            Some(SharedThreadPoolConfig {
                pool_size: 1,
                burst_workers_max: usize::from(burst),
                burst_backlog_threshold: Duration::ZERO,
                idle_sleep_duration: Duration::from_millis(1),
                ..Default::default()
            }),
            Handle::current(),
        ));
        // A real synchronous actor poll occupies the core worker. Only the
        // production elastic path can start the second actor in this case.
        let core = if burst {
            let (release, drops, polled, _) = schedule(&pool, "/held-core", true).await;
            tokio::time::timeout(WAIT, polled).await.unwrap().unwrap();
            Some((release, drops))
        } else {
            None
        };
        let (release, drops, polled, dropping) = schedule(&pool, "/held-future", false).await;
        tokio::time::timeout(WAIT, polled).await.unwrap().unwrap();
        if burst {
            assert_eq!(pool.metrics().burst_workers_alive, 1);
        }
        let status = pool.status.clone();
        let (mut repeated, cancelled) = if last_owner {
            drop(pool);
            (None, true)
        } else {
            let owner = pool.clone();
            let first = tokio::spawn(async move { owner.shutdown(0).await });
            tokio::time::timeout(WAIT, async {
                while pool.shutdown_receipt.lock().unwrap().is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            first.abort();
            let cancelled = first.await.is_err_and(|error| error.is_cancelled());
            (
                Some(Box::pin(async move {
                    let terminal = pool.shutdown(0).await.map_err(|error| error.to_string());
                    let repeated = pool.shutdown(0).await.map_err(|error| error.to_string());
                    (terminal, repeated)
                })),
                cancelled,
            )
        };
        tokio::time::timeout(WAIT, dropping).await.unwrap().unwrap();
        let early = match repeated.as_mut() {
            Some(waiter) => tokio::time::timeout(Duration::from_millis(50), waiter)
                .await
                .ok(),
            None => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                None
            }
        };
        let before = status.load(Ordering::Acquire);
        let before_drops = drops.load(Ordering::SeqCst);
        // Release and observe every real destructor before asserting the
        // counterexample, so a failure cannot strand the host runtime.
        drop(release);
        tokio::time::timeout(WAIT, async {
            while drops.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let after_future = status.load(Ordering::Acquire);
        let held_core = core.as_ref().map(|(_, drops)| drops.load(Ordering::SeqCst));
        let core_drops = core.as_ref().map(|(_, drops)| drops.clone());
        drop(core);
        if let Some(drops) = core_drops {
            tokio::time::timeout(WAIT, async {
                while drops.load(Ordering::SeqCst) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        let terminal = match early {
            Some(result) => Some(result),
            None => match repeated {
                Some(waiter) => Some(tokio::time::timeout(WAIT, waiter).await.unwrap()),
                None => None,
            },
        };
        tokio::time::timeout(WAIT, async {
            while status.load(Ordering::Acquire) != SchedulerStatus::Shutdown as usize {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(before_drops, 0);
        assert_eq!(
            before,
            SchedulerStatus::ShuttingDown as usize,
            "shared pool published shutdown before actual actor future disposal"
        );
        assert!(
            cancelled,
            "the first waiter must remain pending until its cancellation"
        );
        if burst {
            assert_eq!(held_core, Some(0));
            assert_eq!(
                after_future,
                SchedulerStatus::ShuttingDown as usize,
                "shared pool published shutdown before the executing core poll retired"
            );
        }
        if let Some((terminal, repeated)) = terminal {
            assert_eq!(terminal, repeated);
        }
    });
}

macro_rules! contract {
    ($name:ident, $burst:expr, $last:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!(
                    "thread::scheduler::shared::pool::poll_retirement_contracts::",
                    stringify!($name)
                ),
                || check($burst, $last),
            );
        }
    };
}
contract!(
    core_cancelled_shutdown_waits_for_real_future_disposal,
    false,
    false
);
contract!(core_last_owner_waits_for_real_future_disposal, false, true);
contract!(
    burst_cancelled_shutdown_waits_for_real_future_and_core_poll,
    true,
    false
);
contract!(
    burst_last_owner_waits_for_real_future_and_core_poll,
    true,
    true
);
