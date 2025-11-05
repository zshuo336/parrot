//! Host-runtime cancellation must not retain queued payloads in surviving refs.
use parrot::thread::ThreadActorSystem;
use parrot_api::{
    message::Message,
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::{ActorResult, BoxedFuture},
};
use std::{
    future::Future,
    pin::Pin,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

struct Tracked(Arc<AtomicUsize>, bool);
impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "cancelled destructor panic");
    }
}
enum Op {
    Hold,
    Queued(Tracked),
}
impl Message for Op {
    type Result = ();
}
struct Probe {
    _actor: Tracked,
    init: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    future: Arc<AtomicUsize>,
    panic_future: bool,
}
struct Pending {
    entered: Arc<tokio::sync::Notify>,
    _dropped: Tracked,
}
impl Future for Pending {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.entered.notify_one();
        Poll::Pending
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Op;
    type Reply = ();
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.init.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, message: Op) -> BoxedFuture<'_, ActorResult<()>> {
        match message {
            Op::Hold => Box::pin(Pending {
                entered: self.entered.clone(),
                _dropped: Tracked(self.future.clone(), self.panic_future),
            }),
            Op::Queued(payload) => Box::pin(async move {
                drop(payload);
                Ok(())
            }),
        }
    }
}
impl ParrotMsgVariant<Probe> for Op {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(_: ()) -> ActorResult<()> {
        Ok(())
    }
}

fn isolated(name: &str, live: bool, panics: bool) {
    const CHILD: &str = "PARROT_TYPED_HOST_CANCEL_CASE";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(4)
            .build()
            .unwrap();
        let actor_drops = Arc::new(AtomicUsize::new(0));
        let message_drops = Arc::new(AtomicUsize::new(0));
        let init = Arc::new(AtomicUsize::new(0));
        let future_drops = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let (system, reference) = runtime.block_on(async {
            let system = ThreadActorSystem::shared(Default::default());
            let actor = system
                .spawn_typed::<_, Op>(
                    Probe {
                        _actor: Tracked(actor_drops.clone(), panics),
                        init: init.clone(),
                        entered: entered.clone(),
                        future: future_drops.clone(),
                        panic_future: panics,
                    },
                    "/cancel",
                )
                .await
                .unwrap();
            if live {
                actor.tell(Op::Hold).await.unwrap();
                tokio::time::timeout(Duration::from_secs(2), entered.notified())
                    .await
                    .unwrap();
            }
            actor
                .tell(Op::Queued(Tracked(message_drops.clone(), panics)))
                .await
                .unwrap();
            actor
                .tell(Op::Queued(Tracked(message_drops.clone(), false)))
                .await
                .unwrap();
            (system, actor)
        });
        assert_eq!(init.load(Ordering::SeqCst), usize::from(live));
        println!("HOST_CANCEL_READY {name}");
        runtime.shutdown_timeout(Duration::from_secs(3));
        let deadline = Instant::now() + Duration::from_secs(2);
        while actor_drops.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(actor_drops.load(Ordering::SeqCst), 1, "actor must retire");
        assert_eq!(
            future_drops.load(Ordering::SeqCst),
            usize::from(live),
            "dispatch future must retire"
        );
        assert_eq!(
            message_drops.load(Ordering::SeqCst),
            2,
            "retained ref must not retain cancelled mailbox payloads"
        );
        assert!(!reference.is_alive());
        drop(reference);
        drop(system);
        println!("HOST_CANCEL_RETIRED {name}");
        return;
    }
    struct Logs(std::path::PathBuf);
    impl Drop for Logs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let logs =
        Logs(std::env::temp_dir().join(format!("parrot-typed-cancel-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&logs.0).unwrap();
    let stdout_file = std::fs::File::create(logs.0.join("stdout")).unwrap();
    let stderr_file = std::fs::File::create(logs.0.join("stderr")).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (status, false);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break (child.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // File output cannot deadlock a crashing child on an undrained pipe. Bound
    // diagnostic allocation even if a regression emits an unexpectedly large log.
    let read = |name| {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(logs.0.join(name))
            .unwrap()
            .take(256 * 1024)
            .read_to_end(&mut bytes)
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let stdout = read("stdout");
    let stderr = read("stderr");
    assert!(
        !timed_out,
        "owned child did not terminate: {stdout}\n{stderr}"
    );
    assert!(
        stdout.contains(&format!("HOST_CANCEL_READY {name}")),
        "child never reached cancellation: {stdout}\n{stderr}"
    );
    assert!(
        status.success(),
        "child failed {status}: {stdout}\n{stderr}"
    );
    assert!(stdout.contains(&format!("HOST_CANCEL_RETIRED {name}")));
}
#[test]
fn unpolled_runtime_cancellation_drains_retained_mailbox() {
    isolated(
        "unpolled_runtime_cancellation_drains_retained_mailbox",
        false,
        false,
    );
}
#[test]
fn live_runtime_cancellation_drains_retained_mailbox() {
    isolated(
        "live_runtime_cancellation_drains_retained_mailbox",
        true,
        false,
    );
}
#[test]
fn live_runtime_cancellation_isolates_future_actor_and_mailbox_panics() {
    isolated(
        "live_runtime_cancellation_isolates_future_actor_and_mailbox_panics",
        true,
        true,
    );
}

#[test]
fn unpolled_runtime_cancellation_isolates_mailbox_panics() {
    isolated(
        "unpolled_runtime_cancellation_isolates_mailbox_panics",
        false,
        true,
    );
}
