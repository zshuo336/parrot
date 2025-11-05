//! Run the actual application; gossip addresses must not satisfy registration.
use parrot_remote::{
    swim::{Member, MemberEvent, MemberStatus, NodeAddrWire},
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    net::SocketAddr,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
async fn node(id: &str) -> Arc<RemoteActorSystem> {
    let node = RemoteActorSystem::new(
        RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap())).with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    node.start().await.unwrap();
    node
}

struct Application {
    child: Child,
    directory: PathBuf,
}
impl Application {
    fn start(executable: &str, app: &str, serve_only: bool) -> Self {
        static SERIAL: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "{app}-registry-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let log = std::fs::File::create(directory.join("output.log")).unwrap();
        let mut command = Command::new(executable);
        command
            .args([
                "--bind",
                "127.0.0.1:0",
                "--wait",
                "3",
                "--insecure-development",
            ])
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        if app == "websearch" {
            command
                .args(["--port", "0", "--data"])
                .arg(directory.join("data"));
            if serve_only {
                command.arg("--serve-only");
            }
        } else {
            command.args(["--pages", "0"]);
        }
        Self {
            child: command.spawn().unwrap(),
            directory,
        }
    }
    fn output(&self) -> String {
        std::fs::read_to_string(self.directory.join("output.log")).unwrap()
    }
    async fn address(&mut self) -> SocketAddr {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let output = self.output();
                if let Some(address) = output
                    .split("应用监听 ")
                    .nth(1)
                    .and_then(|s| s.split('，').next())
                {
                    return address.parse().expect("real application listener address");
                }
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "application exited before binding: {output}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("application startup deadline")
    }
}
impl Drop for Application {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub async fn gossip_is_not_registration(executable: &str, app: &str, serve_only: bool) {
    let middle = node("registry-middle").await;
    // Real bound TCP address, deliberately without a protocol handshake. Gossip
    // can legitimately still carry such an address after a gateway goes away.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let missing: &[&str] = if serve_only {
        &["jvm-search-1"]
    } else {
        &["erl-gw-1", "ray-gw-1", "jvm-search-1"]
    };
    for id in missing {
        middle
            .membership
            .lock()
            .await
            .merge_event(&MemberEvent::Upsert(Member {
                node_id: (*id).into(),
                addr: NodeAddrWire {
                    node_id: (*id).into(),
                    scheme: "tcp".into(),
                    host: "127.0.0.1".into(),
                    port: silent.local_addr().unwrap().port(),
                },
                incarnation: 1,
                status: MemberStatus::Alive,
                status_until_ms: 0,
                metadata: Default::default(),
            }));
    }
    let mut application = Application::start(executable, app, serve_only);
    let address = application.address().await;
    middle.connect(&NodeAddr::tcp(app, address)).await.unwrap();
    // Enough unrelated real connections to pass the old count-only gate.
    let mut unrelated = Vec::new();
    for n in 1..missing.len() {
        let peer = node(&format!("unrelated-{n}")).await;
        peer.connect(&NodeAddr::tcp(app, address)).await.unwrap();
        unrelated.push(peer);
    }
    let status = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let output = application.output();
            assert!(!output.contains("已全部注册"), "gossip address and unrelated connections falsely passed registry readiness: {output}");
            if let Some(status) = application.child.try_wait().unwrap() { break status; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("registry deadline must terminate the actual process");
    let output = application.output();
    assert_eq!(
        status.code(),
        Some(if app == "websearch" { 3 } else { 1 }),
        "{output}"
    );
    assert!(output.contains("等待网关注册超时"), "{output}");
    assert!(!output.contains("已全部注册"), "{output}");
    for id in missing {
        assert!(
            output.contains(id),
            "timeout must identify missing gateway {id}: {output}"
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), silent.accept())
            .await
            .is_err(),
        "registry must not silently dial a merely advertised gateway"
    );
    for peer in unrelated {
        peer.shutdown().await.unwrap();
    }
    middle.shutdown().await.unwrap();
}

pub async fn direct_registration(executable: &str, app: &str, serve_only: bool) {
    let required: &[&str] = if serve_only {
        &["jvm-search-1"]
    } else {
        &["erl-gw-1", "ray-gw-1", "jvm-search-1"]
    };
    let mut application = Application::start(executable, app, serve_only);
    let address = application.address().await;
    let mut peers = Vec::new();
    for id in required {
        let peer = node(id).await;
        peer.connect(&NodeAddr::tcp(app, address)).await.unwrap();
        peers.push(peer);
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let output = application.output();
            if output.contains("已全部注册") {
                break;
            }
            assert!(
                application.child.try_wait().unwrap().is_none(),
                "application rejected actual registrations: {output}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the configured set of gateways must suffice for registration");
    // Registration only; fixtures do not implement the polyglot components.
    drop(application);
    for peer in peers {
        peer.shutdown().await.unwrap();
    }
}
