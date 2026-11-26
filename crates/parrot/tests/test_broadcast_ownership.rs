//! Broadcast inputs remain owned before polling, through cloning and cancellation.
use parrot::{
    actix::{ActixActor, ActixActorSystem, ActixContext},
    system::{ActorSystemImpl, ParrotActorSystem},
    thread::{context::ThreadContext, system::ThreadActorSystem},
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    message::Message,
    system::ActorSystem,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Counts {
    clones: AtomicUsize,
    drops: AtomicUsize,
    payloads: AtomicUsize,
    delivered: AtomicUsize,
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("broadcast panic payload destructor");
    }
}
struct Input {
    counts: Arc<Counts>,
    clone_fails: bool,
    drop_fails: bool,
    clone_drop_fails: bool,
    gate: Option<Arc<CloneGate>>,
}
#[derive(Default)]
struct CloneGate {
    entered: std::sync::atomic::AtomicBool,
    open: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
    block_at: usize,
}
impl CloneGate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
impl Clone for Input {
    fn clone(&self) -> Self {
        let copy = self.counts.clones.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(gate) = &self.gate
            && (gate.block_at == 0 || gate.block_at == copy)
        {
            gate.entered.store(true, Ordering::SeqCst);
            let (_guard, timeout) = gate
                .changed
                .wait_timeout_while(gate.open.lock().unwrap(), Duration::from_secs(8), |open| {
                    !*open
                })
                .unwrap();
            assert!(
                !timeout.timed_out(),
                "broadcast cloning blocked the host driver"
            );
        }
        if self.clone_fails {
            std::panic::panic_any(Recursive(self.counts.clone()));
        }
        Self {
            counts: self.counts.clone(),
            clone_fails: false,
            drop_fails: self.clone_drop_fails,
            clone_drop_fails: false,
            gate: None,
        }
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        if self.drop_fails {
            std::panic::panic_any(Recursive(self.counts.clone()));
        }
    }
}
impl Message for Input {
    type Result = ();
}
fn input(counts: &Arc<Counts>, clone_fails: bool, drop_fails: bool) -> Input {
    Input {
        counts: counts.clone(),
        clone_fails,
        drop_fails,
        clone_drop_fails: false,
        gate: None,
    }
}
fn cancel<T>(future: T) {
    let result = catch_unwind(AssertUnwindSafe(|| drop(future)));
    let escaped = result.is_err();
    if let Err(payload) = result
        && let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload)))
    {
        std::mem::forget(secondary);
    }
    assert!(
        !escaped,
        "broadcast input destructor escaped unpolled cancellation"
    );
}
async fn disposed(counts: &Counts, drops: usize, payloads: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while counts.drops.load(Ordering::SeqCst) != drops
            || counts.payloads.load(Ordering::SeqCst) != payloads
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("broadcast input or panic payload did not retire");
}

struct Business;
fn receive(message: BoxedMessage) -> ActorResult<BoxedMessage> {
    if let Some(input) = message.downcast_ref::<Input>() {
        input.counts.delivered.fetch_add(1, Ordering::SeqCst);
    }
    if let Some(input) = message.downcast_ref::<TracedInput>() {
        input.record("receive");
    }
    Ok(Box::new(()))
}
impl Actor for Business {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { receive(message) })
    }
}
struct ActixBusiness;
impl Actor for ActixBusiness {
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
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { receive(message) })
    }
}
impl ActixEngineExt for ActixBusiness {}

#[tokio::test]
async fn raw_thread_unpolled_input_is_retired() {
    let system = ThreadActorSystem::shared(Default::default());
    let counts = Arc::new(Counts::default());
    cancel(system.broadcast_message(Box::new(input(&counts, false, true))));
    disposed(&counts, 1, 1).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 0);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn native_thread_unpolled_input_is_retired() {
    let system = ThreadActorSystem::shared(Default::default());
    let counts = Arc::new(Counts::default());
    cancel(system.broadcast(input(&counts, false, true)));
    disposed(&counts, 1, 1).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 0);
    system.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn native_actix_unpolled_input_is_retired() {
    let system = ActixActorSystem::new().await.unwrap();
    let counts = Arc::new(Counts::default());
    cancel(system.broadcast(input(&counts, false, true)));
    disposed(&counts, 1, 1).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 0);
    system.shutdown().await.unwrap();
}
#[tokio::test]
async fn forwarded_thread_unpolled_input_is_retired() {
    let system = ThreadActorSystem::shared(Default::default());
    let forwarded = ActorSystemImpl::Thread(system.clone());
    let counts = Arc::new(Counts::default());
    cancel(forwarded.broadcast(input(&counts, false, true)));
    disposed(&counts, 1, 1).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 0);
    system.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn forwarded_actix_unpolled_input_is_retired() {
    let system = ActixActorSystem::new().await.unwrap();
    let forwarded = ActorSystemImpl::Actix(system.clone());
    let counts = Arc::new(Counts::default());
    cancel(forwarded.broadcast(input(&counts, false, true)));
    disposed(&counts, 1, 1).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 0);
    system.shutdown().await.unwrap();
}
#[tokio::test]
async fn facade_unpolled_input_is_retired() {
    let system = ParrotActorSystem::new(Default::default()).await.unwrap();
    let counts = Arc::new(Counts::default());
    cancel(system.internal_broadcast(input(&counts, false, true)));
    disposed(&counts, 1, 1).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 0);
    system.shutdown_runtime().await.unwrap();
}
#[tokio::test]
async fn facade_trait_unpolled_input_is_retired() {
    let system = ParrotActorSystem::new(Default::default()).await.unwrap();
    let counts = Arc::new(Counts::default());
    cancel(system.broadcast(input(&counts, false, true)));
    disposed(&counts, 1, 1).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 0);
    system.shutdown_runtime().await.unwrap();
}

async fn thread_clone_failure() {
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_root_typed_thread(Business, EmptyConfig)
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let result = system.broadcast(input(&counts, true, true)).await;
    disposed(&counts, 1, 2).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("broadcast clone panicked"), "{error}");
    assert!(error.contains("disposal failed"), "{error}");
    assert_eq!(counts.clones.load(Ordering::SeqCst), 1);
    assert_eq!(counts.delivered.load(Ordering::SeqCst), 0);
    actor.send(Box::new(())).await.unwrap();
    system.shutdown_internal().await.unwrap();
}
async fn actix_clone_failure() {
    let system = ActixActorSystem::new().await.unwrap();
    let actor = system
        .spawn_root_typed(ActixBusiness, EmptyConfig)
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let result = system.broadcast(input(&counts, true, true)).await;
    disposed(&counts, 1, 2).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("broadcast clone panicked"), "{error}");
    assert!(error.contains("disposal failed"), "{error}");
    assert_eq!(counts.clones.load(Ordering::SeqCst), 1);
    assert_eq!(counts.delivered.load(Ordering::SeqCst), 0);
    actor.send(Box::new(())).await.unwrap();
    system.shutdown().await.unwrap();
}
#[path = "support/isolated.rs"]
mod isolated_child;

fn isolated(name: &str, work: impl FnOnce()) {
    isolated_child::run(
        name,
        "PARROT_BROADCAST_WORKER",
        Duration::from_secs(20),
        work,
    );
}
#[test]
#[should_panic(expected = "child did not execute its body exactly once")]
fn missing_broadcast_child_is_rejected() {
    isolated("__missing_broadcast_case__", || unreachable!());
}

#[test]
fn thread_clone_and_source_drop_panics_are_independent() {
    isolated(
        "thread_clone_and_source_drop_panics_are_independent",
        || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(thread_clone_failure())
        },
    );
}
#[test]
fn actix_clone_and_source_drop_panics_are_independent() {
    isolated("actix_clone_and_source_drop_panics_are_independent", || {
        actix::System::new().block_on(actix_clone_failure())
    });
}

#[tokio::test]
async fn thread_broadcast_delivers_each_value_once() {
    let system = ThreadActorSystem::shared(Default::default());
    let mut actors = Vec::new();
    for _ in 0..3 {
        actors.push(
            system
                .spawn_root_typed_thread(Business, EmptyConfig)
                .await
                .unwrap(),
        );
    }
    let counts = Arc::new(Counts::default());
    system
        .broadcast(input(&counts, false, false))
        .await
        .unwrap();
    for actor in actors {
        actor.send(Box::new(())).await.unwrap();
    }
    disposed(&counts, 4, 0).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 3);
    assert_eq!(counts.delivered.load(Ordering::SeqCst), 3);
    system.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn actix_broadcast_delivers_each_value_once() {
    let system = ActixActorSystem::new().await.unwrap();
    let mut actors = Vec::new();
    for _ in 0..3 {
        actors.push(
            system
                .spawn_root_typed(ActixBusiness, EmptyConfig)
                .await
                .unwrap(),
        );
    }
    let counts = Arc::new(Counts::default());
    system
        .broadcast(input(&counts, false, false))
        .await
        .unwrap();
    for actor in actors {
        actor.send(Box::new(())).await.unwrap();
    }
    disposed(&counts, 4, 0).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 3);
    assert_eq!(counts.delivered.load(Ordering::SeqCst), 3);
    system.shutdown().await.unwrap();
}

async fn entered(gate: &CloneGate) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !gate.entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded clone worker did not enter");
}
#[tokio::test]
async fn thread_blocked_clone_allows_host_progress_and_cancellation() {
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_root_typed_thread(Business, EmptyConfig)
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(CloneGate::default());
    let mut message = input(&counts, false, true);
    message.gate = Some(gate.clone());
    let owner = system.clone();
    let operation = tokio::spawn(async move { owner.broadcast(message).await });
    entered(&gate).await;
    // This is a current-thread runtime. The timer and cancellation must run
    // while the actual user Clone remains blocked on an OS condition variable.
    tokio::time::sleep(Duration::from_millis(10)).await;
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
    gate.release();
    disposed(&counts, 2, 1).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 1);
    assert_eq!(counts.delivered.load(Ordering::SeqCst), 0);
    actor.send(Box::new(())).await.unwrap();
    system.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn actix_blocked_clone_allows_host_progress_and_cancellation() {
    let system = ActixActorSystem::new().await.unwrap();
    let actor = system
        .spawn_root_typed(ActixBusiness, EmptyConfig)
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(CloneGate::default());
    let mut message = input(&counts, false, true);
    message.gate = Some(gate.clone());
    let owner = system.clone();
    let operation = tokio::spawn(async move { owner.broadcast(message).await });
    entered(&gate).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
    gate.release();
    disposed(&counts, 2, 1).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 1);
    assert_eq!(counts.delivered.load(Ordering::SeqCst), 0);
    actor.send(Box::new(())).await.unwrap();
    system.shutdown().await.unwrap();
}
#[tokio::test]
async fn thread_source_disposal_failure_is_reported_after_delivery() {
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_root_typed_thread(Business, EmptyConfig)
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let result = system.broadcast(input(&counts, false, true)).await;
    actor.send(Box::new(())).await.unwrap();
    disposed(&counts, 2, 1).await;
    assert!(result.unwrap_err().to_string().contains("disposal failed"));
    assert_eq!(counts.delivered.load(Ordering::SeqCst), 1);
    system.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn actix_source_disposal_failure_is_reported_after_delivery() {
    let system = ActixActorSystem::new().await.unwrap();
    let actor = system
        .spawn_root_typed(ActixBusiness, EmptyConfig)
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let result = system.broadcast(input(&counts, false, true)).await;
    actor.send(Box::new(())).await.unwrap();
    disposed(&counts, 2, 1).await;
    assert!(result.unwrap_err().to_string().contains("disposal failed"));
    assert_eq!(counts.delivered.load(Ordering::SeqCst), 1);
    system.shutdown().await.unwrap();
}

type Events = Arc<std::sync::Mutex<Vec<(&'static str, Option<TraceContext>)>>>;
struct TracedInput(Events);
impl TracedInput {
    fn record(&self, event: &'static str) {
        self.0
            .lock()
            .unwrap()
            .push((event, TraceContext::current()));
    }
}
impl Clone for TracedInput {
    fn clone(&self) -> Self {
        self.record("clone");
        Self(self.0.clone())
    }
}
impl Drop for TracedInput {
    fn drop(&mut self) {
        self.record("drop");
    }
}
impl Message for TracedInput {
    type Result = ();
}
fn check_trace(
    events: &Events,
    caller: Option<&TraceContext>,
    waiter: &TraceContext,
    copies: usize,
) {
    assert_eq!(TraceContext::current().as_ref(), Some(waiter));
    let events = events.lock().unwrap();
    assert_eq!(
        events.iter().filter(|(event, _)| *event == "clone").count(),
        copies
    );
    assert_eq!(
        events.iter().filter(|(event, _)| *event == "drop").count(),
        copies + 1
    );
    assert_eq!(
        events
            .iter()
            .filter(|(event, _)| *event == "receive")
            .count(),
        1
    );
    for (event, context) in events.iter() {
        if *event == "clone" {
            assert_eq!(context.as_ref(), caller, "clone changed caller identity");
        } else if *event == "receive" {
            match caller {
                Some(caller) => {
                    let context = context.as_ref().expect("traced delivery lost parent");
                    assert_eq!(context.trace_id(), caller.trace_id());
                    assert_eq!(context.parent_span_id(), Some(caller.span_id()));
                }
                None => assert!(context.is_none(), "untraced delivery inherited waiter"),
            }
        }
    }
    assert!(
        events
            .iter()
            .any(|(event, context)| *event == "drop" && context.as_ref() == caller),
        "source disposal lost caller identity"
    );
}
#[tokio::test]
async fn thread_clone_delivery_and_disposal_keep_call_context() {
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_root_typed_thread(Business, EmptyConfig)
        .await
        .unwrap();
    for caller in [Some(TraceContext::root()), None] {
        let events = Events::default();
        let operation = trace::in_scope(caller.clone(), || {
            system.broadcast(TracedInput(events.clone()))
        });
        let waiter = TraceContext::root();
        waiter
            .scope(async {
                operation.await.unwrap();
                actor.send(Box::new(())).await.unwrap();
                check_trace(&events, caller.as_ref(), &waiter, 1);
            })
            .await;
        assert!(TraceContext::current().is_none());
    }
    system.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn actix_clone_delivery_and_disposal_keep_call_context() {
    let system = ActixActorSystem::new().await.unwrap();
    let actor = system
        .spawn_root_typed(ActixBusiness, EmptyConfig)
        .await
        .unwrap();
    for caller in [Some(TraceContext::root()), None] {
        let events = Events::default();
        let operation = trace::in_scope(caller.clone(), || {
            system.broadcast(TracedInput(events.clone()))
        });
        let waiter = TraceContext::root();
        waiter
            .scope(async {
                operation.await.unwrap();
                actor.send(Box::new(())).await.unwrap();
                check_trace(&events, caller.as_ref(), &waiter, 1);
            })
            .await;
        assert!(TraceContext::current().is_none());
    }
    system.shutdown().await.unwrap();
}

async fn actix_cancel_prepared() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let mut actors = Vec::new();
    for _ in 0..2 {
        actors.push(
            system
                .spawn_root_typed(ActixBusiness, EmptyConfig)
                .await
                .unwrap(),
        );
    }
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(CloneGate {
        block_at: 2,
        ..Default::default()
    });
    let mut message = input(&counts, false, true);
    message.clone_drop_fails = true;
    message.gate = Some(gate.clone());
    let owner = system.clone();
    let operation = tokio::spawn(async move { owner.broadcast(message).await });
    entered(&gate).await;
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    gate.release();
    disposed(&counts, 3, 3).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 2);
    assert_eq!(counts.delivered.load(Ordering::SeqCst), 0);
    for actor in actors {
        actor.send(Box::new(())).await.unwrap();
    }
    system.shutdown().await.unwrap();
}
#[test]
fn actix_cancellation_retires_prepared_and_late_copies_independently() {
    isolated(
        "actix_cancellation_retires_prepared_and_late_copies_independently",
        || actix::System::new().block_on(actix_cancel_prepared()),
    );
}

async fn bounded_clones(system: ActorSystemImpl, actor: Box<dyn ActorRef>) {
    let first = Arc::new(Counts::default());
    let gate = Arc::new(CloneGate::default());
    let mut message = input(&first, false, true);
    message.gate = Some(gate.clone());
    let owner = system.clone();
    let operation = tokio::spawn(async move { owner.broadcast(message).await });
    entered(&gate).await;
    let second_counts = Arc::new(Counts::default());
    {
        let mut second = std::pin::pin!(system.broadcast(input(&second_counts, false, false)));
        assert!(futures::poll!(second.as_mut()).is_pending());
        assert_eq!(second_counts.clones.load(Ordering::SeqCst), 0);
        operation.abort();
        assert!(operation.await.unwrap_err().is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second.as_mut())
                .await
                .is_err(),
            "second broadcast completed while the first clone still held the only worker"
        );
        assert_eq!(
            second_counts.clones.load(Ordering::SeqCst),
            0,
            "caller cancellation released a still-running clone's permit"
        );
        gate.release();
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .unwrap()
            .unwrap();
    }
    actor.send(Box::new(())).await.unwrap();
    disposed(&first, 2, 1).await;
    disposed(&second_counts, 2, 0).await;
    assert_eq!(first.delivered.load(Ordering::SeqCst), 0);
    assert_eq!(second_counts.clones.load(Ordering::SeqCst), 1);
    assert_eq!(second_counts.delivered.load(Ordering::SeqCst), 1);
    system.shutdown().await.unwrap();
}
#[tokio::test]
async fn thread_clone_budget_survives_caller_cancellation() {
    let system = ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    let actor = system
        .spawn_root_typed_thread(Business, EmptyConfig)
        .await
        .unwrap();
    bounded_clones(ActorSystemImpl::Thread(system), Box::new(actor)).await;
}
#[actix::test]
async fn actix_clone_budget_survives_caller_cancellation() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let actor = system
        .spawn_root_typed(ActixBusiness, EmptyConfig)
        .await
        .unwrap();
    bounded_clones(ActorSystemImpl::Actix(system), actor).await;
}

async fn empty_and_stopped(system: ActorSystemImpl) {
    let healthy = Arc::new(Counts::default());
    system
        .broadcast(input(&healthy, false, false))
        .await
        .unwrap();
    disposed(&healthy, 1, 0).await;
    assert_eq!(healthy.clones.load(Ordering::SeqCst), 0);
    let empty = Arc::new(Counts::default());
    system
        .broadcast(input(&empty, false, true))
        .await
        .unwrap_err();
    disposed(&empty, 1, 1).await;
    assert_eq!(empty.clones.load(Ordering::SeqCst), 0);
    system.clone().shutdown().await.unwrap();
    let stopped = Arc::new(Counts::default());
    let error = system
        .broadcast(input(&stopped, false, true))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("shutting down"), "{error}");
    assert!(error.contains("disposal failed"), "{error}");
    disposed(&stopped, 1, 1).await;
    assert_eq!(stopped.clones.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn thread_empty_and_stopped_broadcasts_retire_without_cloning() {
    empty_and_stopped(ActorSystemImpl::Thread(ThreadActorSystem::shared(
        Default::default(),
    )))
    .await;
}
#[actix::test]
async fn actix_empty_and_stopped_broadcasts_retire_without_cloning() {
    empty_and_stopped(ActorSystemImpl::Actix(
        ActixActorSystem::new().await.unwrap(),
    ))
    .await;
}

#[tokio::test]
async fn facade_broadcast_keeps_context_across_engine_clones() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_root_typed_thread(Business, EmptyConfig)
        .await
        .unwrap();
    facade
        .register_thread_system("broadcast".into(), system, true)
        .await
        .unwrap();
    for caller in [Some(TraceContext::root()), None] {
        let events = Events::default();
        let operation = trace::in_scope(caller.clone(), || {
            facade.internal_broadcast(TracedInput(events.clone()))
        });
        let waiter = TraceContext::root();
        waiter
            .scope(async {
                operation.await.unwrap();
                actor.send(Box::new(())).await.unwrap();
                check_trace(&events, caller.as_ref(), &waiter, 2);
            })
            .await;
        assert!(TraceContext::current().is_none());
    }
    facade.shutdown_runtime().await.unwrap();
}
