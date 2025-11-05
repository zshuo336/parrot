//! Native factory calls own their inputs before the returned future is polled.
use futures::FutureExt;
use parrot::{
    actix::{ActixActor, ActixActorSystem, ActixContext},
    thread::config::SupervisorStrategy,
    thread::{ThreadActorConfig, ThreadActorSystem, ThreadContext},
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
#[derive(Default)]
struct Counts {
    inputs: AtomicUsize,
    built: AtomicUsize,
    init: AtomicUsize,
}
struct BadPayload;
impl Drop for BadPayload {
    fn drop(&mut self) {
        panic!("factory payload disposal");
    }
}
struct Input {
    counts: Arc<Counts>,
    gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    recursive: bool,
}
impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Input")
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.get_mut().unwrap() {
            gate.recv_timeout(Duration::from_secs(5))
                .expect("caller blocked by factory input destructor");
        }
        self.counts.inputs.fetch_add(1, Ordering::SeqCst);
        if self.recursive {
            std::panic::panic_any(BadPayload);
        } else {
            panic!("native factory policy disposal");
        }
    }
}
impl parrot::thread::mailbox::admission::MessageAdmission for Input {
    fn prepare(
        &self,
        message: BoxedMessage,
    ) -> Result<BoxedMessage, parrot::thread::error::MailboxError> {
        Ok(message)
    }
}
struct ThreadProbe(Arc<Counts>);
struct ActixProbe(Arc<Counts>);
macro_rules! actor {
    ($a:ident,$ctx:ty) => {
        impl Actor for $a {
            type Config = EmptyConfig;
            type Context = $ctx;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn init<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                self.0.init.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { Ok(message) })
            }
        }
    };
}
actor!(ThreadProbe, ThreadContext<Self>);
actor!(ActixProbe, ActixContext<ActixActor<Self>>);
impl ActixEngineExt for ActixProbe {}
fn input(c: &Arc<Counts>, gate: Option<std::sync::mpsc::Receiver<()>>, recursive: bool) -> Input {
    Input {
        counts: c.clone(),
        gate: Mutex::new(gate),
        recursive,
    }
}
fn policy(c: &Arc<Counts>) -> OneForOneStrategy {
    let capture = input(c, None, false);
    OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    }
}
fn thread_config(c: &Arc<Counts>) -> ThreadActorConfig {
    ThreadActorConfig {
        message_admission: Some(Arc::new(input(c, None, false))),
        ..Default::default()
    }
}
fn thread_factory(
    c: &Arc<Counts>,
    gate: Option<std::sync::mpsc::Receiver<()>>,
) -> impl Fn() -> ThreadProbe + Send + Sync + 'static {
    let capture = input(c, gate, true);
    let c = c.clone();
    move || {
        let _ = &capture;
        c.built.fetch_add(1, Ordering::SeqCst);
        ThreadProbe(c.clone())
    }
}
fn actix_factory(
    c: &Arc<Counts>,
    gate: Option<std::sync::mpsc::Receiver<()>>,
) -> impl Fn() -> ActixProbe + Send + Sync + 'static {
    let capture = input(c, gate, true);
    let c = c.clone();
    move || {
        let _ = &capture;
        c.built.fetch_add(1, Ordering::SeqCst);
        ActixProbe(c.clone())
    }
}
fn cancel<F: Future + Send>(future: F, count: usize, c: &Counts) {
    assert_eq!(count, 0);
    assert_eq!(c.built.load(Ordering::SeqCst), 0);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok(),
        "unpolled native factory input escaped its caller"
    );
}
async fn retired(c: &Counts, n: usize) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while c.inputs.load(Ordering::SeqCst) < n {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(c.inputs.load(Ordering::SeqCst), n);
    assert_eq!(c.built.load(Ordering::SeqCst), 0);
    assert_eq!(c.init.load(Ordering::SeqCst), 0);
}
fn isolated(name: &str, body: impl FnOnce()) {
    const FLAG: &str = "PARROT_NATIVE_FACTORY_CASE";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(FLAG, name)
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: {status}");
            return;
        }
        if Instant::now() >= end {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name}: deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
async fn unpolled(actix: bool, explicit: bool) {
    let c = Arc::new(Counts::default());
    if actix {
        let s = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        for blocked in [false, true] {
            let (release, gate) = std::sync::mpsc::channel();
            let gate = blocked.then_some(gate);
            if explicit {
                cancel(
                    s.spawn_root_factory_with_strategy(actix_factory(&c, gate), policy(&c)),
                    s.status().active_actors,
                    &c,
                );
            } else {
                cancel(
                    s.spawn_root_factory(actix_factory(&c, gate)),
                    s.status().active_actors,
                    &c,
                );
            }
            if blocked {
                release.send(()).unwrap();
            }
            retired(&c, (1 + usize::from(blocked)) * (1 + usize::from(explicit))).await;
        }
        s.shutdown().await.unwrap();
    } else {
        let s = ThreadActorSystem::shared(Default::default());
        for blocked in [false, true] {
            let (release, gate) = std::sync::mpsc::channel();
            let gate = blocked.then_some(gate);
            if explicit {
                cancel(
                    s.spawn_supervised_with_config(
                        thread_factory(&c, gate),
                        "/unpolled",
                        thread_config(&c),
                    ),
                    s.actor_count(),
                    &c,
                );
            } else {
                cancel(
                    s.spawn_supervised(
                        thread_factory(&c, gate),
                        "/unpolled",
                        SupervisorStrategy::Stop,
                    ),
                    s.actor_count(),
                    &c,
                );
            }
            if blocked {
                release.send(()).unwrap();
            }
            retired(&c, (1 + usize::from(blocked)) * (1 + usize::from(explicit))).await;
        }
        s.shutdown_internal().await.unwrap();
    }
}
macro_rules! test_case {
    ($name:ident,$actix:expr,$explicit:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                actix::System::new().block_on(unpolled($actix, $explicit))
            });
        }
    };
}
test_case!(thread_factory_unpolled, false, false);
test_case!(thread_configured_factory_unpolled, false, true);
test_case!(actix_factory_unpolled, true, false);
test_case!(actix_configured_factory_unpolled, true, true);
async fn rejected(actix: bool) {
    let c = Arc::new(Counts::default());
    let error = if actix {
        let s = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        s.clone().shutdown().await.unwrap();
        match std::panic::AssertUnwindSafe(
            s.spawn_root_factory_with_strategy(actix_factory(&c, None), policy(&c)),
        )
        .catch_unwind()
        .await
        {
            Ok(Err(error)) => error.to_string(),
            _ => panic!("rejected factory must return a cleanup error"),
        }
    } else {
        let s = ThreadActorSystem::shared(Default::default());
        s.shutdown_internal().await.unwrap();
        match std::panic::AssertUnwindSafe(s.spawn_supervised_with_config(
            thread_factory(&c, None),
            "/closed",
            thread_config(&c),
        ))
        .catch_unwind()
        .await
        {
            Ok(Err(error)) => error.to_string(),
            _ => panic!("rejected factory must return a cleanup error"),
        }
    };
    assert!(
        error.contains("panic payload destructor panicked"),
        "{error}"
    );
    retired(&c, 2).await;
}
#[test]
fn thread_factory_rejection_keeps_both_inputs_owned() {
    isolated("thread_factory_rejection_keeps_both_inputs_owned", || {
        actix::System::new().block_on(rejected(false))
    });
}
#[test]
fn actix_factory_rejection_keeps_both_inputs_owned() {
    isolated("actix_factory_rejection_keeps_both_inputs_owned", || {
        actix::System::new().block_on(rejected(true))
    });
}
