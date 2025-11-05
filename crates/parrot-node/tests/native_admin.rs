//! Real Admin deployment through authenticated TCP/QUIC to the stock executable.
#![cfg(feature = "dylib")]
#[path = "support/native.rs"]
mod native;
use parrot_api::{address::ActorRefExt, types::BoxedActorRef};
use parrot_node::{DylibAsk, DylibTell};
use parrot_remote::{
    admin_v2::{AdminArtifactRef, AdminInstancePolicy},
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "parrot-native-admin-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn ask(actor: &BoxedActorRef, key: &str, payload: &[u8]) -> Vec<u8> {
    actor
        .ask(DylibAsk {
            type_key: key.into(),
            payload: payload.into(),
        })
        .await
        .unwrap()
        .0
}

async fn authenticated_process(scheme: &str, authorize: bool) {
    let mut fixture = native::NativeFixture::new();
    let temp = Temp::new();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    let ca = params.self_signed(&key).unwrap().pem();
    let issuer = rcgen::Issuer::new(params, key);
    std::fs::write(temp.0.join("ca.pem"), &ca).unwrap();
    let credentials = |name: &str| {
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &issuer).unwrap().pem();
        std::fs::write(temp.0.join(format!("{name}.crt")), &cert).unwrap();
        std::fs::write(temp.0.join(format!("{name}.key")), key.serialize_pem()).unwrap();
        parrot_remote::tls::TlsConfig {
            cert_pem: cert.into_bytes(),
            key_pem: key.serialize_pem().into_bytes(),
            ca_pem: ca.as_bytes().into(),
        }
    };
    let _server = credentials("native-node.test");
    let operator = credentials("operator.test");
    let ordinary = credentials("ordinary.test");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-node"));
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("PARROT_"))
    {
        command.env_remove(key);
    }
    if authorize {
        command.env(
            "PARROT_TRUST_NATIVE_SHA256",
            format!("{},{}", fixture.digest, "0".repeat(64)),
        );
    }
    let mut child = command
        .current_dir(&temp.0)
        .env("PARROT_NODE_ID", "native-node.test")
        .env("PARROT_BIND", "127.0.0.1:0")
        .env("PARROT_ACTORS", "")
        .env("PARROT_SCHEME", scheme)
        .env("PARROT_SECURITY_MODE", "mtls")
        .env("PARROT_ADMIN_NODES", "operator.test")
        .env("PARROT_TLS_CERT", temp.0.join("native-node.test.crt"))
        .env("PARROT_TLS_KEY", temp.0.join("native-node.test.key"))
        .env("PARROT_TLS_CA", temp.0.join("ca.pem"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut reader = tokio::io::BufReader::new(child.stdout.take().unwrap().take(65536));
    let endpoint = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let mut line = String::new();
            assert!(
                reader.read_line(&mut line).await.unwrap() > 0,
                "node exited before readiness"
            );
            if let Some(address) = line.strip_prefix("PARROT_NODE_READY=") {
                break address.trim().to_string();
            }
        }
    })
    .await
    .unwrap();
    let config = |name| {
        if scheme == "tcp" {
            RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap()))
        } else {
            RemoteConfig::quic(name, Some("127.0.0.1:0".parse().unwrap()))
        }
    };
    let client = RemoteActorSystem::new(
        config("operator.test").with_mtls(operator, []),
        Arc::new(Empty),
    )
    .unwrap();
    let unauthorized = RemoteActorSystem::new(
        config("ordinary.test").with_mtls(ordinary, []),
        Arc::new(Empty),
    )
    .unwrap();
    client.start().await.unwrap();
    unauthorized.start().await.unwrap();
    let address = NodeAddr {
        node_id: "native-node.test".into(),
        identity_is_explicit: true,
        scheme: scheme.into(),
        addr: endpoint.parse().unwrap(),
    };
    for peer in [&client, &unauthorized] {
        tokio::time::timeout(Duration::from_secs(10), peer.connect(&address))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            peer.links_snapshot().await[0].1.authentication(),
            parrot_remote::security::PeerAuthentication::MutualTls
        );
    }
    let caps = client.links_snapshot().await[0].1.peer_capabilities();
    assert_eq!(caps & parrot_remote::handshake::caps::DYLIB != 0, authorize);
    let component = fixture.component("native-service-2026", 2);
    assert!(unauthorized
        .deploy_component("native-node.test", component.clone())
        .await
        .is_err());
    assert!(client
        .component_status("native-node.test", "/user/native-service-2026")
        .await
        .is_err());
    if !authorize {
        let error = client
            .deploy_component("native-node.test", component)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("trust authorization"), "{error}");
        assert!(fixture.events().is_empty());
    } else {
        let mut untrusted = component.clone();
        untrusted.name = "untrusted".into();
        if let AdminArtifactRef::Dylib { digest, .. } = &mut untrusted.artifact {
            *digest = "1".repeat(64);
        }
        untrusted.config = Some(format!("trust_native_sha256 = '{}'", "1".repeat(64)).into_bytes());
        let error = client
            .deploy_component("native-node.test", untrusted)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("trust authorization"), "{error}");
        assert!(fixture.events().is_empty());
        let mut bad_abi = component.clone();
        bad_abi.name = "bad-abi".into();
        if let AdminArtifactRef::Dylib { abi, .. } = &mut bad_abi.artifact {
            *abi = 2;
        }
        assert!(client
            .deploy_component("native-node.test", bad_abi)
            .await
            .unwrap_err()
            .to_string()
            .contains("ABI"));
        let mut bad = component.clone();
        bad.name = "bad-digest".into();
        if let AdminArtifactRef::Dylib { digest, .. } = &mut bad.artifact {
            *digest = "0".repeat(64);
        }
        let error = client
            .deploy_component("native-node.test", bad)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("digest"), "{error}");
        let paths = client
            .deploy_component("native-node.test", component.clone())
            .await
            .unwrap();
        assert_eq!(
            paths,
            ["/user/native-service-2026-0", "/user/native-service-2026-1"]
        );
        let actor: BoxedActorRef = Box::new(
            client
                .remote_ref(&format!("parrot://native-node.test{}", paths[0]))
                .unwrap(),
        );
        let raw = ask(&actor, "config", &[]).await;
        let envelope: toml::Table = toml::from_str(std::str::from_utf8(&raw).unwrap()).unwrap();
        assert_eq!(envelope["config"].as_table(), Some(&fixture.config));
        assert_eq!(
            envelope["self_path"].as_str(),
            Some("/user/native-service-2026-0")
        );
        assert_eq!(ask(&actor, "value", &[]).await, 0u64.to_le_bytes());
        actor
            .tell(DylibTell {
                type_key: "set".into(),
                payload: 99u64.to_le_bytes().to_vec(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while ask(&actor, "value", &[]).await != 99u64.to_le_bytes() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            client
                .deploy_component("native-node.test", component.clone())
                .await
                .unwrap(),
            paths
        );
        assert_eq!(ask(&actor, "value", &[]).await, 99u64.to_le_bytes());
        let states = client
            .component_status("native-node.test", "/user/native-service-2026")
            .await
            .unwrap();
        assert_eq!(states.len(), 2);
        assert!(states
            .iter()
            .all(|state| state.version == "2.3.4" && state.state == "running"));
        let mut neighbor = component.clone();
        neighbor.name = "native-service-2026-side".into();
        neighbor.instances = AdminInstancePolicy::Singleton;
        client
            .deploy_component("native-node.test", neighbor)
            .await
            .unwrap();
        assert_eq!(
            client
                .drain_component(
                    "native-node.test",
                    "/user/native-service-2026",
                    Duration::from_secs(5)
                )
                .await
                .unwrap(),
            (2, 0)
        );
        assert!(client
            .component_status("native-node.test", "/user/native-service-2026")
            .await
            .is_err());
        assert_eq!(
            client
                .component_status("native-node.test", "/user/native-service-2026-side")
                .await
                .unwrap()
                .len(),
            1
        );
        client
            .stop_component("native-node.test", "/user/native-service-2026-side")
            .await
            .unwrap();
        // A fresh generation can be deployed after the owned instances retire.
        client
            .deploy_component("native-node.test", component)
            .await
            .unwrap();
        assert_eq!(ask(&actor, "value", &[]).await, 0u64.to_le_bytes());
        client
            .stop_component("native-node.test", "/user/native-service-2026")
            .await
            .unwrap();
        // Cancelling the real RPC waiter does not revoke an accepted deployment.
        let gate = fixture.gate("construct_gate");
        let accepted = fixture.component("abandoned", 1);
        let caller = client.clone();
        let pending =
            tokio::spawn(
                async move { caller.deploy_component("native-node.test", accepted).await },
            );
        fixture.wait_for("construct-enter:/user/abandoned-0").await;
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        std::fs::write(gate, b"release").unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if client
                    .component_status("native-node.test", "/user/abandoned")
                    .await
                    .is_ok_and(|s| s.len() == 1 && s[0].state == "running")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        client
            .stop_component("native-node.test", "/user/abandoned")
            .await
            .unwrap();
        for path in ["/user/native-service-2026-0", "/user/native-service-2026-1"] {
            assert_eq!(
                fixture
                    .events()
                    .lines()
                    .filter(|l| *l == format!("destroy:{path}"))
                    .count(),
                2
            );
            assert_eq!(
                fixture
                    .events()
                    .lines()
                    .filter(|l| *l == format!("drain:{path}"))
                    .count(),
                2
            );
        }
        assert_eq!(
            fixture
                .events()
                .lines()
                .filter(|l| *l == "destroy:/user/abandoned-0")
                .count(),
            1
        );
        assert!(
            !fixture.events().contains("duplicate-drain")
                && !fixture.events().contains("destroy-without-drain")
        );
    }
    unauthorized.shutdown().await.unwrap();
    client.shutdown().await.unwrap();
    #[cfg(unix)]
    {
        assert!(tokio::process::Command::new("kill")
            .args(["-TERM", &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success());
        assert!(tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
    }
    #[cfg(not(unix))]
    {
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }
}

#[tokio::test]
async fn mtls_tcp_node_admin_deploys_real_native_and_keeps_accepted_operations_owned() {
    authenticated_process("tcp", true).await;
}
#[tokio::test]
async fn mtls_quic_node_admin_deploys_real_native_and_keeps_accepted_operations_owned() {
    authenticated_process("quic", true).await;
}

#[tokio::test]
async fn node_with_no_native_trust_does_not_advertise_or_execute_native_code() {
    authenticated_process("tcp", false).await;
}
