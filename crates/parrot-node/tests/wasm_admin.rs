//! Real Admin deployment through authenticated TCP/QUIC to the stock executable.
#![cfg(feature = "wasm")]
use parrot_api::{address::ActorRefExt, types::BoxedActorRef};
use parrot_node::{WasmAsk, WasmTell};
use parrot_remote::{
    admin_v2::{AdminArtifactRef, AdminInstancePolicy, ComponentDeploy},
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
            "parrot-wasm-admin-{}-{}",
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
fn component() -> ComponentDeploy {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../parrot-wasm/tests/fixtures/fixture-component.wasm")
        .canonicalize()
        .unwrap();
    ComponentDeploy {
        name: "wasm-service-2026".into(),
        version: "2.3.4".into(),
        artifact: AdminArtifactRef::Wasm {
            digest: "sha256:6d3140b431d842807cb9e3e0ffe34d2e1e1138af946164f3cb350202ac00788e"
                .into(),
            uri: url::Url::from_file_path(path).unwrap().into(),
        },
        instances: AdminInstancePolicy::Pool { count: 2 },
        config: Some(
            "label = '远端配置'\n[nested]\nlimit = 7\n"
                .as_bytes()
                .into(),
        ),
    }
}
async fn ask(actor: &BoxedActorRef, key: &str, payload: &[u8]) -> Vec<u8> {
    actor
        .ask(WasmAsk {
            type_key: key.into(),
            payload: payload.into(),
        })
        .await
        .unwrap()
        .0
}

async fn authenticated_process(scheme: &str) {
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
    let _server = credentials("wasm-node.test");
    let operator = credentials("operator.test");
    let ordinary = credentials("ordinary.test");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-node"));
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("PARROT_"))
    {
        command.env_remove(key);
    }
    let mut child = command
        .current_dir(&temp.0)
        .env("PARROT_NODE_ID", "wasm-node.test")
        .env("PARROT_BIND", "127.0.0.1:0")
        .env("PARROT_ACTORS", "")
        .env("PARROT_SCHEME", scheme)
        .env("PARROT_SECURITY_MODE", "mtls")
        .env("PARROT_WASM_EPOCH_TICKS", "1000")
        .env("PARROT_ADMIN_NODES", "operator.test")
        .env("PARROT_TLS_CERT", temp.0.join("wasm-node.test.crt"))
        .env("PARROT_TLS_KEY", temp.0.join("wasm-node.test.key"))
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
        node_id: "wasm-node.test".into(),
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
    let component = component();
    assert!(unauthorized
        .deploy_component("wasm-node.test", component.clone())
        .await
        .is_err());
    assert!(client
        .component_status("wasm-node.test", "/user/wasm-service-2026")
        .await
        .is_err());
    let mut bad = component.clone();
    bad.name = "bad-digest".into();
    if let AdminArtifactRef::Wasm { digest, .. } = &mut bad.artifact {
        *digest = "0".repeat(64);
    }
    let error = client
        .deploy_component("wasm-node.test", bad)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("digest"), "{error}");
    let paths = client
        .deploy_component("wasm-node.test", component.clone())
        .await
        .unwrap();
    assert_eq!(
        paths,
        ["/user/wasm-service-2026-0", "/user/wasm-service-2026-1"]
    );
    let actor: BoxedActorRef = Box::new(
        client
            .remote_ref(&format!("parrot://wasm-node.test{}", paths[0]))
            .unwrap(),
    );
    assert_eq!(ask(&actor, "started", &[]).await, [1]);
    assert_eq!(ask(&actor, "config", b"label").await, "远端配置".as_bytes());
    let nested = String::from_utf8(ask(&actor, "config", b"nested").await).unwrap();
    let table: toml::Table = toml::from_str(&format!("value = {nested}")).unwrap();
    assert_eq!(table["value"]["limit"].as_integer(), Some(7));
    actor
        .tell(WasmTell {
            type_key: "bump".into(),
            payload: vec![9],
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while ask(&actor, "counter", &[]).await != 9u64.to_le_bytes() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        client
            .deploy_component("wasm-node.test", component.clone())
            .await
            .unwrap(),
        paths
    );
    assert_eq!(ask(&actor, "counter", &[]).await, 9u64.to_le_bytes());
    let states = client
        .component_status("wasm-node.test", "/user/wasm-service-2026")
        .await
        .unwrap();
    assert_eq!(states.len(), 2);
    assert!(states
        .iter()
        .all(|state| state.version == "2.3.4" && state.state == "running"));
    let mut neighbor = component.clone();
    neighbor.name = "wasm-service-2026-side".into();
    neighbor.instances = AdminInstancePolicy::Singleton;
    client
        .deploy_component("wasm-node.test", neighbor)
        .await
        .unwrap();
    assert_eq!(
        client
            .drain_component(
                "wasm-node.test",
                "/user/wasm-service-2026",
                Duration::from_secs(5)
            )
            .await
            .unwrap(),
        (2, 0)
    );
    assert!(client
        .component_status("wasm-node.test", "/user/wasm-service-2026")
        .await
        .is_err());
    assert_eq!(
        client
            .component_status("wasm-node.test", "/user/wasm-service-2026-side")
            .await
            .unwrap()
            .len(),
        1
    );
    client
        .stop_component("wasm-node.test", "/user/wasm-service-2026-side")
        .await
        .unwrap();
    // A fresh generation can be deployed after the owned instances retire.
    client
        .deploy_component("wasm-node.test", component)
        .await
        .unwrap();
    assert_eq!(ask(&actor, "counter", &[]).await, 0u64.to_le_bytes());
    client
        .stop_component("wasm-node.test", "/user/wasm-service-2026")
        .await
        .unwrap();
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
async fn mtls_tcp_node_admin_deploys_real_wasm_and_preserves_component_ownership() {
    authenticated_process("tcp").await;
}
#[tokio::test]
async fn mtls_quic_node_admin_deploys_real_wasm_and_preserves_component_ownership() {
    authenticated_process("quic").await;
}
