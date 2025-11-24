//! Explicit native I/O workers must be constructed on every startup entry.
use parrot::{
    actix::ActixSystemSettings,
    system::ParrotActorSystem,
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    engine::{AsyncEngineRuntime, AsyncIoRuntimeExt},
    runtime::RuntimeConfig,
    system::{ActorSystem, ActorSystemConfig},
};
use std::{
    cell::RefCell,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::ThreadId,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LIMIT: Duration = Duration::from_secs(10);
#[path = "runtime_native_io/contexts.rs"]
mod contexts;
#[path = "runtime_native_io/failures.rs"]
mod failures;
#[path = "runtime_native_io/output.rs"]
mod output;
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(LIMIT, future)
        .await
        .expect("I/O contract timed out")
}

struct ThreadExit(Arc<AtomicUsize>);
impl Drop for ThreadExit {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
thread_local! {
    static EXIT: RefCell<Option<ThreadExit>> = const { RefCell::new(None) };
}

async fn tcp() -> parrot_api::types::ActorResult<ThreadId> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut input = [0u8; 4];
        stream.read_exact(&mut input).await.unwrap();
        assert_eq!(&input, b"ping");
        stream.write_all(b"pong").await.unwrap();
    };
    let client = async {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut output = [0u8; 4];
        stream.read_exact(&mut output).await.unwrap();
        assert_eq!(&output, b"pong");
    };
    tokio::join!(server, client);
    Ok(std::thread::current().id())
}

async fn execution_contract(engine: &dyn AsyncEngineRuntime) -> Arc<AtomicUsize> {
    assert!(
        engine.status().resources.thread_count >= 2,
        "status must account for the I/O worker owner"
    );
    let host = std::thread::current().id();
    let io = engine
        .io_runtime()
        .expect("configured engine must expose I/O");
    assert_ne!(bounded(io.run_io_typed(tcp())).await.unwrap(), host);
    let exits = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    let mut entered = Vec::new();
    let mut releases = Vec::new();
    // Dropping releases also unblocks workers if an assertion unwinds the test.
    for _ in 0..2 {
        let (release, wait) = std::sync::mpsc::channel();
        releases.push(release);
        let (send, receive) = tokio::sync::oneshot::channel();
        entered.push(receive);
        let exits = exits.clone();
        tasks.push(tokio::spawn(io.run_io_typed(async move {
            EXIT.with(|slot| {
                *slot.borrow_mut() = Some(ThreadExit(exits));
            });
            let id = std::thread::current().id();
            let _ = send.send(id);
            wait.recv_timeout(LIMIT).expect("worker gate not released");
            Ok(id)
        })));
    }
    let first = bounded(entered.remove(0)).await.unwrap();
    let second = bounded(entered.remove(0)).await.unwrap();
    assert_ne!(first, second, "io_threads=2 requires two actual OS workers");
    assert_ne!(first, host);
    assert_ne!(second, host);
    for release in releases {
        release.send(()).unwrap();
    }
    for task in tasks {
        bounded(task).await.unwrap().unwrap();
    }
    assert_eq!(exits.load(Ordering::SeqCst), 0);
    exits
}

fn config() -> ActorSystemConfig {
    ActorSystemConfig {
        runtime_config: RuntimeConfig {
            worker_threads: Some(1),
            io_threads: Some(2),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn thread_entry(entry: usize) {
    if entry == 0 {
        let system = <ThreadActorSystem as ActorSystem>::start(config())
            .await
            .expect("native Thread startup must support an independent I/O runtime");
        let exits = execution_contract(&system).await;
        bounded(system.shutdown()).await.unwrap();
        assert_eq!(
            exits.load(Ordering::SeqCst),
            2,
            "shutdown must join I/O thread TLS disposal"
        );
    } else {
        let system = match entry {
            1 => ParrotActorSystem::start(config()),
            2 => ParrotActorSystem::with_thread(
                config(),
                ThreadActorSystemConfig {
                    shared_burst_workers_max: 0,
                    ..Default::default()
                },
            ),
            3 => <ParrotActorSystem as ActorSystem>::start(config()),
            _ => unreachable!(),
        }
        .await
        .expect("facade Thread startup must support an independent I/O runtime");
        let exits = execution_contract(&system).await;
        bounded(system.shutdown_runtime()).await.unwrap();
        assert_eq!(
            exits.load(Ordering::SeqCst),
            2,
            "facade must join I/O thread TLS disposal"
        );
    }
}

#[tokio::test]
async fn native_thread_accepts_independent_io_workers() {
    thread_entry(0).await;
}

#[tokio::test]
async fn native_thread_runtime_constructor_exposes_actual_io_workers() {
    let system =
        ThreadActorSystem::with_runtime_config(Default::default(), config().runtime_config)
            .await
            .unwrap();
    let exits = execution_contract(system.as_ref()).await;
    bounded(system.shutdown_internal()).await.unwrap();
    assert_eq!(exits.load(Ordering::SeqCst), 2);
}

#[actix::test]
async fn native_actix_runtime_constructor_exposes_actual_io_workers() {
    let system = parrot::actix::ActixActorSystem::with_runtime_config(
        Default::default(),
        config().runtime_config,
    )
    .await
    .unwrap();
    let exits = execution_contract(&system).await;
    bounded(system.shutdown()).await.unwrap();
    assert_eq!(exits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn default_facade_accepts_independent_io_workers() {
    thread_entry(1).await;
}

#[tokio::test]
async fn thread_facade_accepts_independent_io_workers() {
    thread_entry(2).await;
}

#[tokio::test]
async fn trait_facade_accepts_independent_io_workers() {
    thread_entry(3).await;
}

#[actix::test]
async fn actix_facade_accepts_independent_io_workers() {
    let system = ParrotActorSystem::with_actix(config(), ActixSystemSettings::default())
        .await
        .expect("Actix startup must support an independent I/O runtime");
    let exits = execution_contract(&system).await;
    bounded(system.shutdown_runtime()).await.unwrap();
    assert_eq!(exits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn io_task_cannot_shutdown_its_own_facade_or_close_admission() {
    let system = Arc::new(ParrotActorSystem::start(config()).await.unwrap());
    let native = system.runtime("default").unwrap();
    assert!(
        bounded(
            system
                .io_runtime()
                .unwrap()
                .run_io_typed(async move { Ok(native.shutdown().await.is_err()) })
        )
        .await
        .unwrap()
    );
    let captured = system.clone();
    let rejected = bounded(
        system
            .io_runtime()
            .unwrap()
            .run_io_typed(async move { Ok(captured.shutdown_runtime().await.is_err()) }),
    )
    .await
    .unwrap();
    assert!(rejected);
    bounded(system.io_runtime().unwrap().run_io_typed(tcp()))
        .await
        .unwrap();
    let named = bounded(system.run_io_on(
        "default",
        Box::pin(async {
            tcp()
                .await
                .map(|id| Box::new(id) as parrot_api::types::BoxedMessage)
        }),
    ))
    .await
    .unwrap();
    assert_ne!(
        *named.downcast::<ThreadId>().unwrap(),
        std::thread::current().id()
    );
    bounded(system.shutdown_runtime()).await.unwrap();
}

struct PendingInput {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    dropped: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Future for PendingInput {
    type Output = parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if let Some(send) = self.entered.take() {
            let _ = send.send(());
        }
        std::task::Poll::Pending
    }
}
impl Drop for PendingInput {
    fn drop(&mut self) {
        if let Some(send) = self.dropped.take() {
            let _ = send.send(());
        }
    }
}
fn pending() -> (
    PendingInput,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (entered, receive) = tokio::sync::oneshot::channel();
    let (dropped, retired) = tokio::sync::oneshot::channel();
    (
        PendingInput {
            entered: Some(entered),
            dropped: Some(dropped),
        },
        receive,
        retired,
    )
}

#[tokio::test]
async fn caller_cancellation_and_system_shutdown_dispose_actual_io_inputs() {
    let system = ParrotActorSystem::start(config()).await.unwrap();
    let (input, entered, retired) = pending();
    let task = tokio::spawn(system.io_runtime().unwrap().run_io(Box::pin(input)));
    bounded(entered).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    bounded(retired).await.unwrap();
    let (input, entered, retired) = pending();
    let task = tokio::spawn(system.io_runtime().unwrap().run_io(Box::pin(input)));
    bounded(entered).await.unwrap();
    bounded(system.shutdown_runtime()).await.unwrap();
    bounded(retired).await.unwrap();
    assert!(matches!(
        bounded(task).await.unwrap(),
        Err(parrot_api::errors::ActorError::Stopped)
    ));
}

#[tokio::test]
async fn unpolled_io_input_is_retired_without_polling_it() {
    let system = ParrotActorSystem::start(config()).await.unwrap();
    let (input, entered, retired) = pending();
    drop(system.io_runtime().unwrap().run_io(Box::pin(input)));
    bounded(retired).await.unwrap();
    assert!(entered.await.is_err());
    bounded(system.shutdown_runtime()).await.unwrap();
}
