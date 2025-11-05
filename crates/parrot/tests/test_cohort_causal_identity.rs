use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, ActorSpawnerExt},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
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

type Event = (usize, usize, &'static str, Option<TraceContext>);
struct State {
    next: [AtomicUsize; 2],
    events: Mutex<Vec<Event>>,
    ready: tokio::sync::watch::Sender<[Option<usize>; 2]>,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next: [AtomicUsize::new(0), AtomicUsize::new(0)],
            events: Mutex::default(),
            ready: tokio::sync::watch::channel([None, None]).0,
        })
    }
    fn note(&self, member: usize, generation: usize, phase: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((member, generation, phase, TraceContext::current()));
    }
    async fn wait(&self, generation: usize) {
        let mut ready = self.ready.subscribe();
        loop {
            let observed = *ready.borrow_and_update();
            if observed == [Some(generation); 2] {
                return;
            }
            assert!(
                observed
                    .iter()
                    .all(|item| item.is_none_or(|item| item <= generation))
            );
            ready.changed().await.unwrap();
        }
    }
}
struct Make(usize);
struct Fail(bool);
type Receipt = (usize, Option<TraceContext>);

macro_rules! actors {
    ($module:ident, $context:ty) => {
        mod $module {
            use super::*;
            pub(super) struct Parent(pub Arc<State>);
            impl Actor for Parent {
                type Config = EmptyConfig;
                type Context = $context;
                fn state(&self) -> ActorState {
                    ActorState::Running
                }
                fn use_async_handler(&self) -> bool {
                    true
                }
                fn init<'a>(
                    &'a mut self,
                    ctx: &'a mut Self::Context,
                ) -> BoxedFuture<'a, ActorResult<()>> {
                    let state = self.0.clone();
                    ctx.set_supervisor_strategy(SupervisorStrategyType::OneForAll(
                        OneForAllStrategy {
                            max_restarts: 2,
                            within: Duration::from_secs(60),
                            decider: BasicDecisionFn::new(move |_| {
                                // Each epoch is triggered by a different child.
                                let generation = state
                                    .events
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .filter(|event| event.2 == "policy")
                                    .count();
                                state.note(generation, generation, "policy");
                                SupervisionDecision::Restart
                            }),
                        },
                    ));
                    Box::pin(async { Ok(()) })
                }
                fn receive_message<'a>(
                    &'a mut self,
                    message: BoxedMessage,
                    ctx: &'a mut Self::Context,
                ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                    Box::pin(async move {
                        let member = message.downcast::<Make>().unwrap().0;
                        self.0.note(member, 0, "creation-handler");
                        let state = self.0.clone();
                        let reference = ctx
                            .spawner()
                            .spawn_factory(move || {
                                let generation = state.next[member].fetch_add(1, Ordering::SeqCst);
                                state.note(member, generation, "constructor");
                                Leaf {
                                    state: state.clone(),
                                    member,
                                    generation,
                                }
                            })
                            .await?;
                        Ok(Box::new(reference) as BoxedMessage)
                    })
                }
            }
            pub(super) struct Leaf {
                state: Arc<State>,
                member: usize,
                generation: usize,
            }
            impl Actor for Leaf {
                type Config = EmptyConfig;
                type Context = $context;
                fn state(&self) -> ActorState {
                    ActorState::Running
                }
                fn use_async_handler(&self) -> bool {
                    true
                }
                fn init<'a>(
                    &'a mut self,
                    _: &'a mut Self::Context,
                ) -> BoxedFuture<'a, ActorResult<()>> {
                    self.state.note(self.member, self.generation, "init-call");
                    Box::pin(async move {
                        self.state.note(self.member, self.generation, "init-poll");
                        tokio::task::yield_now().await;
                        self.state
                            .note(self.member, self.generation, "init-resumed");
                        self.state
                            .ready
                            .send_modify(|ready| ready[self.member] = Some(self.generation));
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
                            self.state
                                .note(self.member, self.generation, "failure-call");
                            tokio::task::yield_now().await;
                            self.state
                                .note(self.member, self.generation, "failure-resumed");
                            assert!(!failure.0, "deliberate cohort causal probe panic");
                            return Err(ActorError::MessageHandlingError(
                                "deliberate cohort causal probe error".into(),
                            ));
                        }
                        Ok(Box::new((self.generation, TraceContext::current())) as BoxedMessage)
                    })
                }
            }
        }
    };
}
actors!(thread_actors, parrot::thread::ThreadContext<Self>);
actors!(
    actix_actors,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
impl parrot_api::actor::ActixEngineExt for actix_actors::Parent {}
impl parrot_api::actor::ActixEngineExt for actix_actors::Leaf {}

async fn exercise(
    parent: &dyn parrot_api::address::ActorRef,
    state: &State,
    tell: bool,
    panic: bool,
    traced: bool,
    poll_traced: bool,
) -> Vec<Option<TraceContext>> {
    let ambient = poll_traced.then(TraceContext::root);
    let mut children: Vec<BoxedActorRef> = Vec::new();
    for member in 0..2 {
        let creation = TraceContext::root();
        let work = creation.in_scope(|| parent.send(Box::new(Make(member))));
        children.push(
            *trace::scope(ambient.clone(), work)
                .await
                .unwrap()
                .downcast::<BoxedActorRef>()
                .unwrap(),
        );
    }
    state.wait(0).await;
    let mut causes = Vec::new();
    for epoch in 0..2 {
        // The next recovery must replace, rather than reuse, the prior identity.
        let cause = (if epoch == 0 { traced } else { !traced }).then(TraceContext::root);
        let child = &children[epoch];
        if tell {
            let work = trace::in_scope(cause.clone(), || child.deliver(Box::new(Fail(panic))));
            trace::scope(ambient.clone(), work).await.unwrap();
        } else {
            let work = trace::in_scope(cause.clone(), || child.send(Box::new(Fail(panic))));
            assert!(trace::scope(ambient.clone(), work).await.is_err());
        }
        state.wait(epoch + 1).await;
        for sibling in &children {
            let work = trace::in_scope(None, || sibling.send(Box::new(())));
            let receipt = trace::scope(ambient.clone(), work)
                .await
                .unwrap()
                .downcast::<Receipt>()
                .unwrap();
            assert_eq!(
                receipt.0,
                epoch + 1,
                "stable references must reach the new cohort"
            );
            assert!(
                receipt.1.is_none(),
                "cohort cause must not leak into later messages"
            );
            let independent = TraceContext::root();
            let work = independent.in_scope(|| sibling.send(Box::new(())));
            let receipt = trace::scope(ambient.clone(), work)
                .await
                .unwrap()
                .downcast::<Receipt>()
                .unwrap();
            assert_eq!(receipt.0, epoch + 1);
            let observed = receipt.1.as_ref().unwrap();
            assert_eq!(observed.trace_id(), independent.trace_id());
            assert_ne!(observed.span_id(), independent.span_id());
        }
        causes.push(cause);
    }
    causes
}

fn verify(state: &State, causes: &[Option<TraceContext>]) {
    assert!(TraceContext::current().is_none());
    for count in &state.next {
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }
    let events = state.events.lock().unwrap();
    let phase = |member, generation, name| {
        let found: Vec<_> = events
            .iter()
            .filter(|event| event.0 == member && event.1 == generation && event.2 == name)
            .collect();
        assert_eq!(
            found.len(),
            1,
            "each phase occurs once: {member}/{generation}/{name}"
        );
        found[0].3.clone()
    };
    let mut expected = Vec::new();
    let mut actual = Vec::new();
    for member in 0..2 {
        for name in ["constructor", "init-call", "init-poll", "init-resumed"] {
            assert!(phase(member, 0, "creation-handler").is_some());
            assert_eq!(
                phase(member, 0, name),
                phase(member, 0, "creation-handler"),
                "initial child creation ancestry"
            );
        }
    }
    for (epoch, cause) in causes.iter().enumerate() {
        let failure = phase(epoch, epoch, "failure-resumed");
        assert_eq!(phase(epoch, epoch, "failure-call"), failure);
        match (cause, &failure) {
            (None, None) => {}
            (Some(parent), Some(child)) => {
                assert_eq!(parent.trace_id(), child.trace_id());
                assert_ne!(parent.span_id(), child.span_id());
            }
            _ => panic!("failing handler ancestry differs"),
        }
        assert_eq!(
            phase(epoch, epoch, "policy"),
            failure,
            "group decision sees its actual cause"
        );
        for member in 0..2 {
            for name in ["constructor", "init-call", "init-poll", "init-resumed"] {
                let observed = phase(member, epoch + 1, name);
                if cause.is_none() {
                    assert!(
                        observed.is_none(),
                        "untraced cohort must erase prior recovery and ambient identities"
                    );
                }
                actual.push((epoch, member, name, observed));
                expected.push((epoch, member, name, failure.clone()));
            }
        }
        if cause.is_none() {
            println!("PARROT_COHORT_UNTRACED_CONTROL_OK epoch={epoch}");
        }
    }
    assert_eq!(
        actual, expected,
        "cohort construction and initialization must retain the triggering failure identity"
    );
}

async fn scenario(actix: bool, tell: bool, panic: bool, traced: bool, poll_traced: bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let state = State::new();
        let causes;
        if actix {
            let system = parrot::actix::ActixActorSystem::with_arbiter_count(1).await.unwrap();
            let parent = system.spawn_root_typed(actix_actors::Parent(state.clone()), EmptyConfig).await.unwrap();
            causes = exercise(parent.as_ref(), &state, tell, panic, traced, poll_traced).await;
            parent.stop().await.unwrap();
            system.clone().shutdown().await.unwrap();
            assert_eq!(system.status().active_actors, 0);
        } else {
            let system = parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
                shared_pool_size: 2, shared_burst_workers_max: 0, ..Default::default()
            });
            let parent = system.spawn_at(thread_actors::Parent(state.clone()), "/parent", None,
                parrot::thread::ThreadActorConfig::default()).await.unwrap();
            causes = exercise(&parent, &state, tell, panic, traced, poll_traced).await;
            parent.stop().await.unwrap();
            system.shutdown_internal().await.unwrap();
            assert_eq!(system.actor_count(), 0);
        }
        verify(&state, &causes);
        println!("PARROT_COHORT_CAUSAL_CASE_OK actix={actix} tell={tell} panic={panic} traced={traced} poll_traced={poll_traced}");
    }).await.expect("cohort independent liveness bound");
}
async fn matrix(actix: bool, tell: bool, panic: bool) {
    for traced in [false, true] {
        for poll_traced in [false, true] {
            scenario(actix, tell, panic, traced, poll_traced).await;
        }
    }
}
macro_rules! case {
    ($name:ident, $actix:literal, $tell:literal, $panic:literal) => {
        #[actix::test]
        async fn $name() {
            matrix($actix, $tell, $panic).await;
        }
    };
}
case!(thread_ask_error, false, false, false);
case!(thread_ask_panic, false, false, true);
case!(thread_tell_error, false, true, false);
case!(thread_tell_panic, false, true, true);
case!(actix_ask_error, true, false, false);
case!(actix_ask_panic, true, false, true);
case!(actix_tell_error, true, true, false);
case!(actix_tell_panic, true, true, true);
