//! Deployment configuration drives actual facade checks and TCP discovery.
//! Published paths here are synthetic: this suite does not claim actor execution.
use parrot_api::{
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey},
    types::BoxedActorRef,
};
use parrot_config::ParrotConfig;
use parrot_remote::{acl::AclDecision, LocalLookup, RemoteActorSystem, RemoteConfig};
use std::{io::Write, sync::Arc, time::Duration};

struct NoActors;
#[async_trait::async_trait]
impl LocalLookup for NoActors {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}

fn file_config(text: &str) -> ParrotConfig {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(text.as_bytes()).unwrap();
    ParrotConfig::builder()
        .load_file(file.path())
        .expect("deployment receptionist grants must be accepted")
}

fn config(node: &str, grants: &str) -> RemoteConfig {
    let resolved = file_config(&format!(
        "[remote.node]\nnode_id = '{node}'\nbind = '127.0.0.1:0'\n[remote.security]\nmode = 'insecure-development'\n{grants}"
    ))
    .build()
    .unwrap();
    RemoteConfig::from_resolved(&resolved)
        .unwrap()
        .with_swim_config(parrot_remote::swim::SwimConfig {
            gossip_interval: Duration::from_millis(20),
            ..Default::default()
        })
}

fn key(text: &str) -> ReceptionistKey {
    ReceptionistKey::new(text).unwrap()
}

#[tokio::test]
async fn configured_grants_enforce_real_gateway_operations_and_segment_boundaries() {
    let cfg = config(
        "server",
        "[remote.receptionist.roles]\nserver = ['edge/public/']",
    );
    let node = RemoteActorSystem::new(cfg, Arc::new(NoActors)).unwrap();
    let gateway = node.receptionist_gateway();
    let mut events = gateway
        .subscribe(key("edge/public/query"))
        .await
        .expect("configured service grant must authorize subscriptions");
    gateway
        .register(key("edge/public/query"), "/user/query".into())
        .await
        .unwrap();
    let expected = "parrot://server/user/query".to_owned();
    assert_eq!(
        events.try_recv().unwrap(),
        ReceptionistEvent::Registered {
            key: key("edge/public/query"),
            remote_path: expected.clone(),
        }
    );
    for denied in ["edge/publicity/query", "edge/private", "cloud/query"] {
        assert!(
            gateway.subscribe(key(denied)).await.is_err(),
            "lookalike or unrelated service must remain denied"
        );
        assert!(gateway
            .register(key(denied), "/user/query".into())
            .await
            .is_err());
        assert!(gateway
            .deregister(key(denied), "/user/query".into())
            .await
            .is_err());
    }
    gateway
        .deregister(key("edge/public/query"), "/user/query".into())
        .await
        .unwrap();
    assert_eq!(
        events.try_recv().unwrap(),
        ReceptionistEvent::Unregistered {
            key: key("edge/public/query"),
            remote_path: expected,
        }
    );
    node.shutdown().await.unwrap();
}

#[test]
fn effective_map_replaces_grants_without_silently_retaining_lower_privileges() {
    let lower =
        file_config("[remote.receptionist.roles]\nserver = ['edge/public']\nremoved = ['cloud']");
    let higher = file_config("[remote.receptionist.roles]\nserver = ['edge/private']");
    let cfg = RemoteConfig::from_resolved(&higher.with_fallback(lower).build().unwrap()).unwrap();
    for (node, service, decision) in [
        ("server", "edge/private/query", AclDecision::Allow),
        ("server", "edge/public/query", AclDecision::Deny),
        ("removed", "cloud/query", AclDecision::Deny),
        ("unknown", "edge/private/query", AclDecision::Deny),
    ] {
        assert_eq!(
            cfg.receptionist_acl.check(node, service),
            decision,
            "effective map must preserve exactly the declared grants"
        );
    }
}

#[tokio::test]
async fn configured_acl_authorizes_tcp_snapshot_publication_and_withdrawal() {
    let grants = "[remote.receptionist.roles]\nserver = ['edge/public']\nclient = ['edge/public']";
    let server = RemoteActorSystem::new(config("server", grants), Arc::new(NoActors)).unwrap();
    server.start().await.unwrap();
    let mut client_cfg = config("client", grants);
    client_cfg.seeds.push(parrot_remote::NodeAddr {
        node_id: "server".into(),
        identity_is_explicit: true,
        scheme: "tcp".into(),
        addr: server.local_addr().unwrap(),
    });
    let client = RemoteActorSystem::new(client_cfg, Arc::new(NoActors)).unwrap();
    let mut events = client
        .receptionist_gateway()
        .subscribe(key("edge/public/query"))
        .await
        .unwrap();
    let server_gateway = server.receptionist_gateway();
    server_gateway
        .register(key("edge/public/query"), "/user/query".into())
        .await
        .unwrap();
    client.start().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        assert_eq!(
            events.recv().await.unwrap(),
            ReceptionistEvent::Registered {
                key: key("edge/public/query"),
                remote_path: "parrot://server/user/query".into(),
            }
        );
        server_gateway
            .deregister(key("edge/public/query"), "/user/query".into())
            .await
            .unwrap();
        assert_eq!(
            events.recv().await.unwrap(),
            ReceptionistEvent::Unregistered {
                key: key("edge/public/query"),
                remote_path: "parrot://server/user/query".into(),
            }
        );
    })
    .await;
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    result.expect("configured direct-peer discovery did not publish and withdraw");
}

#[tokio::test]
async fn absent_grants_still_deny_but_explicit_permissive_policy_is_effective() {
    for (text, allowed) in [
        ("", false),
        ("[remote.receptionist]\ndefault_deny = false", true),
    ] {
        let node = RemoteActorSystem::new(config("server", text), Arc::new(NoActors)).unwrap();
        let gateway = node.receptionist_gateway();
        assert_eq!(gateway.subscribe(key("edge/query")).await.is_ok(), allowed);
        assert_eq!(
            gateway
                .register(key("edge/query"), "/user/query".into())
                .await
                .is_ok(),
            allowed
        );
        node.shutdown().await.unwrap();
    }
}
