use super::messaging::fixture;
use parrot_api::receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey};
use parrot_api::{
    address::{ActorRef, ActorRefExt},
    message::serde_remote_serialize,
    types::BoxedActorRef,
};
use parrot_app::host::dylib::{NativeMessageGrant, NativeMessageOperation, NativeMessagingPolicy};
use parrot_app::host::dylib::{NativeServiceGrant, NativeServiceOperation, NativeServicePolicy};

fn service_grant(name: &str, operation: NativeServiceOperation) -> NativeServiceGrant {
    NativeServiceGrant {
        source_path: format!("/user/{name}"),
        artifact_digest: digest(),
        key: "native/business".into(),
        operation,
    }
}
fn service_policy_text(grants: &[NativeServiceGrant]) -> String {
    toml::to_string(&toml::Table::from_iter([(
        "grants".into(),
        toml::Value::try_from(grants).unwrap(),
    )]))
    .unwrap()
}
async fn service_call(actor: &BoxedActorRef, key: &str, bytes: &[u8]) -> Vec<u8> {
    tokio::time::timeout(
        Duration::from_secs(10),
        actor.ask(DylibAsk {
            type_key: format!("service:{key}"),
            payload: bytes.into(),
        }),
    )
    .await
    .unwrap()
    .unwrap()
    .0
}
use parrot_node::{DylibAsk, HostOptions, NodeComponentExecutor};
use parrot_remote::{admin_v2::*, LocalLookup, RemoteActorSystem, RemoteConfig};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

const REQUEST: &str = "bin:deploy::NEcho#v1";
const RESPONSE: &str = "bin:deploy::NEchoed#v1";
fn digest() -> String {
    format!("{:x}", Sha256::digest(std::fs::read(fixture()).unwrap()))
}
fn grant(name: &str) -> NativeMessageGrant {
    NativeMessageGrant {
        source_path: format!("/user/{name}"),
        artifact_digest: digest(),
        destination: "/user/echo".into(),
        request_type_key: REQUEST.into(),
        operation: NativeMessageOperation::Ask {
            reply_type_key: RESPONSE.into(),
        },
    }
}
fn component(name: &str, policy: &str) -> ComponentDeploy {
    let (destination, request) = if name == "teller" {
        ("/user/counter", "bin:deploy::NInc#v1")
    } else {
        ("/user/echo", REQUEST)
    };
    let expected = if name == "replywrong" {
        "bin:deploy::NTotal#v1"
    } else {
        RESPONSE
    };
    let mut cfg = toml::Table::new();
    cfg.insert("destination".into(), destination.into());
    cfg.insert("request".into(), request.into());
    cfg.insert("expected".into(), expected.into());
    cfg.insert("native-messaging-policy".into(), policy.into());
    ComponentDeploy {
        name: name.into(),
        version: "1.0.0".into(),
        artifact: AdminArtifactRef::Dylib {
            digest: digest(),
            uri: url::Url::from_file_path(fixture()).unwrap().into(),
            abi: 1,
        },
        instances: AdminInstancePolicy::Singleton,
        config: Some(toml::to_string(&cfg).unwrap().into_bytes()),
    }
}
fn expected(value: u64) -> Vec<u8> {
    [
        0u32.to_le_bytes().as_slice(),
        &serde_remote_serialize(&parrot_node::NEchoed(value)).unwrap(),
    ]
    .concat()
}
async fn call(actor: &BoxedActorRef, key: &str, value: u64) -> Vec<u8> {
    let payload = if key == "tell" {
        serde_remote_serialize(&parrot_node::NInc(value)).unwrap()
    } else {
        serde_remote_serialize(&parrot_node::NEcho(value)).unwrap()
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        actor.ask(DylibAsk {
            type_key: key.into(),
            payload,
        }),
    )
    .await
    .unwrap()
    .unwrap()
    .0
}

#[tokio::test]
async fn native_node_owner_compares_authority_and_preserves_existing_deployment() {
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
            facade
        })
        .is_ok());
    parrot_node::spawn_builtin(&engine, "echo").await.unwrap();
    let one = grant("guest");
    let two = grant("sibling");
    // SAFETY: repository-built fixture and its dependencies satisfy the native ABI contract.
    let trust = unsafe { parrot_node::NativeTrust::authorize(&[digest()]) }.unwrap();
    let options = HostOptions {
        native: trust.clone(),
        native_messaging: NativeMessagingPolicy::new(vec![one.clone(), two.clone()]).unwrap(),
        native_services: NativeServicePolicy::new(vec![
            service_grant("guest", NativeServiceOperation::Register),
            service_grant("guest", NativeServiceOperation::Subscribe),
        ])
        .unwrap(),
        #[cfg(feature = "wasm")]
        wasm_messaging: Default::default(),
        #[cfg(feature = "wasm")]
        wasm_services: Default::default(),
    };
    let executor =
        NodeComponentExecutor::with_host_options(engine.clone(), None, &options).unwrap();
    assert!(matches!(
        executor.deploy(1, &component("guest", "ignored")).await,
        AdminReplyV2::Deployed { .. }
    ));
    let equal = HostOptions {
        native: trust.clone(),
        native_services: NativeServicePolicy::new(vec![
            service_grant("guest", NativeServiceOperation::Subscribe),
            NativeServiceGrant {
                artifact_digest: format!("sha256:{}", digest().to_uppercase()),
                ..service_grant("guest", NativeServiceOperation::Register)
            },
        ])
        .unwrap(),
        native_messaging: NativeMessagingPolicy::new(vec![
            two,
            NativeMessageGrant {
                artifact_digest: format!("sha256:{}", digest().to_ascii_uppercase()),
                ..one.clone()
            },
        ])
        .unwrap(),
        #[cfg(feature = "wasm")]
        wasm_messaging: Default::default(),
        #[cfg(feature = "wasm")]
        wasm_services: Default::default(),
    };
    NodeComponentExecutor::with_host_options(engine.clone(), None, &equal).unwrap();
    for changed in [
        HostOptions::default(),
        HostOptions {
            native_services: Default::default(),
            ..options.clone()
        },
        HostOptions {
            native: trust.clone(),
            ..Default::default()
        },
        HostOptions {
            native: trust.clone(),
            native_messaging: NativeMessagingPolicy::new(vec![NativeMessageGrant {
                operation: NativeMessageOperation::Ask {
                    reply_type_key: "bin:deploy::NTotal#v1".into(),
                },
                ..one
            }])
            .unwrap(),
            native_services: Default::default(),
            #[cfg(feature = "wasm")]
            wasm_messaging: Default::default(),
            #[cfg(feature = "wasm")]
            wasm_services: Default::default(),
        },
    ] {
        assert!(NodeComponentExecutor::with_host_options(engine.clone(), None, &changed).is_err());
    }
    assert!(NodeComponentExecutor::with_native_trust(engine.clone(), &trust).is_err());
    let actor = engine.resolve_actor("/user/guest").unwrap();
    assert_eq!(call(&actor, "ask", 9).await, expected(9));
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

async fn process(scheme: &str, authorize: bool, trust: bool) {
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
    let _server = credentials("native-policy-node.test");
    let operator = credentials("native-policy-operator.test");
    let service_policy_path = temp.path().join("services.toml");
    let service_grants = vec![
        service_grant("provider", NativeServiceOperation::Register),
        service_grant("consumer", NativeServiceOperation::Subscribe),
    ];
    std::fs::write(&service_policy_path, service_policy_text(&service_grants)).unwrap();
    let node_config = temp.path().join("node.toml");
    std::fs::write(&node_config, "[remote.receptionist]\ndefault_deny=false\n").unwrap();
    let policy_path = temp.path().join("operator.toml");
    let mut grants = vec![
        NativeMessageGrant {
            source_path: "/user/consumer".into(),
            artifact_digest: digest(),
            destination: "parrot://native-policy-node.test/user/provider".into(),
            request_type_key: "bin:parrot.dylib.Ask#v1".into(),
            operation: NativeMessageOperation::Ask {
                reply_type_key: "bin:parrot.dylib.Reply#v1".into(),
            },
        },
        grant("guest"),
        NativeMessageGrant {
            artifact_digest: "00".repeat(32),
            ..grant("wrongdigest")
        },
        NativeMessageGrant {
            operation: NativeMessageOperation::Ask {
                reply_type_key: "bin:deploy::NTotal#v1".into(),
            },
            ..grant("replywrong")
        },
        NativeMessageGrant {
            destination: "/user/counter".into(),
            request_type_key: "bin:deploy::NInc#v1".into(),
            operation: NativeMessageOperation::Tell,
            ..grant("teller")
        },
    ];
    #[derive(serde::Serialize)]
    struct Policy<'a> {
        grants: &'a [NativeMessageGrant],
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
        .env("PARROT_CONFIG", &node_config)
        .env("PARROT_NODE_ID", "native-policy-node.test")
        .env("PARROT_BIND", "127.0.0.1:0")
        .env("PARROT_SCHEME", scheme)
        .env("PARROT_ACTORS", "echo,counter")
        .env("PARROT_SECURITY_MODE", "mtls")
        .env("PARROT_ADMIN_NODES", "native-policy-operator.test")
        .env(
            "PARROT_TLS_CERT",
            temp.path().join("native-policy-node.test.crt"),
        )
        .env(
            "PARROT_TLS_KEY",
            temp.path().join("native-policy-node.test.key"),
        )
        .env("PARROT_TLS_CA", temp.path().join("ca.pem"));
    if trust {
        command.env("PARROT_TRUST_NATIVE_SHA256", digest());
    }
    if authorize {
        command.env("PARROT_NATIVE_MESSAGING_POLICY", &policy_path);
        command.env("PARROT_NATIVE_SERVICE_POLICY", &service_policy_path);
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
    let mut config = if scheme == "tcp" {
        RemoteConfig::tcp(
            "native-policy-operator.test",
            Some("127.0.0.1:0".parse().unwrap()),
        )
    } else {
        RemoteConfig::quic(
            "native-policy-operator.test",
            Some("127.0.0.1:0".parse().unwrap()),
        )
    };
    config.receptionist_acl = parrot_remote::acl::AclRules::permissive();
    let client = RemoteActorSystem::new(config.with_mtls(operator, []), Arc::new(Empty)).unwrap();
    client.start().await.unwrap();
    let service_key = ReceptionistKey::new("native/business").unwrap();
    let mut service_events = client
        .receptionist_gateway()
        .subscribe(service_key.clone())
        .await
        .unwrap();
    client
        .connect(&parrot_remote::NodeAddr {
            node_id: "native-policy-node.test".into(),
            identity_is_explicit: true,
            scheme: scheme.into(),
            addr: endpoint.parse().unwrap(),
        })
        .await
        .unwrap();
    let links = client.links_snapshot().await;
    assert_eq!(
        links[0].1.authentication(),
        parrot_remote::security::PeerAuthentication::MutualTls
    );
    assert_eq!(
        links[0].1.peer_capabilities() & parrot_remote::handshake::caps::DYLIB != 0,
        trust
    );
    grants.push(grant("sibling"));
    let widened = toml::to_string(&Policy { grants: &grants }).unwrap();
    std::fs::write(&policy_path, &widened).unwrap();
    let mut replies = Vec::new();
    for name in if trust {
        vec!["guest", "sibling", "wrongdigest", "replywrong"]
    } else {
        vec!["guest"]
    } {
        let deployed = client
            .deploy_component("native-policy-node.test", component(name, &widened))
            .await;
        if !trust {
            assert!(deployed
                .unwrap_err()
                .to_string()
                .contains("trust authorization"));
            continue;
        }
        assert_eq!(deployed.unwrap(), [format!("/user/{name}")]);
        let actor = Box::new(
            client
                .remote_ref(&format!("parrot://native-policy-node.test/user/{name}"))
                .unwrap(),
        ) as BoxedActorRef;
        replies.push((name, call(&actor, "ask", 17).await));
        client
            .stop_component("native-policy-node.test", &format!("/user/{name}"))
            .await
            .unwrap();
    }
    if trust {
        let mut widened_services = service_grants;
        widened_services.push(service_grant("sibling", NativeServiceOperation::Register));
        let injected = service_policy_text(&widened_services);
        std::fs::write(&service_policy_path, &injected).unwrap();
        for name in ["provider", "consumer", "sibling"] {
            let mut deploy = component(name, &widened);
            let mut cfg: toml::Table =
                toml::from_str(std::str::from_utf8(deploy.config.as_ref().unwrap()).unwrap())
                    .unwrap();
            cfg.insert("native-service-policy".into(), injected.clone().into());
            cfg.insert("request".into(), "bin:parrot.dylib.Ask#v1".into());
            cfg.insert("expected".into(), "bin:parrot.dylib.Reply#v1".into());
            deploy.config = Some(toml::to_string(&cfg).unwrap().into_bytes());
            client
                .deploy_component("native-policy-node.test", deploy)
                .await
                .unwrap();
        }
        let native = |name: &str| {
            Box::new(
                client
                    .remote_ref(&format!("parrot://native-policy-node.test/user/{name}"))
                    .unwrap(),
            ) as BoxedActorRef
        };
        assert_eq!(
            service_call(&native("sibling"), "register", b"").await,
            1u32.to_le_bytes()
        );
        assert_eq!(
            service_call(&native("consumer"), "register", b"").await,
            1u32.to_le_bytes()
        );
        assert_eq!(
            service_call(&native("provider"), "register", b"").await,
            (if authorize { 0u32 } else { 1u32 }).to_le_bytes()
        );
        if authorize {
            let path = "parrot://native-policy-node.test/user/provider";
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(10), service_events.recv())
                    .await
                    .unwrap()
                    .unwrap(),
                ReceptionistEvent::Registered {
                    key: service_key.clone(),
                    remote_path: path.into()
                }
            );
            let request = serde_remote_serialize(&DylibAsk {
                type_key: "echo".into(),
                payload: b"discovered-native-business".to_vec(),
            })
            .unwrap();
            let reply = service_call(&native("consumer"), "discover", &request).await;
            assert_eq!(&reply[..8], &[0u8; 8]);
            let business: parrot_node::DylibReply =
                parrot_api::message::serde_remote_deserialize(&reply[8..]).unwrap();
            assert_eq!(business.0, b"discovered-native-business");
            client
                .stop_component("native-policy-node.test", "/user/provider")
                .await
                .unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(10), service_events.recv())
                    .await
                    .unwrap()
                    .unwrap(),
                ReceptionistEvent::Unregistered {
                    key: service_key,
                    remote_path: path.into()
                }
            );
        } else {
            assert_eq!(
                service_call(&native("consumer"), "subscribe", b"").await,
                [1u32.to_le_bytes().as_slice(), &0u64.to_le_bytes()].concat()
            );
            client
                .stop_component("native-policy-node.test", "/user/provider")
                .await
                .unwrap();
        }
        for name in ["consumer", "sibling"] {
            client
                .stop_component("native-policy-node.test", &format!("/user/{name}"))
                .await
                .unwrap();
        }
        client
            .deploy_component("native-policy-node.test", component("teller", &widened))
            .await
            .unwrap();
        let actor = Box::new(
            client
                .remote_ref("parrot://native-policy-node.test/user/teller")
                .unwrap(),
        ) as BoxedActorRef;
        let tell = call(&actor, "tell", 7).await;
        assert_eq!(tell, (if authorize { 0u32 } else { 1u32 }).to_le_bytes());
        client
            .stop_component("native-policy-node.test", "/user/teller")
            .await
            .unwrap();
    }
    let counter = client
        .remote_ref("parrot://native-policy-node.test/user/counter")
        .unwrap();
    let total = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let reply = counter
                .send(Box::new(parrot_node::NGetTotal))
                .await
                .unwrap();
            let value = reply.downcast_ref::<parrot_node::NTotal>().unwrap().0;
            if !authorize || !trust || value == 7 {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
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
        assert_eq!(
            reply,
            if authorize && name == "guest" {
                expected(17)
            } else {
                (if authorize && name == "replywrong" {
                    7u32
                } else {
                    1u32
                })
                .to_le_bytes()
                .to_vec()
            },
            "{name}"
        );
    }
    assert_eq!(total, if authorize && trust { 7 } else { 0 });
}

#[tokio::test]
async fn mtls_tcp_admin_binds_native_policy_for_real_ask_and_tell() {
    process("tcp", true, true).await;
}
#[tokio::test]
async fn mtls_quic_admin_binds_native_policy_for_real_ask_and_tell() {
    process("quic", true, true).await;
}
#[tokio::test]
async fn remote_business_config_cannot_authorize_default_native_node() {
    process("tcp", false, true).await;
}
#[tokio::test]
async fn native_message_policy_does_not_grant_node_execution_trust() {
    process("tcp", true, false).await;
}
