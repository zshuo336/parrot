//! The public creation future itself may be dropped without ever being polled.
//! Admission and constructor/initialization polling are distinct boundaries.
use parrot::{
    actix::ActixActorSystem,
    thread::ThreadActorSystem,
    typed_context::{TypedContext, TypedContextExt},
};
use parrot_api::{
    Message,
    context::ReadOnlyChildrenVec,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive},
    typed_context::TypedContextServices,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

// Destruction may unwind with a payload whose destructor itself panics.
struct BadPayload;
impl Drop for BadPayload {
    fn drop(&mut self) {
        panic!("secondary payload disposal");
    }
}
struct Capture {
    drops: Arc<AtomicUsize>,
    hostile: bool,
    gate: Option<std::sync::mpsc::Receiver<()>>,
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.recv_timeout(Duration::from_secs(5))
                .expect("caller was blocked by input disposal");
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.hostile {
            std::panic::panic_any(BadPayload);
        }
    }
}
// Mutex makes the gated capture Sync, as required by repeatable constructors.
struct Probe(Mutex<Capture>, Arc<AtomicUsize>);
struct Query;
impl Message for Query {
    type Result = usize;
}
impl TypedReceive<Query> for Probe {
    fn receive_typed(&mut self, _: Query) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async { Ok(42) })
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Query;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, query: Query) -> BoxedFuture<'_, ActorResult<usize>> {
        self.receive_typed(query)
    }
}
impl ParrotMsgVariant<Probe> for Query {
    fn inject(query: Self) -> Query {
        query
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}
fn probe(drops: &Arc<AtomicUsize>, init: &Arc<AtomicUsize>, hostile: bool) -> Probe {
    Probe(
        Mutex::new(Capture {
            drops: drops.clone(),
            hostile,
            gate: None,
        }),
        init.clone(),
    )
}
fn policy(drops: &Arc<AtomicUsize>, hostile: bool) -> OneForOneStrategy {
    let capture = Mutex::new(Capture {
        drops: drops.clone(),
        hostile,
        gate: None,
    });
    OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    }
}
fn factory(
    drops: &Arc<AtomicUsize>,
    called: &Arc<AtomicUsize>,
) -> impl Fn() -> Probe + Send + Sync + 'static {
    let capture = Mutex::new(Capture {
        drops: drops.clone(),
        hostile: true,
        gate: None,
    });
    let called = called.clone();
    move || {
        let _ = &capture;
        called.fetch_add(1, Ordering::SeqCst);
        probe(&Arc::new(AtomicUsize::new(0)), &called, false)
    }
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(4), future)
        .await
        .expect("retirement made no progress")
}
async fn retired(drops: &Arc<AtomicUsize>, expected: usize) {
    if tokio::runtime::Handle::try_current().is_err() {
        assert_eq!(drops.load(Ordering::SeqCst), expected);
        return;
    }
    bounded(async {
        while drops.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(drops.load(Ordering::SeqCst), expected);
}
fn cancel<F: Future + Send>(future: F) {
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok(),
        "public unpolled creation future unwound through its caller"
    );
}
fn isolated(name: &str, run: impl FnOnce()) {
    const FLAG: &str = "PARROT_STATIC_INPUT_CHILD";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        run();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(FLAG, name)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: isolated child failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name}: isolated child exceeded deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn runtime(run: impl Future<Output = ()>) {
    actix::System::new().block_on(run);
}
fn gated_probe(
    d: &Arc<AtomicUsize>,
    c: &Arc<AtomicUsize>,
    gate: Option<std::sync::mpsc::Receiver<()>>,
) -> Probe {
    let mut actor = probe(d, c, gate.is_none());
    actor.0.get_mut().unwrap().gate = gate;
    actor
}
fn gated_factory(
    d: &Arc<AtomicUsize>,
    c: &Arc<AtomicUsize>,
    gate: Option<std::sync::mpsc::Receiver<()>>,
) -> impl Fn() -> Probe + Send + Sync + 'static {
    let captured = gated_probe(d, c, gate);
    let calls = c.clone();
    move || {
        let _ = &captured;
        calls.fetch_add(1, Ordering::SeqCst);
        probe(&Arc::new(AtomicUsize::new(0)), &calls, false)
    }
}
macro_rules! root_cases {
    ($system:expr, $mode:expr, $drops:expr, $calls:expr, $gate:expr) => {{
        let s = $system;
        let d = $drops;
        let c = $calls;
        match $mode {
            0 => cancel(s.spawn_typed::<Probe, Query>(gated_probe(d, c, $gate), "/unpolled")),
            1 => cancel(s.spawn_typed_with_strategy::<Probe, Query>(
                gated_probe(d, c, $gate),
                "/unpolled",
                SupervisorStrategyType::OneForOne(policy(d, true)),
            )),
            2 => {
                cancel(s.spawn_typed_single::<Probe, Query>(gated_probe(d, c, $gate), "/unpolled"))
            }
            3 => cancel(s.spawn_typed_single_with_strategy::<Probe, Query>(
                gated_probe(d, c, $gate),
                "/unpolled",
                SupervisorStrategyType::OneForOne(policy(d, true)),
            )),
            4 => cancel(
                s.spawn_typed_factory::<Probe, Query, _>(gated_factory(d, c, $gate), "/unpolled"),
            ),
            5 => cancel(s.spawn_typed_factory_with_strategy::<Probe, Query, _>(
                gated_factory(d, c, $gate),
                "/unpolled",
                policy(d, true),
            )),
            6 => cancel(s.spawn_typed_single_factory::<Probe, Query, _>(
                gated_factory(d, c, $gate),
                "/unpolled",
            )),
            7 => cancel(
                s.spawn_typed_single_factory_with_strategy::<Probe, Query, _>(
                    gated_factory(d, c, $gate),
                    "/unpolled",
                    policy(d, true),
                ),
            ),
            _ => unreachable!(),
        }
    }};
}
async fn root(actix: bool, mode: usize) {
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    // First hostile disposal, then blocking disposal. The release signal is
    // sent only AFTER the public future has been dropped, never by a timer.
    if actix {
        let s = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
        root_cases!(&s, mode, &drops, &calls, None);
        retired(&drops, 1 + mode % 2).await;
        let (release, gate) = std::sync::mpsc::channel();
        root_cases!(&s, mode, &drops, &calls, Some(gate));
        release.send(()).unwrap();
        retired(&drops, 2 * (1 + mode % 2)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(s.status().active_actors, 0);
        s.as_ref().clone().shutdown().await.unwrap();
    } else {
        let s = ThreadActorSystem::shared(Default::default());
        root_cases!(&s, mode, &drops, &calls, None);
        retired(&drops, 1 + mode % 2).await;
        let (release, gate) = std::sync::mpsc::channel();
        root_cases!(&s, mode, &drops, &calls, Some(gate));
        release.send(()).unwrap();
        retired(&drops, 2 * (1 + mode % 2)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(s.actor_count(), 0);
        s.shutdown_internal().await.unwrap();
    }
}
macro_rules! root_test {
    ($name:ident, $actix:expr, $mode:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || runtime(root($actix, $mode)));
        }
    };
}
root_test!(thread_instance_unpolled, false, 0);
root_test!(thread_instance_policy_unpolled, false, 1);
root_test!(thread_single_unpolled, false, 2);
root_test!(thread_single_policy_unpolled, false, 3);
root_test!(thread_factory_unpolled, false, 4);
root_test!(thread_factory_policy_unpolled, false, 5);
root_test!(thread_single_factory_unpolled, false, 6);
root_test!(thread_single_factory_policy_unpolled, false, 7);
root_test!(actix_instance_unpolled, true, 0);
root_test!(actix_instance_policy_unpolled, true, 1);
root_test!(actix_single_unpolled, true, 2);
root_test!(actix_single_policy_unpolled, true, 3);
root_test!(actix_factory_unpolled, true, 4);
root_test!(actix_factory_policy_unpolled, true, 5);
root_test!(actix_single_factory_unpolled, true, 6);
root_test!(actix_single_factory_policy_unpolled, true, 7);

struct Foreign;
impl TypedContextServices for Foreign {
    fn path(&self) -> &str {
        "/foreign"
    }
    fn actor_ref(&self) -> Option<BoxedActorRef> {
        None
    }
    fn parent(&self) -> Option<BoxedActorRef> {
        None
    }
    fn children(&self) -> ReadOnlyChildrenVec {
        ReadOnlyChildrenVec::new(Arc::new(std::sync::RwLock::new(Vec::new())))
    }
    fn stop(&self) {}
    fn set_child_strategy(&self, _: SupervisorStrategyType) {}
}
async fn foreign(mode: usize, polled: bool) {
    let ctx = TypedContext::from_services(Arc::new(Foreign));
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let future = match mode {
        0 => ctx.spawn_typed_child::<Probe, Query>(probe(&drops, &calls, true)),
        1 => ctx.spawn_typed_child_with_strategy::<Probe, Query>(
            probe(&drops, &calls, true),
            SupervisorStrategyType::OneForOne(policy(&drops, true)),
        ),
        2 => ctx.spawn_typed_child_factory::<Probe, Query, _>(factory(&drops, &calls)),
        3 => ctx.spawn_typed_child_factory_with_strategy::<Probe, Query, _>(
            factory(&drops, &calls),
            SupervisorStrategyType::OneForOne(policy(&drops, true)),
        ),
        _ => unreachable!(),
    };
    if polled {
        use futures::FutureExt;
        let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
        let error = match result {
            Ok(Err(error)) => error,
            _ => panic!("foreign input failure must return an error without unwinding"),
        };
        assert!(matches!(error, parrot_api::errors::ActorError::Panic(_)));
        assert!(
            error
                .to_string()
                .contains("panic payload destructor panicked"),
            "{error}"
        );
    } else {
        cancel(future);
    }
    retired(&drops, 1 + mode % 2).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
macro_rules! child_test {
    ($name:ident, $mode:expr, $polled:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || runtime(foreign($mode, $polled)));
        }
    };
}
child_test!(foreign_instance_unpolled, 0, false);
child_test!(foreign_instance_policy_unpolled, 1, false);
child_test!(foreign_factory_unpolled, 2, false);
child_test!(foreign_factory_policy_unpolled, 3, false);
child_test!(foreign_instance_rejected, 0, true);
child_test!(foreign_instance_policy_rejected, 1, true);
child_test!(foreign_factory_rejected, 2, true);
child_test!(foreign_factory_policy_rejected, 3, true);

struct Parent;
struct GetContext;
impl Message for GetContext {
    type Result = TypedContext;
}
impl ParrotMsgVariant<Parent> for GetContext {
    fn inject(q: Self) -> Self {
        q
    }
    fn extract(ctx: TypedContext) -> ActorResult<TypedContext> {
        Ok(ctx)
    }
}
impl ParrotTypedDispatch for Parent {
    type Msg = GetContext;
    type Reply = TypedContext;
    fn dispatch(&mut self, _: GetContext) -> BoxedFuture<'_, ActorResult<TypedContext>> {
        unreachable!("managed dispatch must supply context")
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        _: GetContext,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<TypedContext>> {
        Box::pin(async move { Ok(ctx.clone()) })
    }
}
async fn check_managed(ctx: &TypedContext, mode: usize) {
    let d = Arc::new(AtomicUsize::new(0));
    let c = Arc::new(AtomicUsize::new(0));
    for blocking in [false, true] {
        let (release, gate) = std::sync::mpsc::channel();
        let gate = blocking.then_some(gate);
        match mode {
            0 => cancel(ctx.spawn_typed_child::<Probe, Query>(gated_probe(&d, &c, gate))),
            1 => cancel(ctx.spawn_typed_child_with_strategy::<Probe, Query>(
                gated_probe(&d, &c, gate),
                SupervisorStrategyType::OneForOne(policy(&d, true)),
            )),
            2 => cancel(
                ctx.spawn_typed_child_factory::<Probe, Query, _>(gated_factory(&d, &c, gate)),
            ),
            3 => cancel(
                ctx.spawn_typed_child_factory_with_strategy::<Probe, Query, _>(
                    gated_factory(&d, &c, gate),
                    SupervisorStrategyType::OneForOne(policy(&d, true)),
                ),
            ),
            _ => unreachable!(),
        }
        if blocking {
            release.send(()).unwrap();
        }
        retired(&d, (1 + usize::from(blocking)) * (1 + mode % 2)).await;
        assert_eq!(c.load(Ordering::SeqCst), 0);
        assert!(ctx.children().is_empty());
    }
    // Positive control: a polled factory creates a real child with this parent.
    let child = ctx
        .spawn_typed_child_factory::<Probe, Query, _>(move || probe(&d, &c, false))
        .await
        .unwrap();
    assert_eq!(child.ask(Query).await.unwrap(), 42);
    assert!(child.path().starts_with(&format!("{}/", ctx.path())));
    assert_eq!(ctx.children().len(), 1);
    child.stop().await.unwrap();
    assert!(ctx.children().is_empty());
}
async fn managed(actix: bool, mode: usize) {
    if actix {
        let s = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
        let parent = s
            .spawn_typed::<Parent, GetContext>(Parent, "/parent")
            .await
            .unwrap();
        check_managed(&parent.ask(GetContext).await.unwrap(), mode).await;
        parent.stop().await.unwrap();
        s.as_ref().clone().shutdown().await.unwrap();
    } else {
        let s = ThreadActorSystem::shared(Default::default());
        let parent = s
            .spawn_typed::<Parent, GetContext>(Parent, "/parent")
            .await
            .unwrap();
        check_managed(&parent.ask(GetContext).await.unwrap(), mode).await;
        parent.stop().await.unwrap();
        s.shutdown_internal().await.unwrap();
    }
}
macro_rules! managed_test {
    ($name:ident, $actix:expr, $mode:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || runtime(managed($actix, $mode)));
        }
    };
}
managed_test!(thread_child_instance_unpolled, false, 0);
managed_test!(thread_child_instance_policy_unpolled, false, 1);
managed_test!(thread_child_factory_unpolled, false, 2);
managed_test!(thread_child_factory_policy_unpolled, false, 3);
managed_test!(actix_child_instance_unpolled, true, 0);
managed_test!(actix_child_instance_policy_unpolled, true, 1);
managed_test!(actix_child_factory_unpolled, true, 2);
managed_test!(actix_child_factory_policy_unpolled, true, 3);

#[test]
fn foreign_without_executor_retires_inputs() {
    isolated("foreign_without_executor_retires_inputs", || {
        assert!(tokio::runtime::Handle::try_current().is_err());
        for mode in 0..4 {
            futures::executor::block_on(foreign(mode, false));
            futures::executor::block_on(foreign(mode, true));
        }
    });
}

async fn cancel_rejection(factory: bool) {
    let ctx = TypedContext::from_services(Arc::new(Foreign));
    let d = Arc::new(AtomicUsize::new(0));
    let c = Arc::new(AtomicUsize::new(0));
    let (release, gate) = std::sync::mpsc::channel();
    let policy = SupervisorStrategyType::OneForOne(policy(&d, true));
    let mut future = if factory {
        ctx.spawn_typed_child_factory_with_strategy::<Probe, Query, _>(
            gated_factory(&d, &c, Some(gate)),
            policy,
        )
    } else {
        ctx.spawn_typed_child_with_strategy::<Probe, Query>(gated_probe(&d, &c, Some(gate)), policy)
    };
    assert!(futures::poll!(&mut future).is_pending());
    cancel(future);
    release.send(()).unwrap();
    retired(&d, 2).await;
    assert_eq!(c.load(Ordering::SeqCst), 0);
}
#[test]
fn cancelled_foreign_instance_rejection_keeps_cleanup() {
    isolated("cancelled_foreign_instance_rejection_keeps_cleanup", || {
        runtime(cancel_rejection(false))
    });
}
#[test]
fn cancelled_foreign_factory_rejection_keeps_cleanup() {
    isolated("cancelled_foreign_factory_rejection_keeps_cleanup", || {
        runtime(cancel_rejection(true))
    });
}
