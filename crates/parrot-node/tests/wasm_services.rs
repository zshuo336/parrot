#![cfg(feature = "wasm")]

use parrot_api::{
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey, ReceptionistStream},
    types::BoxedActorRef,
};
use parrot_app::host::wasm::{
    WasmMessageGrant, WasmMessageOperation, WasmServiceGrant, WasmServiceOperation,
    WasmServicePolicy,
};
use parrot_node::{HostOptions, NodeComponentExecutor, WasmAsk, WasmReply};
use parrot_remote::{acl::AclRules, admin_v2::*, LocalLookup, RemoteActorSystem, RemoteConfig};
use sha2::{Digest, Sha256};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

const KEY: &str = "edge/node-guest";
fn key() -> ReceptionistKey {
    ReceptionistKey::new(KEY).unwrap()
}
fn digest() -> String {
    format!(
        "{:x}",
        Sha256::digest(include_bytes!(
            "../../parrot-wasm/tests/fixtures/services-component.wasm"
        ))
    )
}
fn grant(name: &str, operation: WasmServiceOperation) -> WasmServiceGrant {
    WasmServiceGrant {
        source_path: format!("/user/{name}"),
        artifact_digest: digest(),
        key: KEY.into(),
        operation,
    }
}
fn grants() -> Vec<WasmServiceGrant> {
    vec![
        grant("guest", WasmServiceOperation::Register),
        grant("guest", WasmServiceOperation::Subscribe),
        grant("consumer", WasmServiceOperation::Subscribe),
        WasmServiceGrant {
            artifact_digest: "00".repeat(32),
            ..grant("wrongdigest", WasmServiceOperation::Register)
        },
    ]
}
fn policy_text(grants: &[WasmServiceGrant]) -> String {
    #[derive(serde::Serialize)]
    struct Policy<'a> {
        grants: &'a [WasmServiceGrant],
    }
    toml::to_string(&Policy { grants }).unwrap()
}
fn component(name: &str, injected: &str) -> ComponentDeploy {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../parrot-wasm/tests/fixtures/services-component.wasm")
        .canonicalize()
        .unwrap();
    let mut config = toml::Table::new();
    config.insert("key".into(), KEY.into());
    config.insert("wasm-service-policy".into(), injected.into());
    config.insert("service-grants".into(), injected.into());
    ComponentDeploy {
        name: name.into(),
        version: "1.0.0".into(),
        artifact: AdminArtifactRef::Wasm {
            digest: digest(),
            uri: url::Url::from_file_path(path).unwrap().into(),
        },
        instances: AdminInstancePolicy::Singleton,
        config: Some(toml::to_string(&config).unwrap().into_bytes()),
    }
}
async fn guarded(future: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(120), future)
        .await
        .expect("Node service lifecycle deadline");
}
async fn call(actor: &BoxedActorRef, command: &str, payload: &[u8]) -> Vec<u8> {
    tokio::time::timeout(
        Duration::from_secs(10),
        actor.send(Box::new(WasmAsk {
            type_key: command.into(),
            payload: payload.into(),
        })),
    )
    .await
    .unwrap()
    .unwrap()
    .downcast::<WasmReply>()
    .unwrap()
    .0
}
async fn event(stream: &mut ReceptionistStream, registered: bool, path: &str) {
    let value = tokio::time::timeout(Duration::from_secs(10), stream.recv())
        .await
        .unwrap_or_else(|e| panic!("event registered={registered} path={path}: {e}"))
        .unwrap();
    assert_eq!(
        value,
        if registered {
            ReceptionistEvent::Registered {
                key: key(),
                remote_path: path.into(),
            }
        } else {
            ReceptionistEvent::Unregistered {
                key: key(),
                remote_path: path.into(),
            }
        }
    );
}

#[tokio::test]
async fn node_owner_normalizes_service_authority_and_rejects_live_changes() {
    guarded(async {
        let engine = parrot::thread::ThreadActorSystem::shared(Default::default());
        let facade = Arc::new(
            parrot::system::ParrotActorSystem::new(Default::default())
                .await
                .unwrap(),
        );
        facade
            .register_thread_system("eng".into(), engine.clone(), true)
            .await
            .unwrap();
        let gateway = Arc::new(
            parrot_remote::receptionist::ReceptionistFacade::new("owner-services")
                .with_acl(AclRules::permissive()),
        );
        facade
            .register_receptionist_gateway(gateway.clone())
            .await
            .unwrap();
        assert!(parrot_node::NODE
            .set(parrot_node::NodeState {
                ts: engine.clone(),
                facade,
            })
            .is_ok());
        let original = grants();
        let options = HostOptions {
            wasm_services: WasmServicePolicy::new(original.clone()).unwrap(),
            ..Default::default()
        };
        let executor =
            NodeComponentExecutor::with_host_options(engine.clone(), Some(10000), &options)
                .unwrap();
        assert!(matches!(
            executor.deploy(1, &component("guest", "ignored")).await,
            AdminReplyV2::Deployed { .. }
        ));
        let mut reordered = original.clone();
        reordered.reverse();
        for item in &mut reordered {
            item.artifact_digest = format!("sha256:{}", item.artifact_digest.to_ascii_uppercase());
        }
        let equal = HostOptions {
            wasm_services: WasmServicePolicy::new(reordered).unwrap(),
            ..Default::default()
        };
        NodeComponentExecutor::with_host_options(engine.clone(), Some(10000), &equal).unwrap();
        let mut changed = original;
        changed[0].key = "edge/changed".into();
        for options in [
            HostOptions::default(),
            HostOptions {
                wasm_services: WasmServicePolicy::new(changed).unwrap(),
                ..Default::default()
            },
        ] {
            assert!(NodeComponentExecutor::with_host_options(
                engine.clone(),
                Some(10000),
                &options
            )
            .is_err());
        }
        let actor = engine.resolve_actor("/user/guest").unwrap();
        let mut observed = gateway.subscribe(key()).await.unwrap();
        assert_eq!(call(&actor, "register", b"").await, b"ok");
        event(&mut observed, true, "/user/guest").await;
        let shared = NodeComponentExecutor::new(engine.clone());
        assert!(matches!(
            shared.stop(2, "/user/guest").await,
            AdminReplyV2::Stopped { .. }
        ));
        event(&mut observed, false, "/user/guest").await;
        assert!(matches!(
            shared.deploy(3, &component("guest", "ignored")).await,
            AdminReplyV2::Deployed { .. }
        ));
        let replacement = engine.resolve_actor("/user/guest").unwrap();
        assert_eq!(call(&replacement, "register", b"").await, b"ok");
        event(&mut observed, true, "/user/guest").await;
        engine.shutdown_internal().await.unwrap();
        event(&mut observed, false, "/user/guest").await;
    })
    .await;
}

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}

#[cfg(unix)]
async fn process(scheme: &str, authorized: bool) {
    let temp = tempfile::tempdir().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca = params.self_signed(&ca_key).unwrap().pem();
    let issuer = rcgen::Issuer::new(params, ca_key);
    std::fs::write(temp.path().join("ca.pem"), &ca).unwrap();
    let credentials = |name: &str| {
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &issuer).unwrap().pem();
        std::fs::write(temp.path().join(format!("{name}.crt")), &cert).unwrap();
        std::fs::write(temp.path().join(format!("{name}.key")), key.serialize_pem()).unwrap();
        parrot_remote::tls::TlsConfig {
            cert_pem: cert.into_bytes(),
            key_pem: key.serialize_pem().into_bytes(),
            ca_pem: ca.as_bytes().into(),
        }
    };
    credentials("services-node.test");
    let operator = credentials("services-operator.test");
    let policy = temp.path().join("operator services.toml");
    let initial = grants();
    std::fs::write(&policy, policy_text(&initial)).unwrap();
    let messaging_policy = temp.path().join("operator messages.toml");
    #[derive(serde::Serialize)]
    struct MessagingPolicy {
        grants: Vec<WasmMessageGrant>,
    }
    std::fs::write(
        &messaging_policy,
        toml::to_string(&MessagingPolicy {
            grants: vec![WasmMessageGrant {
                source_path: "/user/consumer".into(),
                artifact_digest: digest(),
                destination: "parrot://services-node.test/user/guest".into(),
                request_type_key: "bin:parrot.wasm.Ask#v1".into(),
                operation: WasmMessageOperation::Ask {
                    reply_type_key: "bin:parrot.wasm.Reply#v1".into(),
                },
            }],
        })
        .unwrap(),
    )
    .unwrap();
    let config = temp.path().join("node.toml");
    std::fs::write(&config, "[remote.receptionist]\ndefault_deny=false\n").unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-node"));
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("PARROT_"))
    {
        command.env_remove(key);
    }
    command
        .current_dir(temp.path())
        .env("PARROT_CONFIG", &config)
        .env("PARROT_NODE_ID", "services-node.test")
        .env("PARROT_BIND", "127.0.0.1:0")
        .env("PARROT_SCHEME", scheme)
        .env("PARROT_ACTORS", "")
        .env("PARROT_SECURITY_MODE", "mtls")
        .env("PARROT_ADMIN_NODES", "services-operator.test")
        .env("PARROT_WASM_EPOCH_TICKS", "10000")
        .env(
            "PARROT_TLS_CERT",
            temp.path().join("services-node.test.crt"),
        )
        .env("PARROT_TLS_KEY", temp.path().join("services-node.test.key"))
        .env("PARROT_TLS_CA", temp.path().join("ca.pem"));
    if authorized {
        command.env("PARROT_WASM_SERVICE_POLICY", &policy);
        command.env("PARROT_WASM_MESSAGING_POLICY", &messaging_policy);
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap().take(65536));
    let endpoint = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let mut line = String::new();
            assert!(
                lines.read_line(&mut line).await.unwrap() > 0,
                "Node exited before readiness"
            );
            if let Some(value) = line.strip_prefix("PARROT_NODE_READY=") {
                break value.trim().to_string();
            }
        }
    })
    .await
    .unwrap();
    let mut cfg = if scheme == "tcp" {
        RemoteConfig::tcp(
            "services-operator.test",
            Some("127.0.0.1:0".parse().unwrap()),
        )
    } else {
        RemoteConfig::quic(
            "services-operator.test",
            Some("127.0.0.1:0".parse().unwrap()),
        )
    }
    .with_mtls(operator, []);
    cfg.receptionist_acl = AclRules::permissive();
    let client = RemoteActorSystem::new(cfg, Arc::new(Empty)).unwrap();
    client.start().await.unwrap();
    let mut observed = client
        .receptionist_gateway()
        .subscribe(key())
        .await
        .unwrap();
    client
        .connect(&parrot_remote::NodeAddr {
            node_id: "services-node.test".into(),
            identity_is_explicit: true,
            scheme: scheme.into(),
            addr: endpoint.parse().unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(
        client.links_snapshot().await[0].1.authentication(),
        parrot_remote::security::PeerAuthentication::MutualTls
    );
    // The process owns its parsed authority; file edits and remote business data
    // cannot turn a new actor or a different artifact into an authorized source.
    let mut widened = initial;
    widened.push(grant("sibling", WasmServiceOperation::Register));
    let injected = policy_text(&widened);
    std::fs::write(&policy, &injected).unwrap();
    for name in ["guest", "consumer", "sibling", "wrongdigest"] {
        assert_eq!(
            client
                .deploy_component("services-node.test", component(name, &injected))
                .await
                .unwrap(),
            [format!("/user/{name}")]
        );
    }
    let actor = |name: &str| {
        Box::new(
            client
                .remote_ref(&format!("parrot://services-node.test/user/{name}"))
                .unwrap(),
        ) as BoxedActorRef
    };
    for name in ["sibling", "wrongdigest"] {
        assert_eq!(call(&actor(name), "register", b"").await, b"error:Disabled");
    }
    let guest = actor("guest");
    let consumer = actor("consumer");
    if authorized {
        assert_eq!(call(&consumer, "register", b"").await, b"error:Denied");
        assert_eq!(
            call(&guest, "register", b"edge/forbidden").await,
            b"error:Denied"
        );
        assert_eq!(call(&guest, "register", b"").await, b"ok");
        event(
            &mut observed,
            true,
            "parrot://services-node.test/user/guest",
        )
        .await;
        let request = parrot_api::message::serde_remote_serialize(&WasmAsk {
            type_key: "echo".into(),
            payload: b"guest-discovered-node-service".to_vec(),
        })
        .unwrap();
        let response = call(&consumer, "discover-ask", &request).await;
        assert_eq!(
            parrot_api::message::serde_remote_deserialize::<WasmReply>(&response)
                .unwrap()
                .0,
            b"guest-discovered-node-service"
        );
        let handle = call(&consumer, "subscribe", b"").await;
        assert_eq!(handle.len(), 8);
        assert_eq!(
            call(&consumer, "next", &handle).await,
            b"registered:parrot://services-node.test/user/guest"
        );
        assert_eq!(
            call(&guest, "echo", b"discovered-node-service").await,
            b"discovered-node-service"
        );
        assert_eq!(call(&guest, "deregister", b"").await, b"ok");
        event(
            &mut observed,
            false,
            "parrot://services-node.test/user/guest",
        )
        .await;
        assert_eq!(
            call(&consumer, "next", &handle).await,
            b"unregistered:parrot://services-node.test/user/guest"
        );
        assert_eq!(call(&guest, "register", b"").await, b"ok");
        event(
            &mut observed,
            true,
            "parrot://services-node.test/user/guest",
        )
        .await;
        assert_eq!(
            call(&consumer, "next", &handle).await,
            b"registered:parrot://services-node.test/user/guest"
        );
        client
            .stop_component("services-node.test", "/user/guest")
            .await
            .unwrap();
        event(
            &mut observed,
            false,
            "parrot://services-node.test/user/guest",
        )
        .await;
        assert_eq!(
            call(&consumer, "next", &handle).await,
            b"unregistered:parrot://services-node.test/user/guest"
        );
        assert_eq!(call(&consumer, "close", &handle).await, b"ok");
        assert_eq!(
            call(&consumer, "next", &handle).await,
            b"error:InvalidHandle"
        );
        client
            .deploy_component("services-node.test", component("guest", &injected))
            .await
            .unwrap();
        assert_eq!(call(&actor("guest"), "register", b"").await, b"ok");
        event(
            &mut observed,
            true,
            "parrot://services-node.test/user/guest",
        )
        .await;
        client
            .stop_component("services-node.test", "/user/guest")
            .await
            .unwrap();
        event(
            &mut observed,
            false,
            "parrot://services-node.test/user/guest",
        )
        .await;
    } else {
        assert_eq!(call(&guest, "register", b"").await, b"error:Disabled");
        assert_eq!(call(&consumer, "subscribe", b"").await, b"error:Disabled");
    }
    assert!(matches!(
        observed.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(tokio::process::Command::new("kill")
        .args(["-TERM", &child.id().unwrap().to_string()])
        .status()
        .await
        .unwrap()
        .success());
    assert!(tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    client.shutdown().await.unwrap();
    if scheme == "tcp" {
        assert!(tokio::net::TcpListener::bind(endpoint).await.is_ok());
    } else {
        assert!(tokio::net::UdpSocket::bind(endpoint).await.is_ok());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn mtls_tcp_node_discovers_guest_services_and_retires_generations() {
    guarded(process("tcp", true)).await;
}
#[cfg(unix)]
#[tokio::test]
async fn mtls_quic_node_discovers_guest_services_and_retires_generations() {
    guarded(process("quic", true)).await;
}
#[cfg(unix)]
#[tokio::test]
async fn remote_business_policy_cannot_authorize_default_node_services() {
    guarded(process("tcp", false)).await;
}
