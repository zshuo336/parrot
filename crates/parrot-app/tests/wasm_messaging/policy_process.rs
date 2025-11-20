use super::*;
use parrot_api::address::ActorRefExt;
use parrot_app::{host::HostLookup, manifest::AppManifest};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

const WIRE_ASK: &str = "bin:parrot.wasm.Ask#v1";
const WIRE_TELL: &str = "bin:parrot.wasm.Tell#v1";
const WIRE_REPLY: &str = "bin:parrot.wasm.Reply#v1";

struct WireTarget(Arc<Observed>);
impl Actor for WireTarget {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let payload = if let Some(ask) = message.downcast_ref::<WasmAsk>() {
                &ask.payload
            } else {
                &message
                    .downcast_ref::<parrot_app::host::wasm::WasmTell>()
                    .unwrap()
                    .payload
            };
            let value = u64::from(payload[0]);
            self.0.values.lock().unwrap().push(value);
            Ok(Box::new(WasmReply(vec![(value + 7) as u8])) as BoxedMessage)
        })
    }
}

async fn execute(with_policy: bool, replay: bool) {
    let temp = tempfile::tempdir().unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    let observed = Arc::new(Observed::default());
    engine
        .spawn_at(
            WireTarget(observed.clone()),
            "/user/native",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let remote = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("policy-native", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    remote.start().await.unwrap();
    let destination = "parrot://policy-native/user/native";
    let policy_path = temp.path().join("operator policy.toml");
    let mut grants = vec![];
    for (source, operation) in [
        (
            "guest",
            WasmMessageOperation::Ask {
                reply_type_key: WIRE_REPLY.into(),
            },
        ),
        ("teller", WasmMessageOperation::Tell),
    ] {
        grants.push(WasmMessageGrant {
            source_path: format!("/user/{source}"),
            destination: destination.into(),
            request_type_key: if source == "teller" {
                WIRE_TELL
            } else {
                WIRE_ASK
            }
            .into(),
            operation,
            ..ask_grant()
        });
    }
    grants.push(WasmMessageGrant {
        source_path: "/user/wrongdigest".into(),
        artifact_digest: "00".repeat(32),
        destination: destination.into(),
        request_type_key: WIRE_ASK.into(),
        ..ask_grant()
    });
    #[derive(serde::Serialize)]
    struct Policy {
        grants: Vec<WasmMessageGrant>,
    }
    std::fs::write(&policy_path, toml::to_string(&Policy { grants }).unwrap()).unwrap();
    let mut app =
        AppManifest::from_toml("name='policy-app'\nversion='1.0.0'\ncomponents=[]").unwrap();
    for name in ["guest", "teller", "sibling", "wrongdigest"] {
        let mut component = AppManifest::from_toml(&format!("name='component'\nversion='1.0.0'\n[[components]]\nname='{name}'\nengine='parrot'\nartifact={{ Wasm={{digest='',uri=''}} }}")).unwrap().components.remove(0);
        component.artifact = artifact();
        let mut settings = config(&[("destination", destination)]);
        settings.insert(
            "request-key".into(),
            if name == "teller" {
                WIRE_TELL
            } else {
                WIRE_ASK
            }
            .into(),
        );
        if name == "teller" {
            settings.insert("mode".into(), "tell".into());
        }
        settings.insert(
            "wasm-messaging-policy".into(),
            policy_path.to_str().unwrap().into(),
        );
        component.config = Some(settings);
        app.components.push(component);
    }
    app.config_overlay = Some(
        toml::from_str(&format!(
        "[remote.node]\nnode_id='policy-app'\nbind='127.0.0.1:0'\nseeds=['tcp://policy-native@{}']",
            remote.local_addr().unwrap()
        ))
        .unwrap(),
    );
    let manifest_path = temp.path().join("app.toml");
    std::fs::write(&manifest_path, toml::to_string(&app).unwrap()).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"));
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("PARROT_"))
    {
        command.env_remove(key);
    }
    command
        .current_dir(temp.path())
        .args([if replay { "replay" } else { "run" }, "--manifest"])
        .arg(&manifest_path)
        .args(["--wasm-epoch-ticks", "10000"]);
    if with_policy {
        command.arg("--wasm-messaging-policy").arg(&policy_path);
    }
    if replay {
        use parrot_app::debug::{record_sink, FileSink, RecordSink};
        let input = temp.path().join("legacy.jsonl");
        let report = temp.path().join("report.json");
        let mut sink = FileSink::open(&input).unwrap();
        let request = WasmAsk {
            type_key: "ask".into(),
            payload: serde_remote_serialize(&WasmAsk {
                type_key: "native-add-seven".into(),
                payload: vec![11],
            })
            .unwrap(),
        };
        sink.write(&record_sink(
            0,
            17,
            "rx",
            parrot_remote::frame::frame_type::ASK,
            "/user/guest",
            WIRE_ASK,
            &serde_remote_serialize(&request).unwrap(),
        ))
        .unwrap();
        sink.finish().unwrap();
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            command
                .arg("--input")
                .arg(input)
                .arg("--report")
                .arg(&report)
                .arg("--allow-legacy-log")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
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
        assert_eq!(report["delivered"], 1);
        assert_eq!(report["replies"][0]["type_key"], WIRE_REPLY);
        let expected = WasmReply(
            [
                WIRE_REPLY.as_bytes(),
                &[0],
                serde_remote_serialize(&WasmReply(vec![18]))
                    .unwrap()
                    .as_slice(),
            ]
            .concat(),
        );
        let hex: String = serde_remote_serialize(&expected)
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(report["replies"][0]["payload_hex"], hex);
        assert_eq!(*observed.values.lock().unwrap(), [11]);
        return;
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
                "CLI exited before readiness"
            );
            if let Some(value) = line.strip_prefix("PARROT_APP_READY=") {
                break value.trim().to_string();
            }
        }
    })
    .await
    .unwrap();
    remote
        .connect(&parrot_remote::NodeAddr::tcp(
            "policy-app",
            endpoint.parse().unwrap(),
        ))
        .await
        .unwrap();
    let ask = |name: &str| {
        Box::new(
            remote
                .remote_ref(&format!("parrot://policy-app/user/{name}"))
                .unwrap(),
        ) as BoxedActorRef
    };
    let payload = serde_remote_serialize(&WasmAsk {
        type_key: "native-add-seven".into(),
        payload: vec![11],
    })
    .unwrap();
    let mut replies = Vec::new();
    for name in ["guest", "sibling", "wrongdigest"] {
        let result = ask(name)
            .ask(WasmAsk {
                type_key: "ask".into(),
                payload: payload.clone(),
            })
            .await
            .unwrap()
            .0;
        replies.push((name, result));
    }
    let tell_result = ask("teller")
        .ask(WasmAsk {
            type_key: "ask".into(),
            payload: serde_remote_serialize(&parrot_app::host::wasm::WasmTell {
                type_key: "native-add-seven".into(),
                payload: vec![12],
            })
            .unwrap(),
        })
        .await
        .unwrap()
        .0;
    if with_policy && tell_result == b"admitted" {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !observed.values.lock().unwrap().contains(&12) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    assert!(tokio::process::Command::new("kill")
        .args(["-TERM", &child.id().unwrap().to_string()])
        .status()
        .await
        .unwrap()
        .success());
    let exit = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .unwrap()
        .unwrap();
    let listener_released = tokio::net::TcpListener::bind(&endpoint).await.is_ok();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    assert!(exit.success());
    assert!(listener_released);
    for (name, result) in replies {
        if with_policy && name == "guest" {
            assert_eq!(
                result,
                [
                    WIRE_REPLY.as_bytes(),
                    &[0],
                    serde_remote_serialize(&WasmReply(vec![18]))
                        .unwrap()
                        .as_slice()
                ]
                .concat()
            );
        } else {
            assert_eq!(result, b"error:Disabled", "{name}");
        }
    }
    if with_policy {
        assert_eq!(tell_result, b"admitted");
        assert_eq!(*observed.values.lock().unwrap(), [11, 12]);
    } else {
        assert_eq!(tell_result, b"error:Disabled");
        assert!(observed.values.lock().unwrap().is_empty());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stock_cli_applies_operator_policy_to_real_guest_tcp_ask_and_tell() {
    execute(true, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn manifest_business_policy_path_cannot_authorize_stock_cli() {
    execute(false, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn stock_replay_uses_operator_policy_for_actual_guest_side_effect_and_reply() {
    execute(true, true).await;
}
