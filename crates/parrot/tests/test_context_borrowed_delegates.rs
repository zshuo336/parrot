//! Invoked, genuinely pending context delegates retain their borrowed target.
use parrot::thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorRef, RetirementStatus},
    context::ActorContext,
    errors::ActorError,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(3);
type Event = (&'static str, Option<[u8; 16]>);

#[derive(Clone, Copy, Debug)]
enum Route {
    Send,
    Ask,
    Watch,
    Unwatch,
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Cancel,
    Success,
    Error,
}
#[derive(Debug, Default)]
struct State {
    invoked: AtomicUsize,
    pending: AtomicUsize,
    completed: AtomicUsize,
    delegate_drops: AtomicUsize,
    target_drops: AtomicUsize,
    input_drops: AtomicUsize,
    reply_drops: AtomicUsize,
    panic_payload_drops: AtomicUsize,
    handler_returns: AtomicUsize,
    handler_drops: AtomicUsize,
    actor_drops: AtomicUsize,
    release: AtomicBool,
    waker: Mutex<Option<Waker>>,
    events: Mutex<Vec<Event>>,
    results: Mutex<Vec<String>>,
}
impl State {
    fn note(&self, stage: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((stage, TraceContext::current().map(|trace| trace.trace_id())));
    }
    fn release(&self) {
        self.release.store(true, Ordering::SeqCst);
        let waker = self.waker.lock().unwrap().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct RecursivePanic(Arc<State>);
impl Drop for RecursivePanic {
    fn drop(&mut self) {
        self.0.note("panic-payload-drop");
        self.0.panic_payload_drops.fetch_add(1, Ordering::SeqCst);
        panic!("borrowed delegate panic payload destructor");
    }
}
struct Input(Arc<State>);
impl Drop for Input {
    fn drop(&mut self) {
        self.0.note("input-drop");
        self.0.input_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Reply(Arc<State>, bool);
impl Drop for Reply {
    fn drop(&mut self) {
        self.0.note("reply-drop");
        self.0.reply_drops.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            std::panic::panic_any(RecursivePanic(self.0.clone()));
        }
    }
}

#[derive(Debug)]
struct Target {
    state: Arc<State>,
    route: Route,
    outcome: Outcome,
    hostile: bool,
}
impl Target {
    fn invoke<'a, T: Send + 'static, I: Send + Unpin + 'static>(
        &'a self,
        input: I,
        output: impl FnOnce() -> ActorResult<T> + Send + 'a,
    ) -> BoxedFuture<'a, ActorResult<T>> {
        self.state.note("invoke");
        self.state.invoked.fetch_add(1, Ordering::SeqCst);
        Box::pin(Delegate {
            target: self,
            input: Some(input),
            output: Some(Box::new(output)),
        })
    }
    fn watcher<'a>(&'a self, watcher: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        assert_eq!(watcher.path(), "/parent");
        self.invoke(watcher, || Ok(()))
    }
}
impl Drop for Target {
    fn drop(&mut self) {
        self.state.note("target-drop");
        assert_eq!(self.state.delegate_drops.load(Ordering::SeqCst), 1);
        self.state.target_drops.fetch_add(1, Ordering::SeqCst);
        if self.hostile {
            std::panic::panic_any(RecursivePanic(self.state.clone()));
        }
    }
}
struct Delegate<'a, I, T> {
    target: &'a Target,
    input: Option<I>,
    output: Option<Box<dyn FnOnce() -> ActorResult<T> + Send + 'a>>,
}
impl<I: Unpin, T> Future for Delegate<'_, I, T> {
    type Output = ActorResult<T>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let state = &self.target.state;
        assert_eq!(state.target_drops.load(Ordering::SeqCst), 0);
        let mut registered = state.waker.lock().unwrap();
        if !state.release.load(Ordering::SeqCst) {
            *registered = Some(cx.waker().clone());
            drop(registered);
            state.note("pending");
            state.pending.fetch_add(1, Ordering::SeqCst);
            return Poll::Pending;
        }
        let stale = registered.take();
        drop(registered);
        drop(stale);
        state.note("complete");
        state.completed.fetch_add(1, Ordering::SeqCst);
        if self.target.outcome == Outcome::Error {
            Poll::Ready(Err(ActorError::MessageHandlingError(
                "delegate business error".into(),
            )))
        } else {
            Poll::Ready(self.output.take().unwrap()())
        }
    }
}
impl<I, T> Drop for Delegate<'_, I, T> {
    fn drop(&mut self) {
        let state = &self.target.state;
        assert_eq!(state.target_drops.load(Ordering::SeqCst), 0);
        drop(self.input.take());
        state.note("delegate-drop");
        state.delegate_drops.fetch_add(1, Ordering::SeqCst);
        if self.target.hostile {
            std::panic::panic_any(RecursivePanic(state.clone()));
        }
    }
}
impl ActorRef for Target {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("context ask must supply its configured timeout")
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(matches!(self.route, Route::Ask));
        assert_eq!(timeout, Some(Duration::from_secs(7)));
        self.invoke(message, || {
            Ok(Box::new(Reply(self.state.clone(), self.hostile)) as BoxedMessage)
        })
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        assert!(matches!(self.route, Route::Send));
        self.invoke(message, || Ok(()))
    }
    fn watch<'a>(&'a self, watcher: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        assert!(matches!(self.route, Route::Watch));
        self.watcher(watcher)
    }
    fn unwatch<'a>(&'a self, watcher: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        assert!(matches!(self.route, Route::Unwatch));
        self.watcher(watcher)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/borrowed-target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        panic!("context must keep the admitted target")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct Operation {
    route: Route,
    outcome: Outcome,
    hostile: bool,
}
struct Handler(Arc<State>);
impl Drop for Handler {
    fn drop(&mut self) {
        self.0.note("handler-drop");
        self.0.handler_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Parent(Arc<State>);
impl Drop for Parent {
    fn drop(&mut self) {
        self.0.actor_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Parent {
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
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if !message.is::<Operation>() {
                return Ok(message);
            }
            let request = *message.downcast::<Operation>().unwrap();
            let _handler = Handler(self.0.clone());
            let target = Box::new(Target {
                state: self.0.clone(),
                route: request.route,
                outcome: request.outcome,
                hostile: request.hostile,
            });
            let result = match request.route {
                Route::Send => context.send(target, Box::new(Input(self.0.clone()))).await,
                Route::Ask => context
                    .ask(target, Box::new(Input(self.0.clone())))
                    .await
                    .map(|reply| {
                        assert!(reply.is::<Reply>());
                        drop(reply);
                    }),
                Route::Watch => context.watch(target).await,
                Route::Unwatch => context.unwatch(target).await,
            };
            self.0.results.lock().unwrap().push(format!("{result:?}"));
            self.0.handler_returns.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("contract did not progress");
}
fn check(sharded: bool, route: Route, outcome: Outcome) {
    for traced in [false, true] {
        for hostile in [false, true] {
            let host = runtime();
            let system = ThreadActorSystem::try_shared_with_handle(
                ThreadActorSystemConfig {
                    shared_pool_size: 1,
                    shared_burst_workers_max: 0,
                    default_scheduling_mode: if sharded {
                        SchedulingMode::Sharded {
                            affinity_key: "borrowed-delegates".into(),
                        }
                    } else {
                        SchedulingMode::DedicatedThread
                    },
                    default_ask_timeout: Duration::from_secs(7),
                    shutdown_timeout: WAIT,
                    ..Default::default()
                },
                host.handle().clone(),
            )
            .unwrap();
            let state = Arc::new(State::default());
            let neighbour_state = Arc::new(State::default());
            let (parent, neighbour) = host.block_on(async {
                let parent = system
                    .spawn_at(Parent(state.clone()), "/parent", None, Default::default())
                    .await
                    .unwrap();
                let neighbour = system
                    .spawn_at(
                        Parent(neighbour_state.clone()),
                        "/neighbour",
                        None,
                        Default::default(),
                    )
                    .await
                    .unwrap();
                (parent, neighbour)
            });
            let processor = Arc::downgrade(
                &system
                    .get_mailbox("/parent")
                    .unwrap()
                    .get_processor()
                    .unwrap(),
            );
            drop(host);
            let driver = runtime();
            let cause = traced.then(TraceContext::root);
            driver.block_on(async {
                let sending = {
                    let parent = parent.clone();
                    let cause = cause.clone();
                    tokio::spawn(async move {
                        trace::scope(cause, async {
                            parent.send(Box::new(Operation { route, outcome, hostile })).await
                        }).await
                    })
                };
                until(|| state.pending.load(Ordering::SeqCst) > 0).await;
                assert_eq!(state.invoked.load(Ordering::SeqCst), 1);
                assert_eq!(state.completed.load(Ordering::SeqCst), 0);
                assert_eq!(state.delegate_drops.load(Ordering::SeqCst), 0);
                assert_eq!(state.target_drops.load(Ordering::SeqCst), 0);
                assert_eq!(state.input_drops.load(Ordering::SeqCst), 0);
                assert_eq!(state.handler_returns.load(Ordering::SeqCst), 0);
                assert_eq!(*neighbour.send(Box::new(31u32)).await.unwrap().downcast::<u32>().unwrap(), 31);
                if outcome == Outcome::Cancel {
                    system.get_mailbox("/parent").unwrap().get_processor().unwrap().cancel_processing();
                    until(|| state.handler_drops.load(Ordering::SeqCst) == 1).await;
                    assert!(tokio::time::timeout(WAIT, sending).await.unwrap().unwrap().is_err());
                } else {
                    state.release();
                    tokio::time::timeout(WAIT, sending).await.unwrap().unwrap().unwrap();
                }
                trace::scope(Some(TraceContext::root()), async {
                    let mut stopped = None;
                    for attempt in 0..2 {
                        let started = Instant::now();
                        let result = parent.stop().await;
                        let diagnostic = || {
                            let processor = processor.upgrade()
                                .map(|processor| (processor.is_initialized(), processor.has_failed(), processor.get_statistics()));
                            format!("sharded={sharded} route={route:?} outcome={outcome:?} traced={traced} hostile={hostile} attempt={attempt} elapsed={:?} retirement={:?} processor(initialized,failed,stats)={processor:?} state={state:?} neighbour={neighbour_state:?}",
                                started.elapsed(), parent.retirement_status())
                        };
                        let receipt = format!("{result:?}");
                        if let Some(previous) = &stopped {
                            assert_eq!(&receipt, previous, "retirement receipt changed; {}", diagnostic());
                        }
                        stopped = Some(receipt);
                        if hostile {
                            assert!(matches!(&result, Err(ActorError::Panic(text))
                                if text.contains("context message future disposal")
                                && text.contains("panic payload destructor panicked")), "{result:?}; {}", diagnostic());
                            assert!(matches!(parent.retirement_status(), RetirementStatus::Retired(Err(_))), "{}", diagnostic());
                        } else if outcome == Outcome::Cancel {
                            assert!(matches!(&result, Err(ActorError::Other(error))
                                if ["Actor system is already shutting down",
                                    "Failed during shutdown: actor /parent execution cancelled"]
                                    .contains(&error.to_string().as_str())), "{result:?}; {}", diagnostic());
                            assert!(matches!(parent.retirement_status(), RetirementStatus::Retired(Err(_))), "{}", diagnostic());
                        } else {
                            assert!(result.is_ok(), "{result:?}; {}", diagnostic());
                            assert!(matches!(parent.retirement_status(), RetirementStatus::Retired(Ok(()))), "{}", diagnostic());
                        }
                    }
                    assert_eq!(*neighbour.send(Box::new(43u32)).await.unwrap().downcast::<u32>().unwrap(), 43);
                    neighbour.stop().await.unwrap();
                    assert_eq!(system.actor_count(), 0);
                    for _ in 0..2 {
                        let result = system.shutdown_internal().await;
                        assert!(matches!(result, Err(parrot::thread::error::SystemError::ShutdownError(ref text))
                            if text == "1 shared cleanup failure(s): shared task unexpectedly cancelled"), "{result:?}");
                    }
                }).await;
            });
            assert_eq!(state.actor_drops.load(Ordering::SeqCst), 1);
            assert_eq!(neighbour_state.actor_drops.load(Ordering::SeqCst), 1);
            assert_eq!(state.delegate_drops.load(Ordering::SeqCst), 1);
            assert_eq!(state.target_drops.load(Ordering::SeqCst), 1);
            assert_eq!(state.handler_drops.load(Ordering::SeqCst), 1);
            let completed = usize::from(outcome != Outcome::Cancel);
            assert_eq!(state.completed.load(Ordering::SeqCst), completed);
            assert_eq!(state.handler_returns.load(Ordering::SeqCst), completed);
            assert_eq!(
                state.input_drops.load(Ordering::SeqCst),
                usize::from(matches!(route, Route::Send | Route::Ask))
            );
            let replies = usize::from(matches!(route, Route::Ask) && outcome == Outcome::Success);
            assert_eq!(state.reply_drops.load(Ordering::SeqCst), replies);
            assert_eq!(
                state.panic_payload_drops.load(Ordering::SeqCst),
                if hostile { 2 + replies } else { 0 }
            );
            let results = state.results.lock().unwrap();
            match outcome {
                Outcome::Cancel => assert!(results.is_empty()),
                Outcome::Error => assert_eq!(
                    *results,
                    ["Err(MessageHandlingError(\"delegate business error\"))"]
                ),
                Outcome::Success if !hostile => assert_eq!(*results, ["Ok(())"]),
                Outcome::Success => {
                    assert_eq!(results.len(), 1);
                    assert!(
                        results[0].contains("context message future disposal"),
                        "{results:?}"
                    );
                }
            }
            let events = state.events.lock().unwrap();
            let expected = cause.map(|trace| trace.trace_id());
            assert!(!events.is_empty());
            assert!(
                events.iter().all(|(_, trace)| *trace == expected),
                "{events:?}"
            );
            let position = |stage| events.iter().position(|(name, _)| *name == stage).unwrap();
            assert!(position("delegate-drop") < position("target-drop"));
            assert!(position("target-drop") < position("handler-drop"));
        }
    }
}
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_BORROWED_DELEGATE_CASE").as_deref() == Ok(name) {
        body();
        let path = std::env::var_os("PARROT_BORROWED_DELEGATE_RECEIPT").unwrap();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        std::io::Write::write_all(&mut file, name.as_bytes()).unwrap();
        return;
    }
    let receipt =
        std::env::temp_dir().join(format!("parrot-borrowed-delegate-{}", uuid::Uuid::new_v4()));
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_BORROWED_DELEGATE_CASE", name)
        .env("PARROT_BORROWED_DELEGATE_RECEIPT", &receipt)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + WAIT * 10;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: {status}");
            assert_eq!(
                std::fs::read_to_string(&receipt).unwrap(),
                name,
                "child did not execute contract"
            );
            std::fs::remove_file(&receipt).unwrap();
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name}: watchdog expired");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! case {
    ($name:ident, $sharded:expr, $route:ident, $outcome:ident) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                check($sharded, Route::$route, Outcome::$outcome)
            });
        }
    };
}
case!(dedicated_send_cancel, false, Send, Cancel);
case!(dedicated_send_success, false, Send, Success);
case!(dedicated_send_error, false, Send, Error);
case!(dedicated_ask_cancel, false, Ask, Cancel);
case!(dedicated_ask_success, false, Ask, Success);
case!(dedicated_ask_error, false, Ask, Error);
case!(dedicated_watch_cancel, false, Watch, Cancel);
case!(dedicated_watch_success, false, Watch, Success);
case!(dedicated_watch_error, false, Watch, Error);
case!(dedicated_unwatch_cancel, false, Unwatch, Cancel);
case!(dedicated_unwatch_success, false, Unwatch, Success);
case!(dedicated_unwatch_error, false, Unwatch, Error);
case!(sharded_send_cancel, true, Send, Cancel);
case!(sharded_send_success, true, Send, Success);
case!(sharded_send_error, true, Send, Error);
case!(sharded_ask_cancel, true, Ask, Cancel);
case!(sharded_ask_success, true, Ask, Success);
case!(sharded_ask_error, true, Ask, Error);
case!(sharded_watch_cancel, true, Watch, Cancel);
case!(sharded_watch_success, true, Watch, Success);
case!(sharded_watch_error, true, Watch, Error);
case!(sharded_unwatch_cancel, true, Unwatch, Cancel);
case!(sharded_unwatch_success, true, Unwatch, Success);
case!(sharded_unwatch_error, true, Unwatch, Error);
