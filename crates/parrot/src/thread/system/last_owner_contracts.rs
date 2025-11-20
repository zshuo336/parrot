use super::*;
use parrot_api::context::{ActorContext, ActorSpawnerExt};
use std::sync::atomic::AtomicUsize;
use std::{cell::RefCell, sync::Condvar};

const WAIT: Duration = Duration::from_secs(8);

#[derive(Default)]
struct ExitGate {
    entered: AtomicBool,
    expired: AtomicBool,
    open: Mutex<bool>,
    changed: Condvar,
}
impl ExitGate {
    fn block(&self) {
        self.entered.store(true, Ordering::Release);
        let (_open, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT * 2, |open| !*open)
            .unwrap();
        self.expired.store(timeout.timed_out(), Ordering::Release);
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct ThreadExit(Arc<ExitGate>);
impl Drop for ThreadExit {
    fn drop(&mut self) {
        self.0.block();
    }
}
thread_local! {
    static EXIT: RefCell<Option<ThreadExit>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct State {
    stopping: AtomicBool,
    hook_open: AtomicBool,
    disposed: AtomicBool,
    owner_dropped: AtomicBool,
    owner: Mutex<Option<std::thread::ThreadId>>,
    changed: Notify,
    exit: Arc<ExitGate>,
}
impl State {
    fn release(&self) {
        self.hook_open.store(true, Ordering::Release);
        self.changed.notify_waiters();
        self.exit.release();
    }
}
struct Release(Arc<State>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Probe(Arc<State>);
impl Drop for Probe {
    fn drop(&mut self) {
        assert_eq!(
            *self.0.owner.lock().unwrap(),
            Some(std::thread::current().id()),
            "actor disposal moved away from its business worker"
        );
        self.0.disposed.store(true, Ordering::Release);
    }
}
enum Command {
    Ping,
    Release(Arc<ThreadActorSystem>),
    Child(Arc<State>),
    Block(Arc<ExitGate>),
}
impl Actor for Probe {
    type Config = parrot_api::actor::EmptyConfig;
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
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let current = std::thread::current().id();
            {
                let mut owner = self.0.owner.lock().unwrap();
                if let Some(owner) = *owner {
                    assert_eq!(owner, current);
                } else {
                    *owner = Some(current);
                    EXIT.with(|slot| {
                        assert!(
                            slot.borrow_mut()
                                .replace(ThreadExit(self.0.exit.clone()))
                                .is_none()
                        );
                    });
                }
            }
            match *message.downcast::<Command>().unwrap() {
                Command::Release(system) => {
                    drop(system);
                    self.0.owner_dropped.store(true, Ordering::Release);
                }
                Command::Child(state) => {
                    let child = ctx
                        .spawner()
                        .spawn_typed(Child(state), Default::default())
                        .await?;
                    return Ok(Box::new(child) as BoxedMessage);
                }
                Command::Block(gate) => gate.block(),
                Command::Ping => {}
            }
            Ok(Box::new(41_u8) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.stopping.store(true, Ordering::Release);
            loop {
                let changed = self.0.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.0.hook_open.load(Ordering::Acquire) {
                    return Ok(());
                }
                changed.await;
            }
        })
    }
}

async fn until(condition: impl Fn() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned retirement did not reach its independently observed boundary");
}
fn worker_count(schedulers: &SchedulerGroup, sharded: bool) -> usize {
    if sharded {
        schedulers
            .sharded_scheduler
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .owned_thread_count()
    } else {
        schedulers.dedicated_scheduler.worker_count()
    }
}

#[derive(Clone, Copy)]
enum Owner {
    Caller,
    Worker,
    Concurrent,
    CancelledWaiter,
}

fn contract(sharded: bool, owner: Owner, host_exit: bool) {
    let host = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let state = Arc::new(State::default());
    let _release = Release(state.clone());
    let (system, actor) = host.block_on(async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            shutdown_timeout: WAIT,
            ..Default::default()
        });
        let mode = if sharded {
            SchedulingMode::Sharded {
                affinity_key: "last-owner".into(),
            }
        } else {
            SchedulingMode::DedicatedThread
        };
        let actor = system
            .spawn_at(
                Probe(state.clone()),
                "/last-owner",
                None,
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let reply = actor
            .ask_with_strategy_and_timeout(
                Box::new(Command::Ping),
                BackpressureStrategy::Block,
                WAIT,
            )
            .await
            .unwrap();
        assert_eq!(*reply.downcast::<u8>().unwrap(), 41);
        (system, actor)
    });
    let weak = Arc::downgrade(&system);
    let registry = system.registry.clone();
    let schedulers = system.scheduler_group.clone();
    let closed = system.is_shutting_down.clone();
    let stopped = system.stopped.clone();
    assert!(worker_count(&schedulers, sharded) > 0);
    let host = if host_exit {
        drop(host);
        None
    } else {
        Some(host)
    };
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    observer.block_on(async {
        match owner {
            Owner::Worker => {
                actor
                    .send(Box::new(Command::Release(system)))
                    .await
                    .unwrap();
                until(|| state.owner_dropped.load(Ordering::Acquire)).await;
            }
            Owner::Concurrent => {
                let other = system.clone();
                let barrier = Arc::new(std::sync::Barrier::new(2));
                let ready = barrier.clone();
                let thread = std::thread::spawn(move || {
                    ready.wait();
                    drop(other);
                });
                barrier.wait();
                drop(system);
                thread.join().unwrap();
            }
            Owner::CancelledWaiter => {
                let waiter_system = system.clone();
                let waiter = tokio::spawn(async move { waiter_system.shutdown_internal().await });
                until(|| state.stopping.load(Ordering::Acquire)).await;
                waiter.abort();
                assert!(waiter.await.unwrap_err().is_cancelled());
                drop(system);
            }
            Owner::Caller => {
                let other = system.clone();
                drop(other);
                assert!(
                    !closed.load(Ordering::Acquire),
                    "nonfinal release closed admission"
                );
                let reply = actor
                    .ask_with_strategy_and_timeout(
                        Box::new(Command::Ping),
                        BackpressureStrategy::Block,
                        WAIT,
                    )
                    .await
                    .unwrap();
                assert_eq!(*reply.downcast::<u8>().unwrap(), 41);
                drop(system);
            }
        }
        until(|| closed.load(Ordering::Acquire)).await;
        assert!(weak.upgrade().is_none(), "final system owner was retained");
        until(|| state.stopping.load(Ordering::Acquire)).await;
        assert!(!state.disposed.load(Ordering::Acquire));
        assert!(!stopped.load(Ordering::Acquire));
        assert!(worker_count(&schedulers, sharded) > 0);
        state.hook_open.store(true, Ordering::Release);
        state.changed.notify_waiters();
        until(|| state.exit.entered.load(Ordering::Acquire)).await;
        assert!(state.disposed.load(Ordering::Acquire));
        if sharded {
            assert!(registry.read().unwrap().is_empty());
        } else {
            assert!(registry.read().unwrap().contains_key("/last-owner"));
        }
        assert!(
            !stopped.load(Ordering::Acquire),
            "actor disposal was mistaken for an OS join"
        );
        assert!(
            worker_count(&schedulers, sharded) > 0,
            "live TLS disposal lost its thread owner"
        );
        state.exit.release();
        until(|| stopped.load(Ordering::Acquire)).await;
        assert!(registry.read().unwrap().is_empty());
        assert_eq!(worker_count(&schedulers, sharded), 0);
        assert!(!state.exit.expired.load(Ordering::Acquire));
        assert_eq!(
            schedulers.shared_scheduler.status(),
            crate::thread::scheduler::shared::SchedulerStatus::Shutdown
        );
    });
    drop(host);
}

macro_rules! cases {
    ($($name:ident: $sharded:expr, $owner:ident, $exit:expr;)*) => {$(
        #[test]
        fn $name() { contract($sharded, Owner::$owner, $exit); }
    )*};
}
cases! {
    dedicated_final_owner_joins_worker_after_host_exit: false, Caller, true;
    sharded_final_owner_joins_worker_after_host_exit: true, Caller, true;
    dedicated_final_owner_on_business_worker_avoids_self_join: false, Worker, true;
    sharded_final_owner_on_business_worker_avoids_self_join: true, Worker, true;
    dedicated_concurrent_final_owners_share_complete_drain: false, Concurrent, false;
    sharded_concurrent_final_owners_share_complete_drain: true, Concurrent, false;
    dedicated_cancelled_shutdown_waiter_keeps_real_join_owned: false, CancelledWaiter, true;
    sharded_cancelled_shutdown_waiter_keeps_real_join_owned: true, CancelledWaiter, true;
}

struct Child(Arc<State>);
impl Drop for Child {
    fn drop(&mut self) {
        self.0.exit.block();
        self.0.disposed.store(true, Ordering::Release);
    }
}
impl Actor for Child {
    type Config = parrot_api::actor::EmptyConfig;
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
        Box::pin(async { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.stopping.store(true, Ordering::Release);
            loop {
                let changed = self.0.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.0.hook_open.load(Ordering::Acquire) {
                    return Ok(());
                }
                changed.await;
            }
        })
    }
}

#[derive(Default)]
struct LateState {
    construct: Arc<ExitGate>,
    output: Arc<ExitGate>,
    capture: Arc<ExitGate>,
    initialized: AtomicBool,
    disposed: AtomicBool,
    captured: AtomicBool,
    constructions: AtomicUsize,
    outputs: AtomicUsize,
    captures: AtomicUsize,
}
struct Capture(Arc<LateState>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.capture.block();
        self.0.captures.fetch_add(1, Ordering::SeqCst);
        self.0.captured.store(true, Ordering::Release);
    }
}
struct Late(Arc<LateState>);
impl Drop for Late {
    fn drop(&mut self) {
        self.0.output.block();
        self.0.outputs.fetch_add(1, Ordering::SeqCst);
        self.0.disposed.store(true, Ordering::Release);
    }
}
impl Actor for Late {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.initialized.store(true, Ordering::Release);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}
struct ReleaseMixed(Arc<LateState>, Arc<ExitGate>);
impl Drop for ReleaseMixed {
    fn drop(&mut self) {
        for gate in [&self.0.construct, &self.0.output, &self.0.capture, &self.1] {
            gate.release();
        }
    }
}

fn mixed_contract(sharded: bool) {
    let host = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let state = Arc::new(State::default());
    let child = Arc::new(State::default());
    let late = Arc::new(LateState::default());
    let dispatch = Arc::new(ExitGate::default());
    let _release = Release(state.clone());
    let _release_child = Release(child.clone());
    let _release_mixed = ReleaseMixed(late.clone(), dispatch.clone());
    let (system, actor) = host.block_on(async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 2,
            shared_burst_workers_max: 0,
            default_scheduling_mode: SchedulingMode::DedicatedThread,
            shutdown_timeout: Duration::from_millis(350),
            ..Default::default()
        });
        let mode = if sharded {
            SchedulingMode::Sharded {
                affinity_key: "mixed-owner".into(),
            }
        } else {
            SchedulingMode::DedicatedThread
        };
        let actor = system
            .spawn_at(
                Probe(state.clone()),
                "/mixed-parent",
                None,
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let reference = actor
            .ask_with_strategy_and_timeout(
                Box::new(Command::Child(child.clone())),
                BackpressureStrategy::Block,
                WAIT,
            )
            .await
            .unwrap();
        let reference = *reference.downcast::<BoxedActorRef>().unwrap();
        reference.send(Box::new(1_u8)).await.unwrap();
        (system, actor)
    });
    let capture = Capture(late.clone());
    let factory = move || {
        capture.0.constructions.fetch_add(1, Ordering::SeqCst);
        capture.0.construct.block();
        Late(capture.0.clone())
    };
    let mut pending =
        Box::pin(system.spawn_supervised_with_config(factory, "/mixed-late", Default::default()));
    host.block_on(async {
        assert!(futures::poll!(&mut pending).is_pending());
        until(|| late.construct.entered.load(Ordering::Acquire)).await;
        actor
            .send_with_strategy(
                Box::new(Command::Block(dispatch.clone())),
                BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        until(|| dispatch.entered.load(Ordering::Acquire)).await;
    });
    drop(pending);
    drop(host);
    let schedulers = system.scheduler_group.clone();
    let stopped = system.stopped.clone();
    let registry = system.registry.clone();
    let closed = system.is_shutting_down.clone();
    let weak = Arc::downgrade(&system);
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    observer.block_on(async {
        let owner = system.clone();
        let waiter = tokio::spawn(async move { owner.shutdown_internal().await });
        until(|| closed.load(Ordering::Acquire)).await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        let first = system.shutdown_internal().await.unwrap_err().to_string();
        assert!(
            first.contains("actor shutdown exceeded configured deadline"),
            "{first}"
        );
        assert_eq!(
            system.shutdown_internal().await.unwrap_err().to_string(),
            first
        );
        assert!(!stopped.load(Ordering::Acquire));
        assert!(!state.disposed.load(Ordering::Acquire));
        assert!(!child.disposed.load(Ordering::Acquire));
        assert!(registry.read().unwrap().contains_key("/mixed-late"));
        assert!(actor.send(Box::new(Command::Ping)).await.is_err());
        drop(system);
        until(|| weak.upgrade().is_none()).await;
        dispatch.release();
        state.hook_open.store(true, Ordering::Release);
        state.changed.notify_waiters();
        child.hook_open.store(true, Ordering::Release);
        child.changed.notify_waiters();
        child.exit.release();
        until(|| child.disposed.load(Ordering::Acquire)).await;
        assert!(!stopped.load(Ordering::Acquire));
        late.construct.release();
        until(|| late.output.entered.load(Ordering::Acquire)).await;
        assert!(!late.initialized.load(Ordering::Acquire));
        assert!(!stopped.load(Ordering::Acquire));
        late.output.release();
        until(|| late.capture.entered.load(Ordering::Acquire)).await;
        assert!(late.disposed.load(Ordering::Acquire));
        assert!(!stopped.load(Ordering::Acquire));
        late.capture.release();
        until(|| state.exit.entered.load(Ordering::Acquire)).await;
        assert!(state.disposed.load(Ordering::Acquire));
        assert!(!stopped.load(Ordering::Acquire));
        assert!(worker_count(&schedulers, sharded) > 0);
        state.exit.release();
        until(|| stopped.load(Ordering::Acquire)).await;
        assert!(registry.read().unwrap().is_empty());
        assert!(late.captured.load(Ordering::Acquire));
        assert_eq!(late.constructions.load(Ordering::SeqCst), 1);
        assert_eq!(late.outputs.load(Ordering::SeqCst), 1);
        assert_eq!(late.captures.load(Ordering::SeqCst), 1);
        assert!(!late.initialized.load(Ordering::Acquire));
        assert_eq!(schedulers.dedicated_scheduler.worker_count(), 0);
        if sharded {
            assert_eq!(worker_count(&schedulers, true), 0);
        }
        for gate in [
            &dispatch,
            &state.exit,
            &child.exit,
            &late.construct,
            &late.output,
            &late.capture,
        ] {
            assert!(
                !gate.expired.load(Ordering::Acquire),
                "a gate watchdog advanced the contract"
            );
        }
    });
}
#[test]
fn dedicated_mixed_shutdown_owns_dispatch_child_late_output_capture_and_os_join() {
    mixed_contract(false);
}
#[test]
fn sharded_mixed_shutdown_owns_dispatch_child_late_output_capture_and_os_join() {
    mixed_contract(true);
}
