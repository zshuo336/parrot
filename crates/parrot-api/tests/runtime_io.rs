use parrot_api::{
    errors::ActorError,
    runtime::{
        ActorRuntime, LoadBalancingStrategy, RuntimeConfig, SchedulerConfig, TokioActorRuntime,
    },
};
use std::{sync::Arc, thread, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{oneshot, Notify},
};

const WAIT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn actor_can_await_independent_io_with_capacity_one_and_real_tcp() {
    for strategy in [
        LoadBalancingStrategy::RoundRobin,
        LoadBalancingStrategy::Random,
        LoadBalancingStrategy::LeastLoaded,
    ] {
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            worker_threads: Some(1),
            io_threads: Some(2),
            scheduler_config: SchedulerConfig {
                task_queue_capacity: 1,
                load_balancing: strategy,
                ..Default::default()
            },
        })
        .await
        .unwrap();
        let io = runtime.clone();
        let actor = runtime.spawn(async move {
            let actor_worker = thread::current().id();
            assert_eq!(tokio::runtime::Handle::current().metrics().num_workers(), 1);
            let io_worker = io
                .spawn_io(async move {
                    assert_eq!(tokio::runtime::Handle::current().metrics().num_workers(), 2);
                    // Create readiness resources inside the I/O runtime, not on the host.
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let address = listener.local_addr().unwrap();
                    let server = async {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let mut bytes = [0; 4];
                        stream.read_exact(&mut bytes).await.unwrap();
                        assert_eq!(&bytes, b"ping");
                        stream.write_all(b"pong").await.unwrap();
                    };
                    let client = async {
                        let mut stream = TcpStream::connect(address).await.unwrap();
                        stream.write_all(b"ping").await.unwrap();
                        let mut bytes = [0; 4];
                        stream.read_exact(&mut bytes).await.unwrap();
                        assert_eq!(&bytes, b"pong");
                    };
                    tokio::join!(server, client);
                    thread::current().id()
                })
                .await
                .unwrap();
            assert_ne!(actor_worker, io_worker);
            (actor_worker, io_worker)
        });
        let (actor, io) = tokio::time::timeout(WAIT, actor)
            .await
            .expect("actor waiting for I/O deadlocked on its own execution permit")
            .unwrap();
        assert_ne!(actor, thread::current().id());
        assert_ne!(io, thread::current().id());
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn io_only_configuration_preserves_owned_actor_executor() {
    let host = thread::current().id();
    let runtime = TokioActorRuntime::start(RuntimeConfig {
        io_threads: Some(1),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_ne!(
        runtime
            .spawn(async { thread::current().id() })
            .await
            .unwrap(),
        host
    );
    assert_ne!(
        runtime
            .spawn_io(async { thread::current().id() })
            .await
            .unwrap(),
        host
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn unconfigured_io_uses_normal_executor_and_lifecycle() {
    let host = thread::current().id();
    let runtime = TokioActorRuntime::start(RuntimeConfig::default())
        .await
        .unwrap();
    assert_eq!(
        runtime
            .spawn_io(async { thread::current().id() })
            .await
            .unwrap(),
        host
    );
    runtime.clone().shutdown().await.unwrap();
    assert!(matches!(
        runtime
            .spawn_io(async { panic!("rejected input was polled") })
            .await,
        Err(ActorError::Stopped)
    ));
}

#[tokio::test]
async fn io_capacity_and_cancelled_shutdown_preserve_pending_work_contract() {
    let runtime = TokioActorRuntime::start(RuntimeConfig {
        io_threads: Some(1),
        scheduler_config: SchedulerConfig {
            task_queue_capacity: 1,
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    let (entered, observed) = oneshot::channel();
    let release = Arc::new(Notify::new());
    let held = release.clone();
    let mut active = Box::pin(runtime.spawn_io(async move {
        entered.send(()).unwrap();
        held.notified().await;
    }));
    assert!(futures::poll!(&mut active).is_pending());
    tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
    let mut queued = Box::pin(runtime.spawn_io(async {
        panic!("queued I/O was executed after shutdown");
    }));
    assert!(futures::poll!(&mut queued).is_pending());
    assert_eq!(runtime.metrics().active_actors, 1);
    assert_eq!(runtime.metrics().pending_messages, 1);
    let mut shutdown = Box::pin(runtime.clone().shutdown());
    assert!(futures::poll!(&mut shutdown).is_pending());
    drop(shutdown);
    assert!(matches!(
        runtime.spawn_io(async { 1 }).await,
        Err(ActorError::Stopped)
    ));
    release.notify_one();
    tokio::time::timeout(WAIT, active).await.unwrap().unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, queued).await.unwrap(),
        Err(ActorError::Stopped)
    ));
    tokio::time::timeout(WAIT, runtime.clone().shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runtime.metrics().active_actors, 0);
    assert_eq!(runtime.metrics().pending_messages, 0);
}
