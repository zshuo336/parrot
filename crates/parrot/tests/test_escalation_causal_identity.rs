use parrot::thread::{
    config::{SchedulingMode, SupervisorStrategy, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, ActorSpawnerExt},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
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
    fn new() -> Arc<Self> {
        let (ready, _) = tokio::sync::watch::channel(None);
        Arc::new(Self {
            next: AtomicUsize::new(0),
            events: Mutex::new(Vec::new()),
            ready,
        })
    }
    fn note(&self, generation: usize, phase: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((generation, phase, identity()));
    }
}
fn policy(state: Arc<State>, parent: bool) -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: usize::from(parent) as u32,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |error| {
            assert!(matches!(
                error,
                ActorError::MessageHandlingError(_) | ActorError::Panic(_)
            ));
            state.note(
                0,
                if parent {
                    "parent-decider"
                } else {
                    "child-decider"
                },
            );
            if parent {
                SupervisionDecision::Restart
            } else {
                SupervisionDecision::Escalate
            }
        }),
    })
}
struct Generation {
    number: usize,
    state: Arc<State>,
}
impl Generation {
    fn new(state: Arc<State>) -> Self {
        let number = state.next.fetch_add(1, Ordering::SeqCst);
        state.note(number, "constructor");
        Self { number, state }
    }
}
struct MakeParent(Arc<State>);
struct MakeChild;
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
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<MakeParent>() {
                let state = message.downcast::<MakeParent>().unwrap().0;
                let strategy = policy(state.clone(), true);
                let parent = ctx
                    .spawner()
                    .spawn_factory_with_strategy(move || Self::new(state.clone()), strategy)
                    .await?;
                return Ok(Box::new(parent) as BoxedMessage);
            }
            if message.is::<MakeChild>() {
                let child = ctx
                    .spawner()
                    .spawn_supervised(
                        Probe(self.state.clone()),
                        EmptyConfig,
                        policy(self.state.clone(), false),
                    )
                    .await?;
                return Ok(Box::new(child) as BoxedMessage);
            }
            Ok(Box::new(Receipt {
                generation: self.number,
                identity: identity(),
            }) as BoxedMessage)
        })
    }
}
struct Probe(Arc<State>);
impl Actor for Probe {
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
        Box::pin(async move {
            let failure = message.downcast::<Fail>().unwrap();
            self.0.note(0, "failure-handler");
            tokio::task::yield_now().await;
            self.0.note(0, "failure-resumed");
            assert!(!failure.panic, "deliberate causal escalation probe panic");
            Err(ActorError::MessageHandlingError(
                "deliberate causal escalation probe error".into(),
            ))
        })
    }
}
async fn await_generation(state: &State, generation: usize) {
    let mut ready = state.ready.subscribe();
    loop {
        let current = *ready.borrow_and_update();
        if current == Some(generation) {
            return;
        }
        assert!(current.is_none_or(|number| number < generation));
        ready.changed().await.expect("generation readiness sender");
    }
}
async fn scenario(mode: SchedulingMode, traced: bool, tell: bool, panic: bool, managed: bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            max_dedicated_threads: 4,
            default_scheduling_mode: mode.clone(),
            ..Default::default()
        });
        let state = State::new();
        let original = TraceContext::root();
        let ambient = TraceContext::root();
        let parent: BoxedActorRef = if managed {
            let host = system.spawn_supervised(
                { let host_state = State::new(); move || Generation::new(host_state.clone()) },
                "/host", SupervisorStrategy::Stop,
            ).await.unwrap();
            let creation = original.in_scope(|| host.send(Box::new(MakeParent(state.clone()))));
            *ambient.scope(creation).await.unwrap().downcast::<BoxedActorRef>().unwrap()
        } else {
            let made = state.clone();
            let creation = original.in_scope(|| system.spawn_supervised_with_config(
                move || Generation::new(made.clone()), "/parent", ThreadActorConfig {
                    supervisor_strategy: Some(SupervisorStrategy::Restart {
                        max_retries: 1, within: Duration::from_secs(60),
                    }),
                    ..Default::default()
                },
            ));
            Box::new(ambient.scope(creation).await.unwrap())
        };
        await_generation(&state, 0).await;
        let child = *parent.send(Box::new(MakeChild)).await.unwrap().downcast::<BoxedActorRef>().unwrap();
        let cause = traced.then(TraceContext::root);
        let failure: BoxedMessage = Box::new(Fail { panic });
        if tell {
            let delivery = trace::in_scope(cause.clone(), || child.deliver(failure));
            ambient.scope(delivery).await.expect("failure TELL admitted");
        } else {
            let delivery = trace::in_scope(cause.clone(), || child.send(failure));
            assert!(ambient.scope(delivery).await.is_err(), "failure ASK reports failure");
        }
        await_generation(&state, 1).await;
        let receipt = parent.send(Box::new(())).await.unwrap().downcast::<Receipt>().unwrap();
        assert_eq!(receipt.generation, 1);
        assert!(receipt.identity.is_none(), "escalation cause must not leak into later messages");
        let followup = TraceContext::root();
        let receipt = followup.scope(async {
            parent.send(Box::new(())).await.unwrap().downcast::<Receipt>().unwrap()
        }).await;
        assert_eq!(receipt.generation, 1);
        assert_eq!(receipt.identity.unwrap().trace, followup.trace_id());
        assert_ne!(receipt.identity.unwrap().span, followup.span_id());
        assert!(identity().is_none(), "runtime restores caller context");
        let events = state.events.lock().unwrap().clone();
        // Retire actors before the negative control's identity assertions.
        system.shutdown_internal().await.unwrap();
        assert_eq!(system.actor_count(), 0);
        assert_eq!(state.next.load(Ordering::SeqCst), 2);
        let phase = |generation, name| {
            let matches: Vec<_> = events.iter().filter(|row| row.0 == generation && row.1 == name).collect();
            assert_eq!(matches.len(), 1, "each phase executes once: {generation}/{name}");
            matches[0].2
        };
        let initial = phase(0, "constructor").unwrap();
        assert_eq!(initial.trace, original.trace_id());
        for name in ["init-call", "init-poll", "init-resumed"] {
            assert_eq!(phase(0, name), Some(initial), "initial creation ancestry");
        }
        let failed = phase(0, "failure-handler");
        assert_eq!(phase(0, "failure-resumed"), failed);
        match (&cause, failed) {
            (Some(parent), Some(observed)) => {
                assert_eq!(observed.trace, parent.trace_id());
                assert_ne!(observed.span, parent.span_id());
            }
            (None, None) => {}
            mismatch => panic!("failure handler ancestry mismatch: {mismatch:?}"),
        }
        let mut actual = vec![("child-decider", phase(0, "child-decider"))];
        if managed {
            actual.push(("parent-decider", phase(0, "parent-decider")));
        } else {
            assert!(!events.iter().any(|row| row.1 == "parent-decider"));
        }
        for name in ["constructor", "init-call", "init-poll", "init-resumed"] {
            actual.push((name, phase(1, name)));
        }
        let expected: Vec<_> = actual.iter().map(|(name, _)| (*name, failed)).collect();
        assert_eq!(actual, expected, "escalation decisions and replacement must inherit the child failure identity");
        println!("PARROT_ESCALATION_CAUSAL_CASE_OK {mode:?}/traced={traced}/tell={tell}/panic={panic}/managed={managed}");
    }).await.expect("causal escalation must complete within the liveness bound");
}
async fn matrix(tell: bool, panic: bool, managed: bool) {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 64,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "causal-escalation".into(),
        },
    ] {
        for traced in [false, true] {
            scenario(mode.clone(), traced, tell, panic, managed).await;
        }
    }
}
macro_rules! case {
    ($name:ident, $tell:literal, $panic:literal, $managed:literal) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            matrix($tell, $panic, $managed).await;
        }
    };
}
case!(
    direct_tell_error_preserves_escalation_identity,
    true,
    false,
    false
);
case!(
    direct_ask_error_preserves_escalation_identity,
    false,
    false,
    false
);
case!(
    direct_tell_panic_preserves_escalation_identity,
    true,
    true,
    false
);
case!(
    direct_ask_panic_preserves_escalation_identity,
    false,
    true,
    false
);
case!(
    managed_tell_error_preserves_escalation_identity,
    true,
    false,
    true
);
case!(
    managed_ask_error_preserves_escalation_identity,
    false,
    false,
    true
);
case!(
    managed_tell_panic_preserves_escalation_identity,
    true,
    true,
    true
);
case!(
    managed_ask_panic_preserves_escalation_identity,
    false,
    true,
    true
);
