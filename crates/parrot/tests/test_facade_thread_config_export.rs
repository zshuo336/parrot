//! Exercise configuration export through both public facade Thread entries.
use parrot::{
    system::ParrotActorSystem,
    thread::{
        SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig,
        ThreadContext,
    },
};
use parrot_api::{
    actor::{Actor, ActorConfig, ActorState},
    address::ActorRef,
    system::SystemError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    future::Future,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(10);

fn shared() -> SchedulingMode {
    SchedulingMode::SharedPool {
        max_messages_per_run: 64,
    }
}

#[derive(Debug, Default)]
struct Counts {
    exports: AtomicUsize,
    actor_drops: AtomicUsize,
    config_drops: AtomicUsize,
    output_drops: AtomicUsize,
    initialized: AtomicUsize,
}

#[derive(Clone, Copy)]
enum Export {
    Healthy,
    Invalid,
    WrongType,
    Panic,
    HostileOutput,
}

#[derive(Debug, Default)]
struct Gate {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    changed: Condvar,
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
struct HostileOutput(Arc<Counts>);
impl Drop for HostileOutput {
    fn drop(&mut self) {
        self.0.output_drops.fetch_add(1, Ordering::SeqCst);
        panic!("facade exported output disposal");
    }
}
struct Config {
    counts: Arc<Counts>,
    export: Export,
    mode: SchedulingMode,
    gate: Option<Arc<Gate>>,
}
impl ActorConfig for Config {
    fn engine_config(&self) -> Option<Box<dyn Any + Send>> {
        self.counts.exports.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.entered.notify_one();
            let (_guard, timeout) = gate
                .changed
                .wait_timeout_while(gate.released.lock().unwrap(), LIMIT, |released| !*released)
                .unwrap();
            assert!(!timeout.timed_out(), "conversion gate was not released");
        }
        match self.export {
            Export::Healthy | Export::Invalid => Some(Box::new(ThreadActorConfig {
                mailbox_capacity: Some(if matches!(self.export, Export::Invalid) {
                    0
                } else {
                    7
                }),
                scheduling_mode: Some(self.mode.clone()),
                ..Default::default()
            })),
            Export::WrongType => Some(Box::new(42_u64)),
            Export::Panic => panic!("facade configuration conversion"),
            Export::HostileOutput => Some(Box::new(HostileOutput(self.counts.clone()))),
        }
    }
}
impl Drop for Config {
    fn drop(&mut self) {
        self.counts.config_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Probe(Arc<Counts>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.actor_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Probe {
    type Config = Config;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let thread = std::thread::current();
        let owner = (thread.id(), thread.name().unwrap_or("").to_owned());
        Box::pin(async move { Ok(Box::new(owner) as BoxedMessage) })
    }
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(LIMIT, future)
        .await
        .expect("facade export deadline")
}
async fn setup(named: bool) -> (Arc<ParrotActorSystem>, Arc<ThreadActorSystem>) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let native = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        max_dedicated_threads: 2,
        default_mailbox_capacity: 19,
        ..Default::default()
    });
    facade
        .register_thread_system("selected".into(), native.clone(), !named)
        .await
        .unwrap();
    (facade, native)
}
fn spawn<'a>(
    facade: &'a ParrotActorSystem,
    named: bool,
    counts: &Arc<Counts>,
    export: Export,
    mode: SchedulingMode,
    gate: Option<Arc<Gate>>,
) -> BoxedFuture<'a, Result<Box<dyn ActorRef>, SystemError>> {
    let actor = Probe(counts.clone());
    let config = Config {
        counts: counts.clone(),
        export,
        mode,
        gate,
    };
    if named {
        facade.spawn_root_thread_in_system("selected", actor, config)
    } else {
        facade.spawn_root_thread(actor, config)
    }
}
async fn healthy(named: bool) {
    let (facade, native) = setup(named).await;
    for mode in [
        shared(),
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "facade-config".into(),
        },
    ] {
        let counts = Arc::new(Counts::default());
        let reference = bounded(spawn(
            &facade,
            named,
            &counts,
            Export::Healthy,
            mode.clone(),
            None,
        ))
        .await
        .unwrap();
        assert_eq!(counts.exports.load(Ordering::SeqCst), 1);
        assert_eq!(counts.config_drops.load(Ordering::SeqCst), 1);
        let mailbox = native
            .get_mailbox(&reference.path())
            .expect("actor published in selected engine");
        assert_eq!(mailbox.capacity(), 7, "exported capacity was not consumed");
        let reply = bounded(reference.send(Box::new(()))).await.unwrap();
        let (_, name) = *reply.downcast::<(std::thread::ThreadId, String)>().unwrap();
        match mode {
            SchedulingMode::DedicatedThread => {
                assert!(name.starts_with("parrot-dedicated-"), "{name}")
            }
            SchedulingMode::Sharded { .. } => assert!(name.starts_with("parrot-shard-"), "{name}"),
            SchedulingMode::SharedPool { .. } => {}
        }
        bounded(reference.stop()).await.unwrap();
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
        assert_eq!(counts.actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(native.actor_count(), 0);
    }
    bounded(facade.shutdown_runtime()).await.unwrap();
}
async fn rejected(named: bool) {
    let (facade, native) = setup(named).await;
    for export in [
        Export::Invalid,
        Export::WrongType,
        Export::Panic,
        Export::HostileOutput,
    ] {
        let counts = Arc::new(Counts::default());
        let result = bounded(spawn(&facade, named, &counts, export, shared(), None)).await;
        let Err(SystemError::ActorCreationError(message)) = result else {
            panic!("facade did not preserve a creation error");
        };
        let expected = match export {
            Export::Invalid => "mailbox_capacity",
            Export::WrongType => "ThreadActorConfig",
            Export::Panic => "facade configuration conversion",
            Export::HostileOutput => "facade exported output disposal",
            Export::Healthy => unreachable!(),
        };
        assert!(message.contains(expected), "{message}");
        assert_eq!(counts.exports.load(Ordering::SeqCst), 1);
        assert_eq!(counts.config_drops.load(Ordering::SeqCst), 1);
        assert_eq!(counts.actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.output_drops.load(Ordering::SeqCst),
            usize::from(matches!(export, Export::HostileOutput))
        );
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(native.actor_count(), 0);
    }
    bounded(facade.shutdown_runtime()).await.unwrap();
}
async fn unpolled_and_closed(named: bool) {
    let (facade, native) = setup(named).await;
    let counts = Arc::new(Counts::default());
    drop(spawn(
        &facade,
        named,
        &counts,
        Export::Panic,
        shared(),
        None,
    ));
    bounded(async {
        while counts.actor_drops.load(Ordering::SeqCst) != 1
            || counts.config_drops.load(Ordering::SeqCst) != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(facade.shutdown_runtime()).await.unwrap();
    assert!(matches!(
        bounded(spawn(
            &facade,
            named,
            &counts,
            Export::Panic,
            shared(),
            None
        ))
        .await,
        Err(SystemError::ShuttingDown)
    ));
    assert_eq!(counts.exports.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actor_drops.load(Ordering::SeqCst), 2);
    assert_eq!(counts.config_drops.load(Ordering::SeqCst), 2);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(native.actor_count(), 0);
}
async fn cancelled_conversion(named: bool) {
    let (facade, native) = setup(named).await;
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let waiter = {
        let facade = facade.clone();
        let counts = counts.clone();
        let gate = gate.clone();
        tokio::spawn(async move {
            spawn(
                &facade,
                named,
                &counts,
                Export::Healthy,
                shared(),
                Some(gate),
            )
            .await
        })
    };
    bounded(gate.entered.notified()).await;
    assert_eq!(
        native.actor_count(),
        1,
        "pending conversion must remain owned"
    );
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    waiter.abort();
    assert!(bounded(waiter).await.unwrap_err().is_cancelled());
    assert_eq!(counts.config_drops.load(Ordering::SeqCst), 0);
    drop(release);
    bounded(async {
        while counts.actor_drops.load(Ordering::SeqCst) != 1
            || counts.config_drops.load(Ordering::SeqCst) != 1
            || native.actor_count() != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(counts.exports.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    bounded(facade.shutdown_runtime()).await.unwrap();
}

macro_rules! contract {
    ($name:ident, $body:ident, $named:expr) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            $body($named).await;
        }
    };
}
contract!(
    default_export_controls_native_mailbox_and_scheduler,
    healthy,
    false
);
contract!(
    named_export_controls_native_mailbox_and_scheduler,
    healthy,
    true
);
contract!(
    default_invalid_exports_retire_without_publication,
    rejected,
    false
);
contract!(
    named_invalid_exports_retire_without_publication,
    rejected,
    true
);
contract!(
    default_unpolled_and_closed_creation_skip_export,
    unpolled_and_closed,
    false
);
contract!(
    named_unpolled_and_closed_creation_skip_export,
    unpolled_and_closed,
    true
);
contract!(
    default_cancelled_export_keeps_late_input_ownership,
    cancelled_conversion,
    false
);
contract!(
    named_cancelled_export_keeps_late_input_ownership,
    cancelled_conversion,
    true
);
