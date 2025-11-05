//! A deployment probe must reject incomplete inputs and actually use the claimed relay.
use std::{
    path::PathBuf,
    process::Stdio,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, Command},
};

struct Node {
    child: Child,
    directory: PathBuf,
    endpoint: String,
}
impl Node {
    async fn start(id: &str, seeds: &str) -> Self {
        static SERIAL: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "parrot-probe-test-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_parrot-node"));
        for key in [
            "PARROT_CONFIG",
            "PARROT_SECURITY_MODE",
            "PARROT_TLS_CERT",
            "PARROT_TLS_KEY",
            "PARROT_TLS_CA",
            "PARROT_ADMIN_NODES",
            "PARROT_READY_FILE",
            "PARROT_HEALTHCHECK",
            "PARROT_SCHEME",
        ] {
            cmd.env_remove(key);
        }
        let mut child = cmd
            .current_dir(&directory)
            .env("PARROT_NODE_ID", id)
            .env("PARROT_BIND", "127.0.0.1:0")
            .env("PARROT_SEEDS", seeds)
            .env("PARROT_SECURITY_MODE", "insecure-development")
            .env("PARROT_ACTORS", "echo,counter,kv,slow")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap().take(65536)).lines();
        let endpoint = tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("node exited before READY");
                if let Some(endpoint) = line.strip_prefix("PARROT_NODE_READY=") {
                    break endpoint.to_owned();
                }
            }
        })
        .await
        .expect("node startup deadline");
        Self {
            child,
            directory,
            endpoint,
        }
    }
    async fn stop(&mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

async fn probe(args: &[String]) -> std::process::Output {
    tokio::time::timeout(
        Duration::from_secs(180),
        Command::new(env!("CARGO_BIN_EXE_parrot-probe"))
            .args(args)
            .arg("--insecure-development")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("probe process deadline")
    .unwrap()
}
#[tokio::test]
async fn malformed_target_must_not_be_silently_skipped() {
    let mut node = Node::start("valid", "").await;
    let result = probe(&[
        format!("valid={}", node.endpoint),
        "this-is-not-a-target".into(),
        "--smoke".into(),
    ])
    .await;
    node.stop().await;
    assert_eq!(
        result.status.code(),
        Some(2),
        "invalid input was accepted: {}",
        String::from_utf8_lossy(&result.stdout)
    );
}
#[tokio::test]
async fn disconnected_nodes_must_fail_cross_route_even_when_each_is_reachable() {
    let mut a = Node::start("isolated-a", "").await;
    let mut b = Node::start("isolated-b", "").await;
    let result = probe(&[
        format!("isolated-a={}", a.endpoint),
        format!("isolated-b={}", b.endpoint),
        "--smoke".into(),
    ])
    .await;
    let remaining = actor_count("isolated-a", &a.endpoint).await;
    a.stop().await;
    b.stop().await;
    assert!(
        !result.status.success(),
        "disconnected nodes reported route success: {}",
        String::from_utf8_lossy(&result.stdout)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("P2 cross-route"));
    assert_eq!(remaining, 4, "failed probe leaked its actors");
}

struct EmptyLookup;
#[async_trait::async_trait]
impl parrot_remote::LocalLookup for EmptyLookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
async fn actor_count(id: &str, endpoint: &str) -> u64 {
    let client = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("probe-test-inspector", None),
        std::sync::Arc::new(EmptyLookup),
    )
    .unwrap();
    client.start().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        client.connect(&parrot_remote::NodeAddr::tcp(id, endpoint.parse().unwrap())),
    )
    .await
    .unwrap()
    .unwrap();
    let count = client.metrics_report(id).await.unwrap().processes;
    client.shutdown().await.unwrap();
    count
}
#[tokio::test]
async fn connected_pair_passes_full_checks_twice_without_leaking_actors() {
    let mut a = Node::start("connected-a", "").await;
    let mut b = Node::start("connected-b", &format!("connected-a={}", a.endpoint)).await;
    for mode in ["--full", "--smoke"] {
        let result = probe(&[
            format!("connected-a={}", a.endpoint),
            format!("connected-b={}", b.endpoint),
            mode.into(),
        ])
        .await;
        let output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.status.success(), "{output}");
        assert!(output.contains("PASS P2 cross-route connected-a -> connected-b"));
        assert!(output.contains("PASS P2 cross-route connected-b -> connected-a"));
        if mode == "--full" {
            for label in ["PASS P4", "PASS P5", "PASS P6", "PASS P7"] {
                assert!(output.contains(label), "{output}");
            }
        }
        assert_eq!(
            actor_count("connected-a", &a.endpoint).await,
            4,
            "successful probe leaked actors"
        );
    }
    let args = [
        format!("connected-a={}", a.endpoint),
        format!("connected-b={}", b.endpoint),
        "--smoke".into(),
    ];
    let (first, second) = tokio::join!(probe(&args), probe(&args));
    for result in [first, second] {
        assert!(
            result.status.success(),
            "concurrent probe failed: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(
        actor_count("connected-a", &a.endpoint).await,
        4,
        "concurrent probes leaked actors"
    );
    a.stop().await;
    b.stop().await;
}
#[tokio::test]
async fn wrong_peer_identity_fails_before_any_business_check() {
    let mut node = Node::start("actual-node", "").await;
    let result = probe(&[
        format!("different-node={}", node.endpoint),
        "--smoke".into(),
    ])
    .await;
    node.stop().await;
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("identity mismatch"));
    assert!(!String::from_utf8_lossy(&result.stdout).contains("PASS P1"));
}
