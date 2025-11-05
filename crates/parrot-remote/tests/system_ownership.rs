//! The system owns its background work; background work must not own the system.
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Weak,
    },
    time::Duration,
};

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}

async fn released(weak: &Weak<RemoteActorSystem>) {
    let expired = tokio::time::timeout(Duration::from_secs(2), async {
        while weak.strong_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    // Clean up the old implementation even when this regression fails.
    if let Some(leaked) = weak.upgrade() {
        leaked.shutdown().await.unwrap();
    }
    assert!(
        expired,
        "background work retained the last system reference"
    );
}

#[tokio::test]
async fn dropping_tcp_system_releases_listener() {
    let sys = RemoteActorSystem::new(
        RemoteConfig::tcp("drop-tcp", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Empty),
    )
    .unwrap();
    sys.start().await.unwrap();
    let addr = sys.local_addr().unwrap();
    let weak = Arc::downgrade(&sys);
    drop(sys);
    released(&weak).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
                break listener;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("TCP listener remained bound");
}

#[tokio::test]
async fn dropping_quic_system_releases_udp_socket() {
    let sys = RemoteActorSystem::new(
        RemoteConfig::quic("drop-quic", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    sys.start().await.unwrap();
    let addr = sys.local_addr().unwrap();
    let weak = Arc::downgrade(&sys);
    drop(sys);
    released(&weak).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(socket) = tokio::net::UdpSocket::bind(addr).await {
                break socket;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("QUIC socket remained bound");
}

#[tokio::test]
async fn dropping_memory_system_unregisters_endpoint() {
    let sys = RemoteActorSystem::new(RemoteConfig::mem("drop-mem"), Arc::new(Empty)).unwrap();
    sys.start().await.unwrap();
    let weak = Arc::downgrade(&sys);
    drop(sys);
    released(&weak).await;
    let client =
        RemoteActorSystem::new(RemoteConfig::mem("drop-mem-client"), Arc::new(Empty)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        client.connect(&NodeAddr::mem("drop-mem")),
    )
    .await;
    client.shutdown().await.unwrap();
    assert!(result
        .expect("unregistered endpoint must fail promptly")
        .is_err());
}

#[tokio::test]
async fn dropping_system_cancels_active_bootstrap_handshake() {
    use futures::StreamExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = RemoteConfig::tcp("drop-bootstrap", None);
    config
        .seeds
        .push(NodeAddr::tcp("blackhole", listener.local_addr().unwrap()));
    let sys = RemoteActorSystem::new(config, Arc::new(Empty)).unwrap();
    sys.start().await.unwrap();
    let (socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut wire = tokio_util::codec::Framed::new(socket, parrot_remote::transport::FrameCodec);
    let hello = tokio::time::timeout(Duration::from_secs(5), wire.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        hello.header.frame_type,
        parrot_remote::frame::frame_type::HANDSHAKE
    );
    let weak = Arc::downgrade(&sys);
    drop(sys);
    released(&weak).await;
    assert!(tokio::time::timeout(Duration::from_secs(1), wire.next())
        .await
        .expect("cancelled handshake must drop IO")
        .is_none());
}

#[tokio::test]
async fn remote_refs_and_senders_do_not_keep_connected_system_alive() {
    let a = RemoteActorSystem::new(RemoteConfig::mem("drop-pair-a"), Arc::new(Empty)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("drop-pair-b"), Arc::new(Empty)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();
    let remote = a.remote_ref("parrot://drop-pair-b/test").unwrap();
    let senders = a.links_snapshot().await;
    let weak = Arc::downgrade(&a);
    drop(a);
    released(&weak).await;
    tokio::time::timeout(Duration::from_secs(2), senders[0].1.closed())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !b.links_snapshot().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("peer did not observe connection closure");
    drop(remote);
    b.shutdown().await.unwrap();
}

use parrot_remote::admin_v2::{AdminReplyV2, ComponentDeploy, ComponentExecutor};
struct Executor(AtomicUsize, Option<Arc<Blocked>>);
struct Blocked {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}
#[async_trait::async_trait]
impl ComponentExecutor for Executor {
    async fn deploy(&self, _: u64, _: &ComponentDeploy) -> AdminReplyV2 {
        unreachable!()
    }
    async fn drain(&self, _: u64, _: &str, _: u64) -> AdminReplyV2 {
        unreachable!()
    }
    async fn status(&self, _: u64, _: &str) -> AdminReplyV2 {
        unreachable!()
    }
    async fn stop(&self, req_id: u64, _: &str) -> AdminReplyV2 {
        self.0.fetch_add(1, Ordering::SeqCst);
        if let Some(blocked) = &self.1 {
            blocked.entered.notify_one();
            let _permit = blocked.release.acquire().await.unwrap();
        }
        AdminReplyV2::Stopped { req_id }
    }
}
#[tokio::test]
async fn executor_install_replace_remove_after_start_reaches_ingress() {
    let server =
        RemoteActorSystem::new(RemoteConfig::mem("executor-server"), Arc::new(Empty)).unwrap();
    let client =
        RemoteActorSystem::new(RemoteConfig::mem("executor-client"), Arc::new(Empty)).unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    client.connect_mem_pair(&server).await.unwrap();
    let first = Arc::new(Executor(AtomicUsize::new(0), None));
    let second = Arc::new(Executor(AtomicUsize::new(0), None));
    server.set_component_executor(Some(first.clone()));
    let installed = client.stop_component("executor-server", "/test").await;
    server.set_component_executor(Some(second.clone()));
    let replaced = client.stop_component("executor-server", "/test").await;
    server.set_component_executor(None);
    let removed = client.stop_component("executor-server", "/test").await;
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    assert!(installed.is_ok(), "late install was ignored: {installed:?}");
    assert!(replaced.is_ok(), "replacement was ignored: {replaced:?}");
    assert!(
        matches!(removed, Err(parrot_remote::RemoteError::AdminRejected { operation: "stop", code: 0x0a02, detail })
        if detail == "no ComponentExecutor installed on this node")
    );
    assert_eq!(first.0.load(Ordering::SeqCst), 1);
    assert_eq!(second.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn executor_replacement_does_not_lock_or_cancel_inflight_command() {
    let server = RemoteActorSystem::new(
        RemoteConfig::mem("executor-inflight-server"),
        Arc::new(Empty),
    )
    .unwrap();
    let client = RemoteActorSystem::new(
        RemoteConfig::mem("executor-inflight-client"),
        Arc::new(Empty),
    )
    .unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    client.connect_mem_pair(&server).await.unwrap();
    let blocked = Arc::new(Blocked {
        entered: Default::default(),
        release: tokio::sync::Semaphore::new(0),
    });
    let first = Arc::new(Executor(AtomicUsize::new(0), Some(blocked.clone())));
    let second = Arc::new(Executor(AtomicUsize::new(0), None));
    server.set_component_executor(Some(first.clone()));
    let caller = client.clone();
    let command = tokio::spawn(async move {
        caller
            .stop_component("executor-inflight-server", "/test")
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), blocked.entered.notified())
        .await
        .unwrap();
    let writer = server.clone();
    let replacement = second.clone();
    let mut replace =
        tokio::task::spawn_blocking(move || writer.set_component_executor(Some(replacement)));
    let ready = tokio::time::timeout(Duration::from_secs(2), &mut replace).await;
    // Release even on failure: a held read lock must not hang the test runtime.
    blocked.release.add_permits(1);
    command.await.unwrap().unwrap();
    let unlocked = ready.is_ok();
    if let Ok(result) = ready {
        result.unwrap();
    } else {
        replace.await.unwrap();
    }
    client
        .stop_component("executor-inflight-server", "/next")
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    assert!(
        unlocked,
        "executor slot read lock was held across command execution"
    );
    assert_eq!(first.0.load(Ordering::SeqCst), 1);
    assert_eq!(second.0.load(Ordering::SeqCst), 1);
}

#[test]
fn system_can_be_dropped_after_its_runtime() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sys = runtime.block_on(async {
        let sys = RemoteActorSystem::new(
            RemoteConfig::tcp("drop-after-runtime", Some("127.0.0.1:0".parse().unwrap())),
            Arc::new(Empty),
        )
        .unwrap();
        sys.start().await.unwrap();
        sys
    });
    let addr = sys.local_addr().unwrap();
    let weak = Arc::downgrade(&sys);
    drop(runtime);
    drop(sys);
    assert_eq!(weak.strong_count(), 0);
    std::net::TcpListener::bind(addr)
        .expect("drop must release the listener without spawning cleanup");
}

#[tokio::test]
async fn component_admin_failures_preserve_codes_without_parsing_diagnostics() {
    use parrot_remote::admin_v2::{AdminArtifactRef, AdminInstancePolicy};
    let server =
        RemoteActorSystem::new(RemoteConfig::mem("reject-server"), Arc::new(Empty)).unwrap();
    let client =
        RemoteActorSystem::new(RemoteConfig::mem("reject-client"), Arc::new(Empty)).unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    client.connect_mem_pair(&server).await.unwrap();
    let deploy = ComponentDeploy {
        name: "example".into(),
        version: "1".into(),
        artifact: AdminArtifactRef::Beam {
            app: "example".into(),
            uri: None,
        },
        instances: AdminInstancePolicy::Singleton,
        config: None,
    };
    let failures = [
        (
            "deploy",
            client
                .deploy_component("reject-server", deploy)
                .await
                .unwrap_err(),
        ),
        (
            "drain",
            client
                .drain_component("reject-server", "/user/example", Duration::from_secs(1))
                .await
                .unwrap_err(),
        ),
        (
            "stop",
            client
                .stop_component("reject-server", "/user/example")
                .await
                .unwrap_err(),
        ),
        (
            "status",
            client
                .component_status("reject-server", "/user/example")
                .await
                .unwrap_err(),
        ),
    ];
    for (expected, failure) in failures {
        match failure {
            parrot_remote::RemoteError::AdminRejected {
                operation,
                code,
                detail,
            } => {
                assert_eq!(operation, expected);
                assert_eq!(code, 0x0a02);
                assert!(!detail.is_empty());
            }
            other => panic!("lost structured admin rejection: {other:?}"),
        }
    }
    assert!(matches!(
        client.stop_component("absent-node", "/user/example").await,
        Err(parrot_remote::RemoteError::UnknownNode(_))
    ));
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}
