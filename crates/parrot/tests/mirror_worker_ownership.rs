use parrot::{
    mirror::{MirrorCopier, MirrorOptions, MirrorPolicy},
    system::ParrotActorSystem,
};
use parrot_api::{
    address::{ActorPath, ActorRef},
    system::ActorSystem,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Default, Debug)]
struct State {
    events: Mutex<Vec<&'static str>>,
    admissions: AtomicUsize,
    targets: AtomicUsize,
    messages: AtomicUsize,
}
impl State {
    fn event(&self, s: &'static str) {
        self.events.lock().unwrap().push(s);
    }
}
#[derive(Debug)]
struct Target {
    state: Arc<State>,
    mode: u8,
    owned: bool,
}
impl Drop for Target {
    fn drop(&mut self) {
        if self.owned {
            self.state.targets.fetch_add(1, Ordering::SeqCst);
            self.state.event("target-drop");
            if self.mode <= 4 {
                panic!("target destructor failed");
            }
        }
    }
}
struct Admission<'a> {
    target: &'a Target,
    message: Option<BoxedMessage>,
}
impl Future for Admission<'_> {
    type Output = ActorResult<()>;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.target.state.event("poll");
        match self.target.mode {
            0 => panic!("admission poll failed"),
            1 | 2 | 6 => Poll::Pending,
            5 => {
                self.message.take();
                Poll::Ready(Err(parrot_api::errors::ActorError::Other(
                    anyhow::Error::new(HostileError(self.target.state.clone())),
                )))
            }
            _ => {
                self.message.take();
                self.target.state.admissions.fetch_add(1, Ordering::SeqCst);
                Poll::Ready(Ok(()))
            }
        }
    }
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.target.state.event("future-drop");
        if self.target.mode <= 2 {
            panic!("admission destructor failed");
        }
    }
}
#[derive(Debug)]
struct HostileError(Arc<State>);
impl std::fmt::Display for HostileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("hostile admission error")
    }
}
impl std::error::Error for HostileError {}
impl Drop for HostileError {
    fn drop(&mut self) {
        self.0.event("error-drop");
        panic!("admission error destructor failed");
    }
}
#[async_trait::async_trait]
impl ActorRef for Target {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { panic!("mirror must use TELL") })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.state.event("construct");
        if self.mode == 3 {
            panic!("admission construction failed");
        }
        Box::pin(Admission {
            target: self,
            message: Some(message),
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/shadow/target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            state: self.state.clone(),
            mode: self.mode,
            owned: true,
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct Message {
    state: Arc<State>,
    shadow: bool,
}
impl Drop for Message {
    fn drop(&mut self) {
        if self.shadow {
            self.state.messages.fetch_add(1, Ordering::SeqCst);
            panic!("queued shadow destructor failed");
        }
    }
}
struct Primary(Arc<AtomicUsize>);
impl parrot_api::actor::Actor for Primary {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = parrot::thread::context::ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(42u64) as BoxedMessage)
        })
    }
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
}
async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn exercise(mode: u8) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let state = Arc::new(State::default());
    let primary = Arc::new(AtomicUsize::new(0));
    let engine = parrot::thread::system::ThreadActorSystem::shared(Default::default());
    engine
        .spawn_at(
            Primary(primary.clone()),
            "/source/main",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    facade
        .register_thread_system("primary".into(), engine, true)
        .await
        .unwrap();
    facade
        .register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
        .unwrap();
    facade
        .register_mirror_copier(MirrorCopier::new(
            |_: &Message| 8,
            |m: &Message| {
                Ok(Message {
                    state: m.state.clone(),
                    shadow: true,
                })
            },
        ))
        .unwrap();
    facade
        .register_prefix_handler(
            "/shadow/",
            Arc::new(Target {
                state: state.clone(),
                mode,
                owned: false,
            }),
        )
        .unwrap();
    let handle = facade
        .register_mirror_policy_with_options(
            MirrorPolicy::new("/source/", "/shadow/target"),
            MirrorOptions {
                capacity: 3,
                delivery_timeout: Duration::from_millis(if mode == 1 { 30 } else { 2000 }),
                ..Default::default()
            },
        )
        .unwrap();
    let source = facade
        .get_actor(&ActorPath::placeholder("/source/main"))
        .await
        .unwrap();
    assert_eq!(
        *source
            .send(Box::new(1u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    if mode == 2 || mode == 6 {
        wait_for(|| state.events.lock().unwrap().contains(&"poll")).await;
        if mode == 6 {
            for _ in 0..2 {
                assert_eq!(
                    *source
                        .send(Box::new(Message {
                            state: state.clone(),
                            shadow: false
                        }))
                        .await
                        .unwrap()
                        .downcast::<u64>()
                        .unwrap(),
                    42
                );
            }
            assert_eq!(handle.stats().queued, 3);
        }
        handle.shutdown().await;
    } else {
        wait_for(|| {
            handle.is_finished() || handle.stats().delivery_failed + handle.stats().admitted > 0
        })
        .await;
        // A failed admission must leave the worker available for later healthy work.
        facade
            .register_prefix_handler(
                "/shadow/",
                Arc::new(Target {
                    state: state.clone(),
                    mode: 7,
                    owned: false,
                }),
            )
            .unwrap();
        assert_eq!(
            *source
                .send(Box::new(2u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
        wait_for(|| {
            handle.is_finished()
                || state.admissions.load(Ordering::SeqCst) == if mode == 4 { 2 } else { 1 }
        })
        .await;
        handle.shutdown().await;
    }
    let stats = handle.stats();
    facade.internal_shutdown().await.unwrap();
    let events = state.events.lock().unwrap();
    assert!(handle.is_finished());
    assert_eq!(stats.worker_failed, 0);
    assert_eq!(
        stats.disposal_failed,
        if mode <= 2 || mode == 6 { 2 } else { 1 }
    );
    assert_eq!(
        primary.load(Ordering::SeqCst),
        if mode == 6 {
            3
        } else if mode == 2 {
            1
        } else {
            2
        }
    );
    assert_eq!(
        state.targets.load(Ordering::SeqCst),
        if mode == 2 || mode == 6 { 1 } else { 2 },
        "worker did not continue after shadow failure: {events:?}"
    );
    if mode == 6 {
        assert_eq!(state.messages.load(Ordering::SeqCst), 2);
        assert_eq!(stats.cancelled, 3);
    } else if mode == 2 {
        assert_eq!(stats.cancelled, 1);
    } else if mode == 4 {
        assert_eq!(
            stats.admitted, 2,
            "actual successful TELL must remain an admission even if target cleanup fails"
        );
    } else {
        assert_eq!(stats.admitted, 1);
        assert_eq!(stats.delivery_failed, 1);
    }
    let target = events.iter().position(|e| *e == "target-drop").unwrap();
    if mode != 3 {
        assert!(
            events[..target].contains(&"future-drop"),
            "borrowed future must retire before target"
        );
    }
    if mode == 5 {
        assert_eq!(events.iter().filter(|e| **e == "error-drop").count(), 1);
    }
}
fn isolated(name: &str, mode: u8) {
    if std::env::var("PARROT_MIRROR_WORKER_CHILD").as_deref() == Ok(name) {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(exercise(mode));
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_MIRROR_WORKER_CHILD", name)
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: {status}");
            return;
        }
        if start.elapsed() > Duration::from_secs(30) {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("worker failed to retire");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! contract {
    ($name:ident,$mode:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), $mode);
        }
    };
}
contract!(poll_future_and_target_panics_have_independent_owners, 0);
contract!(timeout_retires_borrowed_future_before_target, 1);
contract!(cancel_retires_borrowed_future_before_target, 2);
contract!(construction_panic_does_not_unwind_through_target, 3);
contract!(successful_admission_survives_target_disposal_failure, 4);
contract!(hostile_error_disposal_does_not_kill_worker, 5);
contract!(queued_shadow_disposal_retires_every_message, 6);

type DisposalGate = Arc<
    Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
>;
struct BlockingShadow {
    gate: DisposalGate,
    count: Arc<AtomicUsize>,
    shadow: bool,
}
impl Drop for BlockingShadow {
    fn drop(&mut self) {
        if self.shadow {
            let (entered, release) = self.gate.lock().unwrap().take().unwrap();
            let _ = entered.send(());
            release.recv_timeout(Duration::from_secs(5)).unwrap();
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }
}
async fn held_capacity(bytes: bool) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = parrot::thread::system::ThreadActorSystem::shared(Default::default());
    engine
        .spawn_at(
            Primary(calls.clone()),
            "/source/main",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    facade
        .register_thread_system("primary".into(), engine, true)
        .await
        .unwrap();
    facade
        .register_mirror_copier(MirrorCopier::new(
            |_: &BlockingShadow| 8,
            |v: &BlockingShadow| {
                Ok(BlockingShadow {
                    gate: v.gate.clone(),
                    count: v.count.clone(),
                    shadow: true,
                })
            },
        ))
        .unwrap();
    let handle = facade
        .register_mirror_policy_with_options(
            MirrorPolicy::new("/source/", "/missing"),
            MirrorOptions {
                capacity: if bytes { 2 } else { 1 },
                max_message_bytes: 8,
                max_pending_bytes: 8,
                ..Default::default()
            },
        )
        .unwrap();
    let source = facade
        .get_actor(&ActorPath::placeholder("/source/main"))
        .await
        .unwrap();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let gate = Arc::new(Mutex::new(Some((entered, blocked))));
    let count = Arc::new(AtomicUsize::new(0));
    let value = || {
        Box::new(BlockingShadow {
            gate: gate.clone(),
            count: count.clone(),
            shadow: false,
        }) as BoxedMessage
    };
    source.send(value()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), started)
        .await
        .unwrap()
        .unwrap();
    source.send(value()).await.unwrap();
    let before = handle.stats();
    let mut first = Box::pin(handle.shutdown());
    let mut second = Box::pin(handle.shutdown());
    let first_pending = futures::poll!(&mut first).is_pending();
    let second_pending = futures::poll!(&mut second).is_pending();
    let finished_early = handle.is_finished();
    let retired_early = count.load(Ordering::SeqCst);
    // Release before assertions so a failed check cannot strand the runtime.
    release.send(()).unwrap();
    first.await;
    second.await;
    facade.internal_shutdown().await.unwrap();
    assert!(first_pending && second_pending && !finished_early);
    assert_eq!(retired_early, 0);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(before.queued, 1);
    assert_eq!(before.full, u64::from(!bytes));
    assert_eq!(before.copy_failed, u64::from(bytes));
    assert_eq!(handle.stats().disposal_failed, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_disposal_holds_count_slot_and_all_shutdown_observers() {
    held_capacity(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_disposal_holds_byte_budget_and_all_shutdown_observers() {
    held_capacity(true).await;
}

#[derive(Debug)]
struct ImmediatePrimary;
#[async_trait::async_trait]
impl ActorRef for ImmediatePrimary {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(Box::new(42u64) as BoxedMessage) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/source/main".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[test]
fn unpolled_worker_runtime_shutdown_receipt_waits_for_queued_disposal() {
    // An immediate primary isolates executor retirement: the mirror worker is
    // queued but has never been polled. Other tests above exercise real actors.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered_runtime = runtime.enter();
    let facade = futures::executor::block_on(ParrotActorSystem::new(Default::default())).unwrap();
    facade
        .register_prefix_handler("/source/", Arc::new(ImmediatePrimary))
        .unwrap();
    facade
        .register_mirror_copier(MirrorCopier::new(
            |_: &BlockingShadow| 8,
            |v: &BlockingShadow| {
                Ok(BlockingShadow {
                    gate: v.gate.clone(),
                    count: v.count.clone(),
                    shadow: true,
                })
            },
        ))
        .unwrap();
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/missing"))
        .unwrap();
    let source =
        futures::executor::block_on(facade.get_actor(&ActorPath::placeholder("/source/main")))
            .unwrap();
    let (entered, mut started) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let gate = Arc::new(Mutex::new(Some((entered, blocked))));
    let count = Arc::new(AtomicUsize::new(0));
    let reply = futures::executor::block_on(source.send(Box::new(BlockingShadow {
        gate,
        count: count.clone(),
        shadow: false,
    })))
    .unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 42);
    assert_eq!(handle.stats().queued, 1);
    drop(entered_runtime);
    let closing = std::thread::spawn(move || drop(runtime));
    let start = std::time::Instant::now();
    let mut entered = false;
    while start.elapsed() < Duration::from_secs(3) {
        if started.try_recv().is_ok() {
            entered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let finished_early = handle.is_finished();
    let retired_early = count.load(Ordering::SeqCst);
    let _ = release.send(());
    closing.join().unwrap();
    assert!(entered && !finished_early);
    assert_eq!(retired_early, 0);
    assert!(handle.is_finished());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(handle.stats().cancelled, 1);
}
