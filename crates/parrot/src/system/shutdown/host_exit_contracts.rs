use crate::system::ParrotActorSystem;
use parrot_api::{
    engine::{AsyncEngineRuntime, InstanceConfig, RuntimeActor, SpawnOptions},
    system::{SystemError, SystemResources, SystemState, SystemStatus},
    types::{BoxedActorRef, BoxedFuture},
};
use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Default)]
struct Counts {
    shutdowns: AtomicUsize,
    polls: AtomicUsize,
    futures: AtomicUsize,
    engines: AtomicUsize,
    payloads: AtomicUsize,
}
struct Payload(Arc<Counts>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("host-exit panic payload disposal");
    }
}
struct Engine {
    counts: Arc<Counts>,
    throws: bool,
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.counts.engines.fetch_add(1, Ordering::SeqCst);
        if self.throws {
            std::panic::panic_any(Payload(self.counts.clone()));
        }
    }
}
struct Shutdown {
    counts: Arc<Counts>,
    throws: bool,
}
impl Future for Shutdown {
    type Output = Result<(), SystemError>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.counts.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Pending
    }
}
impl Drop for Shutdown {
    fn drop(&mut self) {
        self.counts.futures.fetch_add(1, Ordering::SeqCst);
        if self.throws {
            std::panic::panic_any(Payload(self.counts.clone()));
        }
    }
}
impl AsyncEngineRuntime for Engine {
    fn spawn<'a>(
        &'a self,
        _: RuntimeActor,
        _: InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        panic!("host-exit fixture must not spawn actors");
    }
    fn get_actor<'a>(&'a self, _: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async { None })
    }
    fn status(&self) -> SystemStatus {
        SystemStatus {
            state: SystemState::Running,
            active_actors: 0,
            uptime: Duration::ZERO,
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 0,
            },
        }
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        self.counts.shutdowns.fetch_add(1, Ordering::SeqCst);
        Box::pin(Shutdown {
            counts: self.counts.clone(),
            throws: self.throws,
        })
    }
}

fn host_exit(started: bool, throwing: usize) {
    let counts: Vec<_> = (0..3).map(|_| Arc::new(Counts::default())).collect();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut facade = ParrotActorSystem::new(Default::default()).await.unwrap();
        // Exercise forced cancellation of the actual owner, independently of
        // the production facade's host-independent executor selection.
        facade.runtime = runtime.handle().clone();
        for (index, counts) in counts.iter().enumerate() {
            let engine = Arc::new(Engine {
                counts: counts.clone(),
                throws: index < throwing,
            });
            facade
                .register_runtime(format!("engine-{index}"), engine.clone(), index == 0)
                .unwrap();
            facade
                .register_runtime(format!("alias-{index}"), engine, false)
                .unwrap();
        }
        let mut waiter = facade.shutdown_runtime();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        // Poll only the public waiter. On this current-thread executor the
        // spawned retirement task cannot execute until this block yields.
        assert!(waiter.as_mut().poll(&mut cx).is_pending());
        drop(waiter);
        assert!(matches!(
            facade.unregister_system("engine-0"),
            Err(SystemError::ShuttingDown)
        ));
        for counts in &counts {
            assert_eq!(counts.shutdowns.load(Ordering::SeqCst), 0);
            assert_eq!(counts.engines.load(Ordering::SeqCst), 0);
        }
        if started {
            tokio::time::timeout(Duration::from_secs(5), async {
                while counts
                    .iter()
                    .any(|counts| counts.polls.load(Ordering::SeqCst) == 0)
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("every engine shutdown must have entered its pending future");
        }
        // The owned task, rather than the registration map or a caller Arc,
        // must be the final owner when the host cancels its tasks below.
        drop(facade);
        for counts in &counts {
            assert_eq!(counts.engines.load(Ordering::SeqCst), 0);
        }
    });
    let result = catch_unwind(AssertUnwindSafe(|| drop(runtime)));
    if let Err(payload) = result {
        if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
            std::mem::forget(secondary);
        }
        panic!("facade cleanup unwound through host shutdown");
    }
    for (index, counts) in counts.iter().enumerate() {
        assert_eq!(counts.engines.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.shutdowns.load(Ordering::SeqCst),
            usize::from(started)
        );
        assert_eq!(counts.futures.load(Ordering::SeqCst), usize::from(started));
        assert_eq!(
            counts.payloads.load(Ordering::SeqCst),
            usize::from(index < throwing) * (1 + usize::from(started)),
            "engine and future panic payloads must each be retired once"
        );
    }
}

async fn isolated(name: &str, started: bool, throwing: usize) {
    const KEY: &str = "PARROT_FACADE_HOST_EXIT_WORKER";
    if std::env::var_os(KEY).is_some() {
        std::thread::spawn(move || host_exit(started, throwing))
            .join()
            .expect("host shutdown worker failed");
        return;
    }
    let output = tokio::time::timeout(
        Duration::from_secs(25),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("system::shutdown::host_exit_contracts::{name}"),
                "--nocapture",
            ])
            .env(KEY, "1")
            .env("RUST_BACKTRACE", "0")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("host-exit worker stalled")
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success()
            && stdout
                .lines()
                .filter(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"))
                .count()
                == 1,
        "host-exit worker must execute exactly one passing contract: status={}\n{}\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}
macro_rules! case {
    ($name:ident, $started:expr, $throwing:expr) => {
        #[tokio::test]
        async fn $name() {
            isolated(stringify!($name), $started, $throwing).await;
        }
    };
}
case!(unpolled_snapshot_retires_all_healthy_engines, false, 0);
case!(unpolled_snapshot_isolates_one_engine_destructor, false, 1);
case!(
    unpolled_snapshot_isolates_multiple_engine_destructors,
    false,
    2
);
case!(pending_snapshot_retires_all_healthy_engines, true, 0);
case!(
    pending_snapshot_isolates_engine_and_future_destructors,
    true,
    1
);
case!(
    pending_snapshot_isolates_multiple_engine_and_future_destructors,
    true,
    2
);
