use parrot_api::{
    address::{ActorRef, ActorRefExt},
    message::serde_remote_serialize,
    types::BoxedActorRef,
};
use parrot_app::host::wasm::{WasmMessageGrant, WasmMessageOperation, WasmMessagingPolicy};
use parrot_node::{HostOptions, NodeComponentExecutor, WasmAsk};
use parrot_remote::{admin_v2::*, LocalLookup, RemoteActorSystem, RemoteConfig};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

const REQUEST: &str = "bin:deploy::NEcho#v1";
const RESPONSE: &str = "bin:deploy::NEchoed#v1";

fn digest() -> String {
    format!(
        "{:x}",
        Sha256::digest(include_bytes!(
            "../../../parrot-wasm/tests/fixtures/messaging-component.wasm"
        ))
    )
}
fn grant(name: &str) -> WasmMessageGrant {
    WasmMessageGrant {
        source_path: format!("/user/{name}"),
        artifact_digest: digest(),
        destination: "/user/echo".into(),
        request_type_key: REQUEST.into(),
        operation: WasmMessageOperation::Ask {
            reply_type_key: RESPONSE.into(),
        },
    }
}
fn component(name: &str, mode: &str, policy: &str) -> ComponentDeploy {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../parrot-wasm/tests/fixtures/messaging-component.wasm")
        .canonicalize()
        .unwrap();
    let mut cfg = toml::Table::new();
    cfg.insert(
        "destination".into(),
        if mode == "tell" {
            "/user/counter"
        } else {
            "/user/echo"
        }
        .into(),
    );
    cfg.insert(
        "request-key".into(),
        if mode == "tell" {
            "bin:deploy::NInc#v1"
        } else {
            REQUEST
        }
        .into(),
    );
    cfg.insert("mode".into(), mode.into());
    cfg.insert("wasm-messaging-policy".into(), policy.into());
    ComponentDeploy {
        name: name.into(),
        version: "1.0.0".into(),
        artifact: AdminArtifactRef::Wasm {
            digest: digest(),
            uri: url::Url::from_file_path(path).unwrap().into(),
        },
        instances: AdminInstancePolicy::Singleton,
        config: Some(toml::to_string(&cfg).unwrap().into_bytes()),
    }
}
fn expected(value: u64) -> Vec<u8> {
    [
        RESPONSE.as_bytes(),
        &[0],
        serde_remote_serialize(&parrot_node::NEchoed(value))
            .unwrap()
            .as_slice(),
    ]
    .concat()
}
async fn call(actor: &BoxedActorRef, value: u64) -> Vec<u8> {
    tokio::time::timeout(
        Duration::from_secs(10),
        actor.ask(WasmAsk {
            type_key: "ask".into(),
            payload: serde_remote_serialize(&parrot_node::NEcho(value)).unwrap(),
        }),
    )
    .await
    .unwrap()
    .unwrap()
    .0
}

#[tokio::test]
async fn node_owner_rejects_policy_changes_and_preserves_existing_deployment() {
    use parrot::system::ParrotActorSystem;
    let engine = parrot::thread::system::ThreadActorSystem::shared(Default::default());
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    facade
        .register_thread_system("eng".into(), engine.clone(), true)
        .await
        .unwrap();
    assert!(parrot_node::NODE
        .set(parrot_node::NodeState {
            ts: engine.clone(),
            facade: facade.clone()
        })
        .is_ok());
    parrot_node::spawn_builtin(&engine, "echo").await.unwrap();
    let one = grant("guest");
    let two = WasmMessageGrant {
        source_path: "/user/sibling".into(),
        ..grant("guest")
    };
    let options = HostOptions {
        wasm_messaging: WasmMessagingPolicy::new(vec![one.clone(), two.clone()]).unwrap(),
        wasm_services: Default::default(),
        #[cfg(feature = "dylib")]
        native: Default::default(),
        #[cfg(feature = "dylib")]
        native_messaging: Default::default(),
        #[cfg(feature = "dylib")]
        native_services: Default::default(),
    };
    let executor =
        NodeComponentExecutor::with_host_options(engine.clone(), Some(10000), &options).unwrap();
    assert!(matches!(
        executor
            .deploy(1, &component("guest", "ask", "ignored"))
            .await,
        AdminReplyV2::Deployed { .. }
    ));
    let equal = HostOptions {
        wasm_messaging: WasmMessagingPolicy::new(vec![
            two.clone(),
            WasmMessageGrant {
                artifact_digest: format!("sha256:{}", digest().to_ascii_uppercase()),
                ..one.clone()
            },
        ])
        .unwrap(),
        wasm_services: Default::default(),
        #[cfg(feature = "dylib")]
        native: Default::default(),
        #[cfg(feature = "dylib")]
        native_messaging: Default::default(),
        #[cfg(feature = "dylib")]
        native_services: Default::default(),
    };
    NodeComponentExecutor::with_host_options(engine.clone(), Some(10000), &equal).unwrap();
    for changed in [
        HostOptions::default(),
        HostOptions {
            wasm_messaging: WasmMessagingPolicy::new(vec![
                two,
                WasmMessageGrant {
                    operation: WasmMessageOperation::Ask {
                        reply_type_key: "bin:deploy::NTotal#v1".into(),
                    },
                    ..one
                },
            ])
            .unwrap(),
            wasm_services: Default::default(),
            #[cfg(feature = "dylib")]
            native: Default::default(),
            #[cfg(feature = "dylib")]
            native_messaging: Default::default(),
            #[cfg(feature = "dylib")]
            native_services: Default::default(),
        },
    ] {
        assert!(
            NodeComponentExecutor::with_host_options(engine.clone(), Some(10000), &changed)
                .is_err()
        );
    }
    let actor = engine.resolve_actor("/user/guest").unwrap();
    assert_eq!(call(&actor, 9).await, expected(9));
    let shared = NodeComponentExecutor::new(engine.clone());
    assert!(matches!(
        shared.stop(2, "/user/guest").await,
        AdminReplyV2::Stopped { .. }
    ));
    assert!(engine.resolve_actor("/user/guest").is_none());
    engine.shutdown_internal().await.unwrap();
}

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}

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
    let _server = credentials("policy-node.test");
    let operator = credentials("policy-operator.test");
    let policy_path = temp.path().join("operator.toml");
    let mut grants = vec![
        grant("guest"),
        WasmMessageGrant {
            source_path: "/user/wrongdigest".into(),
            artifact_digest: "00".repeat(32),
            ..grant("guest")
        },
        WasmMessageGrant {
            source_path: "/user/replywrong".into(),
            operation: WasmMessageOperation::Ask {
                reply_type_key: "bin:deploy::NTotal#v1".into(),
            },
            ..grant("guest")
        },
        WasmMessageGrant {
            source_path: "/user/teller".into(),
            destination: "/user/counter".into(),
            request_type_key: "bin:deploy::NInc#v1".into(),
            operation: WasmMessageOperation::Tell,
            ..grant("guest")
        },
    ];
    #[derive(serde::Serialize)]
    struct Policy<'a> {
        grants: &'a [WasmMessageGrant],
    }
    std::fs::write(
        &policy_path,
        toml::to_string(&Policy { grants: &grants }).unwrap(),
    )
    .unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-node"));
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("PARROT_"))
    {
        command.env_remove(key);
    }
    command
        .current_dir(temp.path())
        .env("PARROT_NODE_ID", "policy-node.test")
        .env("PARROT_BIND", "127.0.0.1:0")
        .env("PARROT_SCHEME", scheme)
        .env("PARROT_ACTORS", "echo,counter")
        .env("PARROT_SECURITY_MODE", "mtls")
        .env("PARROT_ADMIN_NODES", "policy-operator.test")
        .env("PARROT_WASM_EPOCH_TICKS", "10000")
        .env("PARROT_TLS_CERT", temp.path().join("policy-node.test.crt"))
        .env("PARROT_TLS_KEY", temp.path().join("policy-node.test.key"))
        .env("PARROT_TLS_CA", temp.path().join("ca.pem"));
    if authorized {
        command.env("PARROT_WASM_MESSAGING_POLICY", &policy_path);
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
                "node exited before readiness"
            );
            if let Some(value) = line.strip_prefix("PARROT_NODE_READY=") {
                break value.trim().to_string();
            }
        }
    })
    .await
    .unwrap();
    let config = if scheme == "tcp" {
        RemoteConfig::tcp("policy-operator.test", Some("127.0.0.1:0".parse().unwrap()))
    } else {
        RemoteConfig::quic("policy-operator.test", Some("127.0.0.1:0".parse().unwrap()))
    };
    let client = RemoteActorSystem::new(config.with_mtls(operator, []), Arc::new(Empty)).unwrap();
    client.start().await.unwrap();
    client
        .connect(&parrot_remote::NodeAddr {
            node_id: "policy-node.test".into(),
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
    // A later operator file edit cannot widen the policy already owned by the
    // process. Inject the widened document into remote business config too.
    grants.push(grant("sibling"));
    let policy_text = toml::to_string(&Policy { grants: &grants }).unwrap();
    std::fs::write(&policy_path, &policy_text).unwrap();
    let mut replies = Vec::new();
    for name in ["guest", "sibling", "wrongdigest", "replywrong"] {
        let paths = client
            .deploy_component("policy-node.test", component(name, "ask", &policy_text))
            .await
            .unwrap();
        assert_eq!(paths, [format!("/user/{name}")]);
        let actor = Box::new(
            client
                .remote_ref(&format!("parrot://policy-node.test/user/{name}"))
                .unwrap(),
        ) as BoxedActorRef;
        let reply = call(&actor, 17).await;
        replies.push((name, reply));
        client
            .stop_component("policy-node.test", &format!("/user/{name}"))
            .await
            .unwrap();
    }
    client
        .deploy_component(
            "policy-node.test",
            component("teller", "tell", &policy_text),
        )
        .await
        .unwrap();
    let teller = Box::new(
        client
            .remote_ref("parrot://policy-node.test/user/teller")
            .unwrap(),
    ) as BoxedActorRef;
    let result = call(&teller, 7).await;
    let counter = client
        .remote_ref("parrot://policy-node.test/user/counter")
        .unwrap();
    let total = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let reply = counter
                .send(Box::new(parrot_node::NGetTotal))
                .await
                .unwrap();
            let value = reply.downcast_ref::<parrot_node::NTotal>().unwrap().0;
            if result != b"admitted" || value == 7 {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    client
        .stop_component("policy-node.test", "/user/teller")
        .await
        .unwrap();
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
        assert!(tokio::net::TcpListener::bind(&endpoint).await.is_ok());
    } else {
        assert!(tokio::net::UdpSocket::bind(&endpoint).await.is_ok());
    }
    for (name, reply) in replies {
        if authorized && name == "guest" {
            assert_eq!(reply, expected(17));
        } else if authorized && name == "replywrong" {
            assert_eq!(reply, b"error:UnexpectedReply");
        } else {
            assert_eq!(reply, b"error:Disabled", "{name}");
        }
    }
    assert_eq!(
        result,
        if authorized {
            b"admitted".as_slice()
        } else {
            b"error:Disabled".as_slice()
        }
    );
    assert_eq!(total, if authorized { 7 } else { 0 });
}

#[cfg(unix)]
#[tokio::test]
async fn mtls_tcp_admin_uses_operator_policy_for_guest_ask_and_tell() {
    process("tcp", true).await;
}
#[cfg(unix)]
#[tokio::test]
async fn mtls_quic_admin_uses_operator_policy_for_guest_ask_and_tell() {
    process("quic", true).await;
}
#[cfg(unix)]
#[tokio::test]
async fn remote_business_configuration_cannot_authorize_default_node() {
    process("tcp", false).await;
}
