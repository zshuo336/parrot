//! DNS changes cross the actual libc resolver boundary in one isolated child.
#![cfg(unix)]
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};
use std::{path::PathBuf, sync::Arc, time::Duration};
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Ping(u64);
parrot_api::message::inventory::submit! {parrot_api::message::CodecRegistration {
    type_key:"bin:parrot.seed.dns.Ping#v1",type_id:std::any::TypeId::of::<Ping>(),
    encode:|m:&BoxedMessage|parrot_api::message::serde_remote_serialize(m.downcast_ref::<Ping>().ok_or("Ping")?),
    decode:|b:&[u8]|{let m:Ping=parrot_api::message::serde_remote_deserialize(b)?;Ok(Box::new(m) as BoxedMessage)},
}}
#[derive(Debug)]
struct Echo;
impl ActorRef for Echo {
    fn send<'a>(&'a self, m: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(m) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        m: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(m)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/echo".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Echo)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Lookup;
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        Some(Box::new(Echo))
    }
}
async fn echo(client: &Arc<RemoteActorSystem>) {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if let Ok(actor) = client.remote_ref("parrot://seed-dns-server/echo") {
                if let Ok(reply) = actor
                    .send_with_timeout(Box::new(Ping(42)), Some(Duration::from_millis(100)))
                    .await
                {
                    assert_eq!(reply.downcast_ref::<Ping>().unwrap().0, 42);
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("configured DNS seed must refresh hostname and serve real business");
}
fn write_state(path: &std::path::Path, address: &str) {
    let next = path.with_extension("next");
    std::fs::write(&next, format!("{address}\n")).unwrap();
    std::fs::rename(next, path).unwrap();
}
async fn child_case(reconnect: bool, inferred: bool) {
    let state = PathBuf::from(std::env::var_os("PARROT_TEST_DNS_STATE").unwrap());
    let initial = if reconnect { "127.0.0.1" } else { "127.0.0.2" };
    write_state(&state, initial);
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("seed-dns-server", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup),
    )
    .unwrap();
    server.start().await.unwrap();
    let mut builder = parrot_config::ParrotConfig::builder().remote_node_id("seed-dns-client");
    let identity = if inferred { "" } else { "seed-dns-server@" };
    builder.remote.node.seeds = Some(vec![format!(
        "tcp://{identity}changing.parrot.test:{}",
        server.local_addr().unwrap().port()
    )]);
    let resolved = builder.build().unwrap();
    let config = RemoteConfig::from_resolved_async(&resolved, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(!config.seeds.is_empty());
    assert!(
        config
            .seeds
            .iter()
            .all(|address| address.addr.ip() == initial.parse::<std::net::IpAddr>().unwrap()),
        "test resolver must intercept the real startup DNS call"
    );
    println!("controlled resolver active; startup snapshot is {initial}");
    let client = RemoteActorSystem::new(config, Arc::new(Lookup)).unwrap();
    client.start().await.unwrap();
    if reconnect {
        echo(&client).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            client.links_snapshot().await.len(),
            1,
            "healthy seed must not be dialed repeatedly"
        );
        let address = server.local_addr().unwrap();
        server.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !client.links_snapshot().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        println!("old seed socket retired; restarting the same identity at a new DNS address");
        let address = std::net::SocketAddr::new("::1".parse().unwrap(), address.port());
        write_state(&state, "::1");
        let replacement = RemoteActorSystem::new(
            RemoteConfig::tcp("seed-dns-server", Some(address)),
            Arc::new(Lookup),
        )
        .unwrap();
        replacement.start().await.unwrap();
        echo(&client).await;
        assert!(
            client
                .nodes
                .get("seed-dns-server")
                .unwrap()
                .0
                .addr
                .is_ipv6(),
            "old DNS IP must not shadow the current connection"
        );
        println!(
            "reconnected actual peer at {}",
            client.nodes.get("seed-dns-server").unwrap().0.addr
        );
        replacement.shutdown().await.unwrap();
    } else {
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(client.links_snapshot().await.is_empty());
        write_state(&state, "127.0.0.1");
        echo(&client).await;
        println!(
            "refreshed actual peer at {}",
            client.nodes.get("seed-dns-server").unwrap().0.addr
        );
    }
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}
struct OwnedChild(std::process::Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn run_case(reconnect: bool, inferred: bool, test_name: &str) {
    if std::env::var_os("PARROT_TEST_DNS_CHILD").is_some() {
        child_case(reconnect, inferred).await;
        return;
    }
    let dir = std::env::temp_dir().join(format!("parrot-seed-dns-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let library = dir.join(if cfg!(target_os = "macos") {
        "resolver.dylib"
    } else {
        "resolver.so"
    });
    let mut compile = std::process::Command::new("cc");
    compile.args(["-Wall", "-Wextra", "-Werror", "-fPIC"]);
    if cfg!(target_os = "macos") {
        compile.arg("-dynamiclib");
    } else {
        compile.arg("-shared");
    }
    compile
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dns/resolver.c"))
        .arg("-o")
        .arg(&library);
    if !cfg!(target_os = "macos") {
        compile.arg("-ldl");
    }
    let build = compile.output().unwrap();
    assert!(
        build.status.success(),
        "resolver fixture compilation: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let logfile = dir.join("child.log");
    let output = std::fs::File::create(&logfile).unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test_name, "--nocapture"])
        .env("PARROT_TEST_DNS_CHILD", "1")
        .env("PARROT_TEST_DNS_STATE", dir.join("state"));
    command.env(
        if cfg!(target_os = "macos") {
            "DYLD_INSERT_LIBRARIES"
        } else {
            "LD_PRELOAD"
        },
        &library,
    );
    let mut child = OwnedChild(
        command
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "owned child timed out; logs: {}",
            logfile.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let log = std::fs::read_to_string(&logfile).unwrap();
    assert!(
        log.contains("controlled resolver active"),
        "resolver fixture did not establish its boundary: {log}"
    );
    assert!(
        status.success(),
        "real resolver refresh child failed: {log}"
    );
    println!(
        "verified seed child {} reconnect={reconnect} inferred={inferred}:\n{log}",
        child.0.id()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn configured_dns_seed_refreshes_after_an_offline_address_changes() {
    run_case(
        false,
        false,
        "configured_dns_seed_refreshes_after_an_offline_address_changes",
    )
    .await;
}
#[tokio::test]
async fn configured_dns_seed_reconnects_after_a_successful_connection_is_lost() {
    run_case(
        true,
        false,
        "configured_dns_seed_reconnects_after_a_successful_connection_is_lost",
    )
    .await;
}

#[tokio::test]
async fn inferred_dns_seed_keeps_learned_identity_and_recovers_after_dns_move() {
    run_case(
        true,
        true,
        "inferred_dns_seed_keeps_learned_identity_and_recovers_after_dns_move",
    )
    .await;
}
