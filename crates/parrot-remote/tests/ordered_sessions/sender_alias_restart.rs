// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use parrot_api::sender::ActorSender;
use parrot_remote::{handshake::TopologyRole, tls::TlsConfig};

const ALIAS: &str = "parrot://alias-hub/alias";
const TARGET: &str = "parrot://recovery-target/target";
const WRITER: &str = "/user/writer";

struct Alias(&'static str);

#[async_trait::async_trait]
impl LocalLookup for Alias {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
    fn forwarding_target(&self, path: &str) -> Option<String> {
        (path == "/alias").then(|| self.0.to_owned())
    }
}

async fn alias_node(tls: TlsConfig, path: &'static str) -> Arc<RemoteActorSystem> {
    let cfg = RemoteConfig::tcp("alias-hub", Some("127.0.0.1:0".parse().unwrap()))
        .with_role(TopologyRole::Hub)
        .with_mtls(tls, []);
    let node = RemoteActorSystem::new(cfg, Arc::new(Alias(path))).unwrap();
    node.start().await.unwrap();
    node
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "source subprocess driven by the mTLS alias restart contracts"]
async fn sender_fixture() {
    let dir = PathBuf::from(std::env::var_os("PARROT_SENDER_DIR").unwrap());
    let target = std::env::var("PARROT_ALIAS_ADDR").unwrap().parse().unwrap();
    let tls = TlsConfig {
        ca_pem: std::fs::read(dir.join("ca.pem")).unwrap(),
        cert_pem: std::fs::read(dir.join("source.pem")).unwrap(),
        key_pem: std::fs::read(dir.join("source.key")).unwrap(),
    };
    let source = start(sender_config(&dir).with_mtls(tls, []), state(None).0).await;
    source
        .connect(&NodeAddr::tcp("alias-hub", target))
        .await
        .unwrap();
    let reference = source.remote_ref(ALIAS).unwrap();
    let owner = ActorSender::new(WRITER);
    owner.scope(send(&reference, 700)).await;
    let links = source.links_snapshot().await;
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].0, "alias-hub");
    std::fs::write(dir.join("sent"), b"sent").unwrap();
    std::future::pending::<()>().await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    Direct,
    RestartedAlias,
    AliasWithForeignDirect,
    RemappedAliasWithOriginalDirect,
}

async fn recover(route: Route) {
    let directory = tempfile::tempdir().unwrap();
    let pki = crate::pki::Pki::new();
    let source_tls = pki.node("recovery-source");
    let alias_tls = pki.node("alias-hub");
    for (name, pem) in [
        ("source.pem", &source_tls.cert_pem),
        ("source.key", &source_tls.key_pem),
        ("ca.pem", &source_tls.ca_pem),
    ] {
        std::fs::write(directory.path().join(name), pem).unwrap();
    }
    let alias = alias_node(alias_tls.clone(), TARGET).await;
    let alias_addr = NodeAddr::tcp("alias-hub", alias.local_addr().unwrap());
    let (old_delivery, mut rx) = state(None);
    let release = Release(old_delivery.clone());
    let target = start(
        RemoteConfig::tcp("recovery-target", Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(pki.node("recovery-target"), [])
            .with_ordered_relays(["alias-hub".into()]),
        old_delivery.clone(),
    )
    .await;
    target.connect(&alias_addr).await.unwrap();
    let mut child = Process(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sender_restart::alias_restart::sender_fixture",
                "--ignored",
            ])
            .env("PARROT_SENDER_DIR", directory.path())
            .env("PARROT_ALIAS_ADDR", alias.local_addr().unwrap().to_string())
            .spawn()
            .unwrap(),
    );
    wait_file(&directory.path().join("sent")).await;
    let timeout = Duration::from_secs(5);
    tokio::time::timeout(timeout, old_delivery.entered.notified())
        .await
        .unwrap();
    assert_eq!(child.kill_and_reap().signal(), Some(libc::SIGKILL));
    alias.shutdown().await.unwrap();

    let source = start(
        sender_config(directory.path()).with_mtls(source_tls, []),
        state(None).0,
    )
    .await;
    let replacement = ActorSender::new(WRITER);
    let foreign = if route == Route::AliasWithForeignDirect {
        let node = start(
            RemoteConfig::tcp("recovery-target", Some("127.0.0.1:0".parse().unwrap()))
                .with_mtls(pki.node("recovery-target"), []),
            state(None).0,
        )
        .await;
        connect(&source, &node).await;
        let reference = source.remote_ref(TARGET).unwrap();
        assert!(replacement
            .in_scope(|| reference.close_ordered_session(Duration::from_millis(550)))
            .await
            .is_err());
        Some(node)
    } else {
        None
    };
    let direct = route == Route::Direct;
    let new_alias = if direct {
        connect(&source, &target).await;
        None
    } else {
        let remapped = route == Route::RemappedAliasWithOriginalDirect;
        let path = if remapped {
            "parrot://recovery-target/moved"
        } else {
            TARGET
        };
        let alias = alias_node(alias_tls, path).await;
        let addr = NodeAddr::tcp("alias-hub", alias.local_addr().unwrap());
        target.connect(&addr).await.unwrap();
        source.connect(&addr).await.unwrap();
        if remapped {
            let alias_ref = source.remote_ref(ALIAS).unwrap();
            assert!(replacement
                .in_scope(|| alias_ref.close_ordered_session(Duration::from_millis(550)))
                .await
                .is_err());
            connect(&source, &target).await;
        }
        Some(alias)
    };
    let reference = source
        .remote_ref(
            if direct || route == Route::RemappedAliasWithOriginalDirect {
                TARGET
            } else {
                ALIAS
            },
        )
        .unwrap();
    tokio::time::timeout(timeout, old_delivery.dropping.notified())
        .await
        .expect("restored sender failed to find the original receiver");
    if let Some(node) = foreign {
        node.shutdown().await.unwrap();
        tokio::time::timeout(timeout, async {
            while source
                .links_snapshot()
                .await
                .iter()
                .any(|(name, peer, _)| name == "recovery-target" && !peer.is_closed())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    let sibling = ActorSender::new("/user/sibling");
    sibling.scope(send(&reference, 91)).await;
    seen(&mut rx, 91).await;
    assert!(replacement
        .in_scope(|| reference.close_ordered_session(Duration::from_millis(80)))
        .await
        .is_err());
    assert!(!old_delivery.dropped.load(Ordering::SeqCst));
    release.release();
    replacement
        .in_scope(|| reference.close_ordered_session(timeout))
        .await
        .unwrap();
    assert!(old_delivery.dropped.load(Ordering::SeqCst));
    replacement.scope(send(&reference, 92)).await;
    seen(&mut rx, 92).await;
    sibling.scope(send(&reference, 93)).await;
    seen(&mut rx, 93).await;
    for owner in [sibling, replacement] {
        owner
            .in_scope(|| reference.close_ordered_session(timeout))
            .await
            .unwrap();
    }
    source.shutdown().await.unwrap();
    if let Some(alias) = new_alias {
        alias.shutdown().await.unwrap();
    }
    target.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_actor_sender_retires_over_the_only_direct_link_after_alias_loss() {
    recover(Route::Direct).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_actor_sender_retires_through_a_restarted_alias_with_a_new_route_key() {
    recover(Route::RestartedAlias).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_name_direct_receiver_cannot_hide_or_certify_an_old_alias_ticket() {
    recover(Route::AliasWithForeignDirect).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remapped_alias_cannot_hide_the_reachable_original_receiver() {
    recover(Route::RemappedAliasWithOriginalDirect).await;
}
