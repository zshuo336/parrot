use super::*;
use parrot_api::message::serde_remote_serialize;
use parrot_app::host::wasm::{WasmMessageGrant, WasmMessageOperation};
use parrot_app::{host::HostLookup, manifest::AppManifest};
use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

#[derive(serde::Serialize)]
struct Policy<'a> {
    grants: &'a [WasmServiceGrant],
}
fn text(grants: &[WasmServiceGrant]) -> String {
    toml::to_string(&Policy { grants }).unwrap()
}

async fn execute(authorized: bool, replay: bool) {
    let temp = tempfile::tempdir().unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    let mut cfg = RemoteConfig::tcp("services-observer", Some("127.0.0.1:0".parse().unwrap()))
        .with_insecure_development();
    cfg.receptionist_acl = AclRules::permissive();
    let remote = RemoteActorSystem::new(cfg, Arc::new(HostLookup { ts: engine.clone() })).unwrap();
    remote.start().await.unwrap();
    let mut observed = remote
        .receptionist_gateway()
        .subscribe(key())
        .await
        .unwrap();
    let mut grants = all();
    grants.push(WasmServiceGrant {
        source_path: "/user/consumer".into(),
        ..grant(WasmServiceOperation::Subscribe)
    });
    grants.push(WasmServiceGrant {
        source_path: "/user/wrongdigest".into(),
        artifact_digest: "00".repeat(32),
        ..grant(WasmServiceOperation::Register)
    });
    let policy = temp.path().join("operator services.toml");
    std::fs::write(&policy, text(&grants)).unwrap();
    let messaging_policy = temp.path().join("operator messages.toml");
    #[derive(serde::Serialize)]
    struct MessagingPolicy {
        grants: Vec<WasmMessageGrant>,
    }
    let ArtifactRef::Wasm { digest, .. } = artifact() else {
        unreachable!()
    };
    std::fs::write(
        &messaging_policy,
        toml::to_string(&MessagingPolicy {
            grants: vec![WasmMessageGrant {
                source_path: "/user/consumer".into(),
                artifact_digest: digest,
                destination: "parrot://services-app/user/guest".into(),
                request_type_key: "bin:parrot.wasm.Ask#v1".into(),
                operation: WasmMessageOperation::Ask {
                    reply_type_key: "bin:parrot.wasm.Reply#v1".into(),
                },
            }],
        })
        .unwrap(),
    )
    .unwrap();
    let mut widened = grants.clone();
    widened.push(WasmServiceGrant {
        source_path: "/user/sibling".into(),
        ..grant(WasmServiceOperation::Register)
    });
    let injected = text(&widened);
    let mut app =
        AppManifest::from_toml("name='services-app'\nversion='1.0.0'\ncomponents=[]").unwrap();
    for name in ["guest", "consumer", "sibling", "wrongdigest"] {
        let mut component = AppManifest::from_toml(&format!("name='component'\nversion='1.0.0'\n[[components]]\nname='{name}'\nengine='parrot'\nartifact={{ Wasm={{digest='',uri=''}} }}")).unwrap().components.remove(0);
        component.artifact = artifact();
        component.config = Some(config(&[
            ("wasm-service-policy", policy.to_str().unwrap()),
            ("service-grants", &injected),
            ("key", KEY),
        ]));
        app.components.push(component);
    }
    app.config_overlay = Some(toml::from_str(&format!("[remote.node]\nnode_id='services-app'\nbind='127.0.0.1:0'\nseeds=['tcp://services-observer@{}']\n[remote.security]\nmode='insecure-development'\n[remote.receptionist]\ndefault_deny=false", remote.local_addr().unwrap())).unwrap());
    let manifest = temp.path().join("app.toml");
    std::fs::write(&manifest, toml::to_string(&app).unwrap()).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"));
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("PARROT_"))
    {
        command.env_remove(key);
    }
    command
        .current_dir(temp.path())
        .args([if replay { "replay" } else { "run" }, "--manifest"])
        .arg(manifest)
        .args(["--wasm-epoch-ticks", "10000"])
        .kill_on_drop(true);
    if authorized {
        command.arg("--wasm-service-policy").arg(&policy);
        command
            .arg("--wasm-messaging-policy")
            .arg(&messaging_policy);
    }
    if replay {
        use parrot_app::debug::{record_sink, FileSink, RecordSink};
        let input = temp.path().join("capture.jsonl");
        let report = temp.path().join("report.json");
        let mut sink = FileSink::open(&input).unwrap();
        for (n, operation) in ["register", "subscribe", "next"].iter().enumerate() {
            let request = serde_remote_serialize(&WasmAsk {
                type_key: (*operation).into(),
                payload: vec![],
            })
            .unwrap();
            sink.write(&record_sink(
                n as u64,
                17 + n as u64,
                "rx",
                parrot_remote::frame::frame_type::ASK,
                "/user/guest",
                "bin:parrot.wasm.Ask#v1",
                &request,
            ))
            .unwrap();
        }
        sink.finish().unwrap();
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            command
                .arg("--input")
                .arg(input)
                .arg("--report")
                .arg(&report)
                .arg("--allow-legacy-log")
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        remote.shutdown().await.unwrap();
        engine.shutdown_internal().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
        assert_eq!(report["status"], "succeeded");
        assert_eq!(report["delivered"], 3);
        for (n, payload) in [
            b"ok".to_vec(),
            1u64.to_le_bytes().to_vec(),
            b"registered:parrot://services-app/user/guest".to_vec(),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(report["replies"][n]["type_key"], "bin:parrot.wasm.Reply#v1");
            let hex: String = serde_remote_serialize(&WasmReply(payload))
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(report["replies"][n]["payload_hex"], hex);
        }
        return;
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap().take(65536));
    let endpoint = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let mut line = String::new();
            assert!(
                lines.read_line(&mut line).await.unwrap() > 0,
                "App exited before readiness"
            );
            if let Some(value) = line.strip_prefix("PARROT_APP_READY=") {
                break value.trim().to_string();
            }
        }
    })
    .await
    .unwrap();
    remote
        .connect(&NodeAddr::tcp("services-app", endpoint.parse().unwrap()))
        .await
        .unwrap();
    // Neither a later operator edit nor the already-injected complete document
    // can widen authority retained by the running process.
    std::fs::write(&policy, injected).unwrap();
    let actor = |name: &str| {
        Box::new(
            remote
                .remote_ref(&format!("parrot://services-app/user/{name}"))
                .unwrap(),
        ) as BoxedActorRef
    };
    for name in ["sibling", "wrongdigest"] {
        assert_eq!(call(&actor(name), "register", b"").await, b"error:Disabled");
    }
    let guest = actor("guest");
    let consumer = actor("consumer");
    if authorized {
        assert_eq!(call(&guest, "register", b"").await, b"ok");
        event(&mut observed, true, "parrot://services-app/user/guest").await;
        let request = serde_remote_serialize(&WasmAsk {
            type_key: "echo".into(),
            payload: b"guest-discovered-app-service".to_vec(),
        })
        .unwrap();
        let response = call(&consumer, "discover-ask", &request).await;
        assert_eq!(
            parrot_api::message::serde_remote_deserialize::<WasmReply>(&response)
                .unwrap()
                .0,
            b"guest-discovered-app-service"
        );
        let handle = call(&consumer, "subscribe", b"").await;
        assert_eq!(
            call(&consumer, "next", &handle).await,
            b"registered:parrot://services-app/user/guest"
        );
        let discovered = remote
            .remote_ref("parrot://services-app/user/guest")
            .unwrap();
        assert_eq!(
            call(
                &(Box::new(discovered) as BoxedActorRef),
                "echo",
                b"discovered-service"
            )
            .await,
            b"discovered-service"
        );
        assert_eq!(call(&guest, "deregister", b"").await, b"ok");
        event(&mut observed, false, "parrot://services-app/user/guest").await;
        assert_eq!(
            call(&consumer, "next", &handle).await,
            b"unregistered:parrot://services-app/user/guest"
        );
        assert_eq!(call(&consumer, "close", &handle).await, b"ok");
        assert_eq!(call(&guest, "register", b"").await, b"ok");
        event(&mut observed, true, "parrot://services-app/user/guest").await;
    } else {
        assert_eq!(call(&guest, "register", b"").await, b"error:Disabled");
        assert_eq!(call(&consumer, "subscribe", b"").await, b"error:Disabled");
        empty(&mut observed);
    }
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
    if authorized {
        event(&mut observed, false, "parrot://services-app/user/guest").await;
    }
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    assert!(tokio::net::TcpListener::bind(endpoint).await.is_ok());
}

#[tokio::test]
async fn stock_cli_service_policy_drives_real_network_discovery_and_retirement() {
    guarded(execute(true, false)).await;
}
#[tokio::test]
async fn manifest_service_policy_injection_cannot_authorize_stock_cli() {
    guarded(execute(false, false)).await;
}
#[tokio::test]
async fn stock_replay_service_policy_produces_actual_registration_and_snapshot_replies() {
    guarded(execute(true, true)).await;
}
