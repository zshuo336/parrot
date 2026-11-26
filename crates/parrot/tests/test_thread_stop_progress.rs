//! Public stop timeout diagnostics describe real, still-owned cleanup work.
use parrot::thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorRef, RetirementStatus, stop_for_cleanup},
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Gate {
    release: (Mutex<bool>, Condvar),
    entered: AtomicBool,
    finished: AtomicBool,
}
impl Gate {
    fn block(&self) {
        self.entered.store(true, Ordering::Release);
        let (_guard, waited) = self
            .release
            .1
            .wait_timeout_while(self.release.0.lock().unwrap(), WAIT * 2, |ready| !*ready)
            .unwrap();
        assert!(
            !waited.timed_out(),
            "test gate expired before explicit release"
        );
        self.finished.store(true, Ordering::Release);
    }
    fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Payload(Arc<Gate>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.block();
    }
}
struct Hold;
struct Probe {
    destructor: Option<Arc<Gate>>,
    running: Arc<AtomicBool>,
    drops: Arc<AtomicUsize>,
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<Hold>() {
                self.running.store(true, Ordering::Release);
                std::future::pending::<()>().await;
            }
            Ok(message)
        })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(gate) = &self.destructor {
            gate.block();
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real cleanup did not reach the required stage");
}
async fn check(mode: SchedulingMode, mailbox: bool) {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_scheduling_mode: mode,
        shutdown_timeout: Duration::from_millis(250),
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let running = Arc::new(AtomicBool::new(false));
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = system
        .spawn_at(
            Probe {
                destructor: (!mailbox).then(|| gate.clone()),
                running: running.clone(),
                drops: drops.clone(),
            },
            "/stop-progress",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    actor.send(Box::new(())).await.unwrap();
    if mailbox {
        actor.deliver(Box::new(Hold)).await.unwrap();
        until(|| running.load(Ordering::Acquire)).await;
        actor
            .deliver(Box::new(Payload(gate.clone())))
            .await
            .unwrap();
    }
    let stopping = tokio::spawn({
        let actor = actor.clone();
        async move { actor.stop().await }
    });
    until(|| gate.entered.load(Ordering::Acquire)).await;
    let result = tokio::time::timeout(WAIT, stopping).await.unwrap().unwrap();
    let Err(ActorError::TimeoutDetail(detail)) = result else {
        panic!("public deadline did not preserve timeout: {result:?}");
    };
    let phase = if mailbox {
        "mailbox-drain"
    } else {
        "scheduler-retirement"
    };
    assert!(
        detail.contains(&format!("observed phase={phase}")),
        "{detail}"
    );
    assert!(detail.contains("children=finished"), "{detail}");
    assert!(!gate.finished.load(Ordering::Acquire));
    if !mailbox {
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }
    assert!(!matches!(
        actor.retirement_status(),
        RetirementStatus::Retired(_)
    ));
    gate.release();
    until(|| matches!(actor.retirement_status(), RetirementStatus::Retired(_))).await;
    let retirement = tokio::time::timeout(WAIT, stop_for_cleanup(&actor))
        .await
        .unwrap();
    if mailbox {
        assert!(
            matches!(retirement, Err(ActorError::Other(ref error))
            if error.to_string().contains("mailbox disposal exceeded deadline")),
            "{retirement:?}"
        );
    } else {
        retirement.unwrap();
    }
    assert!(matches!(
        actor.retirement_status(),
        RetirementStatus::Retired(_)
    ));
    assert!(gate.finished.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(system.actor_count(), 0);
    assert!(
        matches!(actor.stop().await, Err(ActorError::TimeoutDetail(repeated)) if repeated == detail)
    );
    system.shutdown_internal().await.unwrap();
}

macro_rules! contract {
    ($name:ident, $mode:expr, $mailbox:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            check($mode, $mailbox).await;
        }
    };
}
contract!(
    dedicated_mailbox_timeout_keeps_real_progress_and_cleanup,
    SchedulingMode::DedicatedThread,
    true
);
contract!(
    dedicated_retirement_timeout_keeps_real_progress_and_cleanup,
    SchedulingMode::DedicatedThread,
    false
);
contract!(
    shared_mailbox_timeout_keeps_real_progress_and_cleanup,
    SchedulingMode::SharedPool {
        max_messages_per_run: 1
    },
    true
);
contract!(
    shared_retirement_timeout_keeps_real_progress_and_cleanup,
    SchedulingMode::SharedPool {
        max_messages_per_run: 1
    },
    false
);
contract!(
    sharded_mailbox_timeout_keeps_real_progress_and_cleanup,
    SchedulingMode::Sharded {
        affinity_key: "stop-progress".into()
    },
    true
);
contract!(
    sharded_retirement_timeout_keeps_real_progress_and_cleanup,
    SchedulingMode::Sharded {
        affinity_key: "stop-progress".into()
    },
    false
);
