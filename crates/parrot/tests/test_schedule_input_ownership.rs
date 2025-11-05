//! Registration owns each input before polling and rejects before publishing work.
use parrot::actix::ActixContext;
use parrot::thread::context::ThreadContext;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorPath, ActorRef},
    context::{ActorContext, ActorContextScheduler},
    errors::ActorError,
    message::{CloneableMessage, Message},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone)]
struct Payload {
    drops: Arc<AtomicUsize>,
    panic: bool,
}
impl Message for Payload {
    type Result = ();
}
impl Drop for Payload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "public schedule message destructor panic");
    }
}
#[derive(Debug)]
struct Target {
    drops: Arc<AtomicUsize>,
    panic: bool,
}
impl Drop for Target {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "public schedule target destructor panic");
    }
}
impl ActorRef for Target {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("unused ask")
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(msg)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("unpolled or invalid timer delivered")
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/schedule-target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            drops: self.drops.clone(),
            panic: self.panic,
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct ThreadProbe;
impl Actor for ThreadProbe {
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
        Box::pin(async { Ok(Box::new(()) as BoxedMessage) })
    }
}
struct ActixProbe;
impl actix::Actor for ActixProbe {
    type Context = actix::Context<Self>;
}
async fn wait_count(count: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while count.load(Ordering::SeqCst) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("input disposal did not finish");
}
async fn unpolled<C: ActorContext + ActorContextScheduler>(context: &C, kind: &str) {
    let messages = Arc::new(AtomicUsize::new(0));
    let targets = Arc::new(AtomicUsize::new(0));
    let message = Payload {
        drops: messages.clone(),
        panic: true,
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if kind.ends_with("generic") {
            drop(context.schedule(message, Duration::from_secs(3600), None));
        } else {
            let target = Box::new(Target {
                drops: targets.clone(),
                panic: true,
            });
            if kind.ends_with("periodic") {
                drop(context.schedule_periodic(
                    target,
                    CloneableMessage::from_message(message),
                    Duration::from_secs(3600),
                    Duration::from_secs(1),
                ));
            } else {
                drop(context.schedule_once(target, Box::new(message), Duration::from_secs(3600)));
            }
        }
    }));
    assert!(outcome.is_ok(), "unpolled registration unwound on caller");
    wait_count(&messages, 1).await;
    wait_count(&targets, usize::from(!kind.ends_with("generic"))).await;
    // No admission was attempted; an ordinary timer still registers normally.
    context
        .schedule_once(
            Box::new(Target {
                drops: Arc::default(),
                panic: false,
            }),
            Box::new(()),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
}
fn overflow_delay() -> Duration {
    let wall = std::time::SystemTime::now();
    let (mut low, mut high) = (0, u64::MAX);
    while low < high {
        let middle = low + (high - low) / 2 + 1;
        if wall.checked_add(Duration::from_secs(middle)).is_some() {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let delay = Duration::from_secs(low.checked_add(60).unwrap());
    assert!(
        tokio::time::Instant::now().checked_add(delay).is_some(),
        "platform has no tested clock-range gap"
    );
    assert!(std::time::SystemTime::now().checked_add(delay).is_none());
    delay
}
async fn clock<C: ActorContext + ActorContextScheduler>(context: &C) {
    let drops = Arc::new(AtomicUsize::new(0));
    let result = context
        .schedule(
            Payload {
                drops: drops.clone(),
                panic: false,
            },
            overflow_delay(),
            None,
        )
        .await;
    assert!(matches!(result, Err(ActorError::InternalError(ref s)) if s.contains("clock range")));
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "rejected clock left message owned by a launched timer"
    );
    // All default slots must remain usable. No yield occurs between registrations
    // on this current-thread runtime, so background cleanup cannot mask a leak.
    for _ in 0..parrot_config::Defaults::max_context_tasks() {
        context
            .schedule_once(
                Box::new(Target {
                    drops: Arc::default(),
                    panic: false,
                }),
                Box::new(()),
                Duration::from_secs(3600),
            )
            .await
            .expect("rejected clock consumed a task slot");
    }
}
fn isolated(case: &str) {
    const FLAG: &str = "PARROT_PUBLIC_SCHEDULE_INPUT";
    if std::env::var(FLAG).as_deref() == Ok(case) {
        actix::System::new().block_on(async {
            if case.starts_with("thread") {
                let context = ThreadContext::<ThreadProbe>::new_for_test("/schedule-input");
                if case.ends_with("clock") {
                    clock(&context).await;
                } else {
                    unpolled(&context, case).await;
                }
            } else {
                use actix::Actor as _;
                let context =
                    ActixContext::new(ActixProbe.start(), ActorPath::for_test("/schedule-input"));
                if case.ends_with("clock") {
                    clock(&context).await;
                } else {
                    unpolled(&context, case).await;
                }
            }
        });
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", case, "--nocapture"])
        .env(FLAG, case)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{case}: {status}");
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{case}: deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! case {
    ($name:ident) => {
        #[test]
        fn $name() {
            isolated(stringify!($name));
        }
    };
}
case!(thread_unpolled_once);
case!(thread_unpolled_periodic);
case!(thread_unpolled_generic);
case!(actix_unpolled_once);
case!(actix_unpolled_periodic);
case!(actix_unpolled_generic);
case!(thread_invalid_clock);
case!(actix_invalid_clock);

// Public context construction permits application-supplied self references.
// Keep abort-capable callback/destructor cases in real child test processes.
mod generated_schedule_inputs {
    use super::*;
    use futures::FutureExt;
    use std::panic::AssertUnwindSafe;

    #[derive(Debug, Default)]
    struct Counts {
        clones: AtomicUsize,
        references: AtomicUsize,
        messages: AtomicUsize,
        deliveries: AtomicUsize,
        payloads: AtomicUsize,
    }
    #[derive(Debug)]
    struct RecursivePayload(Arc<Counts>);
    impl Drop for RecursivePayload {
        fn drop(&mut self) {
            self.0.payloads.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(RecursivePayload(self.0.clone()));
        }
    }
    #[derive(Debug)]
    struct SelfRef {
        counts: Arc<Counts>,
        case: &'static str,
        generation: usize,
    }
    impl Drop for SelfRef {
        fn drop(&mut self) {
            if self.generation != 0 {
                self.counts.references.fetch_add(1, Ordering::SeqCst);
                if self.generation == 1 && self.case.ends_with("drop") {
                    panic!("generated reference cleanup");
                }
            }
        }
    }
    impl ActorRef for SelfRef {
        fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            panic!("schedule must deliver without asking")
        }
        fn send_with_timeout<'a>(
            &'a self,
            m: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(m)
        }
        fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                assert!(self.case.starts_with("healthy"), "rejected timer delivered");
                self.counts.deliveries.fetch_add(1, Ordering::SeqCst);
                drop(message);
                Ok(())
            })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "/generated-schedule".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            let next = self.counts.clones.fetch_add(1, Ordering::SeqCst) + 1;
            if self.case == "first_payload" && next == 1 {
                std::panic::panic_any(RecursivePayload(self.counts.clone()));
            }
            if (self.case == "first_clone" && next == 1)
                || (self.case.starts_with("second_clone") && next == 2)
            {
                panic!("self-reference clone callback");
            }
            Box::new(Self {
                counts: self.counts.clone(),
                case: self.case,
                generation: next,
            })
        }
    }
    #[derive(Clone)]
    struct GeneratedMessage(Arc<Counts>);
    impl Message for GeneratedMessage {
        type Result = ();
    }
    impl Drop for GeneratedMessage {
        fn drop(&mut self) {
            self.0.messages.fetch_add(1, Ordering::SeqCst);
        }
    }
    async fn exercise<C: ActorContext + ActorContextScheduler>(
        context: &C,
        counts: &Arc<Counts>,
        case: &'static str,
    ) {
        let periodic = case.ends_with("periodic");
        let rejected = case.starts_with("rejected");
        let interval = if periodic {
            Some(Duration::from_secs(3600))
        } else if rejected {
            Some(Duration::ZERO)
        } else {
            None
        };
        let result = AssertUnwindSafe(context.schedule(
            GeneratedMessage(counts.clone()),
            Duration::ZERO,
            interval,
        ))
        .catch_unwind()
        .await;
        assert!(
            result.is_ok(),
            "self-reference callback/destructor unwound out of schedule"
        );
        let result = result.unwrap();
        if case.starts_with("healthy") {
            let task = result.expect("healthy registration");
            assert_eq!(task.target.path(), "/generated-schedule");
            wait_count(&counts.deliveries, 1).await;
            context.cancel_schedule(task.clone());
            drop(task);
            wait_count(&counts.references, 2).await;
            wait_count(&counts.messages, if periodic { 2 } else { 1 }).await;
            assert_eq!(counts.clones.load(Ordering::SeqCst), 2);
        } else {
            let error = result.expect_err("fault/rejection must not admit a timer");
            if rejected {
                assert!(
                    matches!(error, ActorError::InternalError(ref text) if text.contains("interval")),
                    "{error:?}"
                );
            } else {
                assert!(
                    matches!(error, ActorError::Panic(ref text) if text.contains(if case == "first_payload" { "panic payload destructor panicked" } else { "self-reference clone callback" })),
                    "{error:?}"
                );
            }
            // Failure is acknowledged only after each generated input retires.
            assert_eq!(counts.messages.load(Ordering::SeqCst), 1);
            assert_eq!(
                counts.references.load(Ordering::SeqCst),
                if rejected {
                    2
                } else if case.starts_with("second") {
                    1
                } else {
                    0
                }
            );
            assert_eq!(counts.deliveries.load(Ordering::SeqCst), 0);
            assert_eq!(
                counts.payloads.load(Ordering::SeqCst),
                usize::from(case == "first_payload")
            );
            // The failed registration must not poison or consume context slots.
            for _ in 0..parrot_config::Defaults::max_context_tasks() {
                context
                    .schedule_once(
                        Box::new(Target {
                            drops: Arc::default(),
                            panic: false,
                        }),
                        Box::new(()),
                        Duration::from_secs(3600),
                    )
                    .await
                    .unwrap();
            }
        }
    }
    fn isolated(name: &str, engine: &str, case: &'static str) {
        const FLAG: &str = "PARROT_GENERATED_SCHEDULE_INPUT";
        if std::env::var(FLAG).as_deref() == Ok(name) {
            actix::System::new().block_on(async {
                let counts = Arc::new(Counts::default());
                let target = SelfRef {
                    counts: counts.clone(),
                    case,
                    generation: 0,
                };
                if engine == "thread" {
                    let mut context =
                        ThreadContext::<ThreadProbe>::new_for_test("/generated-schedule");
                    context.set_self_ref(Box::new(target));
                    exercise(&context, &counts, case).await;
                } else {
                    use actix::Actor as _;
                    let context = ActixContext::new(
                        ActixProbe.start(),
                        ActorPath::new(Arc::new(target), "/generated-schedule".into()),
                    );
                    exercise(&context, &counts, case).await;
                }
            });
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(FLAG, name)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{name}: {status}");
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{name}: deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    macro_rules! contract {
        ($name:ident, $engine:literal, $case:literal) => {
            #[test]
            fn $name() {
                isolated(
                    concat!("generated_schedule_inputs::", stringify!($name)),
                    $engine,
                    $case,
                );
            }
        };
    }
    contract!(thread_first_clone, "thread", "first_clone");
    contract!(thread_first_payload, "thread", "first_payload");
    contract!(thread_second_clone, "thread", "second_clone");
    contract!(thread_second_clone_drop, "thread", "second_clone_drop");
    contract!(thread_rejected_drop, "thread", "rejected_drop");
    contract!(thread_rejected_healthy, "thread", "rejected_healthy");
    contract!(thread_healthy_once, "thread", "healthy_once");
    contract!(thread_healthy_periodic, "thread", "healthy_periodic");
    contract!(actix_first_clone, "actix", "first_clone");
    contract!(actix_first_payload, "actix", "first_payload");
    contract!(actix_second_clone, "actix", "second_clone");
    contract!(actix_second_clone_drop, "actix", "second_clone_drop");
    contract!(actix_rejected_drop, "actix", "rejected_drop");
    contract!(actix_rejected_healthy, "actix", "rejected_healthy");
    contract!(actix_healthy_once, "actix", "healthy_once");
    contract!(actix_healthy_periodic, "actix", "healthy_periodic");
}
