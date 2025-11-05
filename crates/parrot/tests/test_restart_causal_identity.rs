use parrot::thread::{
    config::{SchedulingMode, SupervisorStrategy, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    trace: [u8; 16],
    span: [u8; 8],
}
fn identity() -> Option<Identity> {
    TraceContext::current().map(|context| Identity {
        trace: context.trace_id(),
        span: context.span_id(),
    })
}
type Event = (usize, &'static str, Option<Identity>);
struct State {
    next: AtomicUsize,
    events: Mutex<Vec<Event>>,
    ready: tokio::sync::watch::Sender<Option<usize>>,
}
impl State {
    fn note(&self, generation: usize, phase: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((generation, phase, identity()));
    }
    fn snapshot(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}
struct Generation {
    number: usize,
    state: Arc<State>,
}
struct Fail {
    panic: bool,
}
struct Receipt {
    generation: usize,
    identity: Option<Identity>,
}
impl Actor for Generation {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        if message.is::<Fail>() {
            self.state.note(self.number, "batch-admission");
            true
        } else {
            false
        }
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.state.note(self.number, "init-call");
        Box::pin(async move {
            self.state.note(self.number, "init-poll");
            tokio::task::yield_now().await;
            self.state.note(self.number, "init-resumed");
            self.state.ready.send_replace(Some(self.number));
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Ok(failure) = message.downcast::<Fail>() {
                self.state.note(self.number, "failure-handler");
                tokio::task::yield_now().await;
                self.state.note(self.number, "failure-resumed");
                assert!(!failure.panic, "deliberate causal restart probe panic");
                return Err(ActorError::MessageHandlingError(
                    "deliberate causal restart probe error".into(),
                ));
            }
            Ok(Box::new(Receipt {
                generation: self.number,
                identity: identity(),
            }) as BoxedMessage)
        })
    }
}

async fn await_generation(state: &State, generation: usize) {
    let mut ready = state.ready.subscribe();
    loop {
        let current = *ready.borrow_and_update();
        if current == Some(generation) {
            break;
        }
        assert!(current.is_none_or(|number| number < generation));
        ready.changed().await.expect("generation readiness sender");
    }
}

async fn scenario(mode: SchedulingMode, traced: bool, tell: bool, panic: bool, batched: bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            max_dedicated_threads: 2,
            ..Default::default()
        });
        let (ready, _) = tokio::sync::watch::channel(None);
        let state = Arc::new(State {
            next: AtomicUsize::new(0),
            events: Mutex::new(Vec::new()),
            ready,
        });
        let original = TraceContext::root();
        let initial_identity = original.in_scope(identity);
        let ambient = TraceContext::root();
        let factory_state = state.clone();
        let creation = original.in_scope(|| {
            system.spawn_supervised_with_config(
                move || {
                    let number = factory_state.next.fetch_add(1, Ordering::SeqCst);
                    factory_state.note(number, "constructor");
                    Generation {
                        number,
                        state: factory_state.clone(),
                    }
                },
                "/causal-restart",
                ThreadActorConfig {
                    scheduling_mode: Some(mode.clone()),
                    message_batch_size: Some(if batched { 8 } else { 1 }),
                    supervisor_strategy: Some(SupervisorStrategy::Restart {
                        max_retries: 1,
                        within: Duration::from_secs(60),
                    }),
                    ..Default::default()
                },
            )
        });
        let actor = ambient.scope(creation).await.unwrap();
        await_generation(&state, 0).await;
        // Creation is a different operation from the failure that causes restart.
        let cause = traced.then(TraceContext::root);
        let failing: BoxedMessage = Box::new(Fail { panic });
        let erased = actor.clone_boxed();
        if tell {
            let delivery = trace::in_scope(cause.clone(), || erased.deliver(failing));
            ambient
                .scope(delivery)
                .await
                .expect("failure TELL admitted");
        } else {
            let delivery = trace::in_scope(cause.clone(), || actor.send(failing));
            assert!(
                ambient.scope(delivery).await.is_err(),
                "failure ASK must report failure"
            );
        }
        await_generation(&state, 1).await;
        // Read the actual replacement generation through the stable reference.
        let receipt = actor
            .send(Box::new(()))
            .await
            .unwrap()
            .downcast::<Receipt>()
            .unwrap();
        assert_eq!(receipt.generation, 1);
        assert!(
            receipt.identity.is_none(),
            "restart cause must not leak into later messages"
        );
        let followup = TraceContext::root();
        let receipt = followup
            .scope(async {
                erased
                    .send(Box::new(()))
                    .await
                    .unwrap()
                    .downcast::<Receipt>()
                    .unwrap()
            })
            .await;
        assert_eq!(receipt.generation, 1);
        assert_eq!(receipt.identity.unwrap().trace, followup.trace_id());
        assert_ne!(receipt.identity.unwrap().span, followup.span_id());
        assert!(
            identity().is_none(),
            "runtime operation restored caller context"
        );
        let events = state.snapshot();
        // Shutdown before assertions so the negative control also retires actors.
        system.shutdown_internal().await.unwrap();
        assert_eq!(system.actor_count(), 0);
        assert_eq!(state.next.load(Ordering::SeqCst), 2);
        let phase = |generation, name| {
            let matches: Vec<_> = events
                .iter()
                .filter(|row| row.0 == generation && row.1 == name)
                .collect();
            assert_eq!(
                matches.len(),
                1,
                "each causal phase executes once: {generation}/{name}"
            );
            matches[0].2
        };
        for name in ["constructor", "init-call", "init-poll", "init-resumed"] {
            assert_eq!(
                phase(0, name),
                initial_identity,
                "initial creation ancestry"
            );
        }
        let failure_identity = phase(0, "failure-handler");
        assert_eq!(phase(0, "failure-resumed"), failure_identity);
        if batched {
            assert_eq!(phase(0, "batch-admission"), failure_identity,
                "batch eligibility and batch handler must share invocation identity");
        } else {
            assert!(!events.iter().any(|row| row.1 == "batch-admission"));
        }
        match (cause, failure_identity) {
            (Some(parent), Some(observed)) => {
                assert_eq!(observed.trace, parent.trace_id());
                assert_ne!(observed.span, parent.span_id());
            }
            (None, None) => {}
            mismatch => panic!("failure handler ancestry mismatch: {mismatch:?}"),
        }
        for name in ["constructor", "init-call", "init-poll", "init-resumed"] {
            assert_eq!(
                phase(1, name),
                failure_identity,
                "replacement generation must inherit the failing handler identity: {name}"
            );
        }
        println!(
            "PARROT_RESTART_CAUSAL_CASE_OK {mode:?}/traced={traced}/tell={tell}/panic={panic}/batched={batched}"
        );
    })
    .await
    .expect("causal restart must complete within the liveness bound");
}

async fn matrix(tell: bool, panic: bool, batched: bool) {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 64,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "causal-restart".into(),
        },
    ] {
        // Untraced first provides a direct control even when the traced case fails.
        for traced in [false, true] {
            scenario(mode.clone(), traced, tell, panic, batched).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tell_error_restarts_under_failure_identity() {
    matrix(true, false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_error_restarts_under_failure_identity() {
    matrix(false, false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tell_panic_restarts_under_failure_identity() {
    matrix(true, true, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_panic_restarts_under_failure_identity() {
    matrix(false, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_tell_error_restarts_under_failure_identity() {
    matrix(true, false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_ask_error_restarts_under_failure_identity() {
    matrix(false, false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_tell_panic_restarts_under_failure_identity() {
    matrix(true, true, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_ask_panic_restarts_under_failure_identity() {
    matrix(false, true, true).await;
}
