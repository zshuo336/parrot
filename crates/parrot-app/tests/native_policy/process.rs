use super::messaging::fixture;
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRefExt,
    message::serde_remote_serialize,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::host::dylib::{NativeServiceGrant, NativeServiceOperation};
use parrot_app::{
    host::{
        dylib::{DylibAsk, DylibReply, NativeMessageGrant, NativeMessageOperation},
        HostLookup,
    },
    manifest::AppManifest,
    ArtifactRef,
};
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

const REQUEST: &str = "bin:parrot.dylib.Ask#v1";
const RESPONSE: &str = "bin:parrot.dylib.Reply#v1";

struct Target(Arc<Mutex<Vec<u8>>>);
impl Actor for Target {
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
            let request = message.downcast::<DylibAsk>().unwrap();
            assert_eq!(request.type_key, "add-seven");
            assert_eq!(request.payload.len(), 1);
            let value = request.payload[0];
            self.0.lock().unwrap().push(value);
            Ok(Box::new(DylibReply(vec![value + 7])) as BoxedMessage)
        })
    }
}
fn expected(value: u8) -> Vec<u8> {
    [
        0u32.to_le_bytes().as_slice(),
        &serde_remote_serialize(&DylibReply(vec![value + 7])).unwrap(),
    ]
    .concat()
}
async fn execute(authorize: bool, replay: bool, trust: bool) {
    let digest = format!("{:x}", Sha256::digest(std::fs::read(fixture()).unwrap()));
    let temp = tempfile::tempdir().unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    let observed = Arc::new(Mutex::new(Vec::new()));
    engine
        .spawn_at(
            Target(observed.clone()),
            "/user/backend",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let mut remote_config =
        parrot_remote::RemoteConfig::tcp("native-backend", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development();
    remote_config.receptionist_acl = parrot_remote::acl::AclRules::permissive();
    let remote = parrot_remote::RemoteActorSystem::new(
        remote_config,
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    remote.start().await.unwrap();
    let service_key = ReceptionistKey::new("native/business").unwrap();
    let mut service_events = remote
        .receptionist_gateway()
        .subscribe(service_key.clone())
        .await
        .unwrap();
    let service_policy_path = temp.path().join("operator services.toml");
    let services = vec![
        NativeServiceGrant {
            source_path: "/user/guest".into(),
            artifact_digest: digest.clone(),
            key: "native/business".into(),
            operation: NativeServiceOperation::Register,
        },
        NativeServiceGrant {
            source_path: "/user/guest".into(),
            artifact_digest: digest.clone(),
            key: "native/business".into(),
            operation: NativeServiceOperation::Subscribe,
        },
        NativeServiceGrant {
            source_path: "/user/consumer".into(),
            artifact_digest: digest.clone(),
            key: "native/business".into(),
            operation: NativeServiceOperation::Subscribe,
        },
    ];
    let service_text = |grants: &[NativeServiceGrant]| {
        toml::to_string(&toml::Table::from_iter([(
            "grants".into(),
            toml::Value::try_from(grants).unwrap(),
        )]))
        .unwrap()
    };
    std::fs::write(&service_policy_path, service_text(&services)).unwrap();
    let destination = "parrot://native-backend/user/backend";
    let policy_path = temp.path().join("operator policy.toml");
    let grant = |name: &str, operation| NativeMessageGrant {
        source_path: format!("/user/{name}"),
        artifact_digest: digest.clone(),
        destination: destination.into(),
        request_type_key: REQUEST.into(),
        operation,
    };
    let ask = || NativeMessageOperation::Ask {
        reply_type_key: RESPONSE.into(),
    };
    let mut grants = vec![
        NativeMessageGrant {
            source_path: "/user/consumer".into(),
            artifact_digest: digest.clone(),
            destination: "parrot://native-policy-app/user/guest".into(),
            request_type_key: REQUEST.into(),
            operation: ask(),
        },
        grant("guest", ask()),
        grant("teller", NativeMessageOperation::Tell),
        NativeMessageGrant {
            artifact_digest: "00".repeat(32),
            ..grant("wrongdigest", ask())
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
    let mut app =
        AppManifest::from_toml("name='native-policy-app'\nversion='1.0.0'\ncomponents=[]").unwrap();
    for name in ["guest", "teller", "sibling", "wrongdigest", "consumer"] {
        let mut component = AppManifest::from_toml(&format!("name='component'\nversion='1.0.0'\n[[components]]\nname='{name}'\nengine='parrot'\nartifact={{Dylib={{digest='',uri='',abi=1}}}}")).unwrap().components.remove(0);
        component.artifact = ArtifactRef::Dylib {
            digest: digest.clone(),
            uri: url::Url::from_file_path(fixture()).unwrap().into(),
            abi: 1,
        };
        component.config = Some(toml::from_str(&format!(
            "destination='{destination}'\nrequest='{REQUEST}'\nexpected='{RESPONSE}'\nnative-service-policy={:?}\nnative-messaging-policy={:?}",
            service_policy_path.to_str().unwrap(),
            policy_path.to_str().unwrap())).unwrap());
        app.components.push(component);
    }
    app.config_overlay = Some(toml::from_str(&format!(
        "[remote.node]\nnode_id='native-policy-app'\nbind='127.0.0.1:0'\nseeds=['tcp://native-backend@{}']\n[remote.security]\nmode='insecure-development'\n[remote.receptionist]\ndefault_deny=false",
        remote.local_addr().unwrap())).unwrap());
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
        .arg(&manifest);
    if trust {
        command.arg("--trust-native-sha256").arg(&digest);
    }
    if authorize {
        command.arg("--native-messaging-policy").arg(&policy_path);
        command
            .arg("--native-service-policy")
            .arg(&service_policy_path);
    }
    let payload = serde_remote_serialize(&DylibAsk {
        type_key: "add-seven".into(),
        payload: vec![11],
    })
    .unwrap();
    if replay {
        use parrot_app::debug::{record_sink, FileSink, RecordSink};
        let input = temp.path().join("input.jsonl");
        let report = temp.path().join("report.json");
        let mut sink = FileSink::open(&input).unwrap();
        sink.write(&record_sink(
            0,
            17,
            "rx",
            parrot_remote::frame::frame_type::ASK,
            "/user/guest",
            REQUEST,
            &serde_remote_serialize(&DylibAsk {
                type_key: "ask".into(),
                payload,
            })
            .unwrap(),
        ))
        .unwrap();
        for (n, (operation, bytes)) in [
            ("service:register", vec![]),
            ("service:subscribe", vec![]),
            ("service:next", 1u64.to_le_bytes().to_vec()),
        ]
        .into_iter()
        .enumerate()
        {
            sink.write(&record_sink(
                1 + n as u64,
                18 + n as u64,
                "rx",
                parrot_remote::frame::frame_type::ASK,
                "/user/guest",
                REQUEST,
                &serde_remote_serialize(&DylibAsk {
                    type_key: operation.into(),
                    payload: bytes,
                })
                .unwrap(),
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
        assert_eq!(report["delivered"], 4);
        assert_eq!(report["replies"][0]["type_key"], RESPONSE);
        let hex: String = serde_remote_serialize(&DylibReply(expected(11)))
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(report["replies"][0]["payload_hex"], hex);
        for (n, bytes) in [
            0u32.to_le_bytes().to_vec(),
            [0u32.to_le_bytes().as_slice(), &1u64.to_le_bytes()].concat(),
            [
                0u32.to_le_bytes().as_slice(),
                &1u32.to_le_bytes(),
                b"parrot://native-policy-app/user/guest",
            ]
            .concat(),
        ]
        .into_iter()
        .enumerate()
        {
            let hex: String = serde_remote_serialize(&DylibReply(bytes))
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(report["replies"][n + 1]["type_key"], RESPONSE);
            assert_eq!(report["replies"][n + 1]["payload_hex"], hex);
        }
        assert_eq!(*observed.lock().unwrap(), [11]);
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
            if lines.read_line(&mut line).await.unwrap() == 0 {
                break None;
            }
            if let Some(value) = line.strip_prefix("PARROT_APP_READY=") {
                break Some(value.trim().to_string());
            }
        }
    })
    .await
    .unwrap();
    if !trust {
        if endpoint.is_some() {
            child.start_kill().unwrap();
        }
        let exit = tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .unwrap()
            .unwrap();
        remote.shutdown().await.unwrap();
        engine.shutdown_internal().await.unwrap();
        assert!(
            endpoint.is_none(),
            "message policy granted native execution"
        );
        assert!(!exit.success());
        assert!(observed.lock().unwrap().is_empty());
        return;
    }
    let endpoint = endpoint.expect("CLI readiness");
    remote
        .connect(&parrot_remote::NodeAddr::tcp(
            "native-policy-app",
            endpoint.parse().unwrap(),
        ))
        .await
        .unwrap();
    grants.push(grant("sibling", ask()));
    std::fs::write(
        &policy_path,
        toml::to_string(&Policy { grants: &grants }).unwrap(),
    )
    .unwrap();
    let actor = |name: &str| {
        Box::new(
            remote
                .remote_ref(&format!("parrot://native-policy-app/user/{name}"))
                .unwrap(),
        ) as BoxedActorRef
    };
    let mut widened_services = services;
    widened_services.push(NativeServiceGrant {
        source_path: "/user/sibling".into(),
        artifact_digest: digest.clone(),
        key: "native/business".into(),
        operation: NativeServiceOperation::Register,
    });
    std::fs::write(&service_policy_path, service_text(&widened_services)).unwrap();
    let service = |name: &str, operation: &str, bytes: Vec<u8>| {
        let actor = actor(name);
        let operation = operation.to_string();
        async move {
            actor
                .ask(DylibAsk {
                    type_key: format!("service:{operation}"),
                    payload: bytes,
                })
                .await
                .unwrap()
                .0
        }
    };
    assert_eq!(
        service("sibling", "register", vec![]).await,
        1u32.to_le_bytes()
    );
    assert_eq!(
        service("guest", "register", vec![]).await,
        (if authorize { 0u32 } else { 1u32 }).to_le_bytes()
    );
    if authorize {
        let path = "parrot://native-policy-app/user/guest";
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
            payload: b"native-app-discovered".to_vec(),
        })
        .unwrap();
        let response = service("consumer", "discover", request).await;
        assert_eq!(&response[..8], &[0u8; 8]);
        let reply: DylibReply =
            parrot_api::message::serde_remote_deserialize(&response[8..]).unwrap();
        assert_eq!(reply.0, b"native-app-discovered");
        assert_eq!(
            service("guest", "deregister", vec![]).await,
            0u32.to_le_bytes()
        );
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
            service("consumer", "subscribe", vec![]).await,
            [1u32.to_le_bytes().as_slice(), &0u64.to_le_bytes()].concat()
        );
    }
    let mut replies = Vec::new();
    for name in ["guest", "sibling", "wrongdigest"] {
        let reply = tokio::time::timeout(
            Duration::from_secs(10),
            actor(name).ask(DylibAsk {
                type_key: "ask".into(),
                payload: payload.clone(),
            }),
        )
        .await
        .unwrap()
        .unwrap()
        .0;
        replies.push((name, reply));
    }
    let tell = actor("teller")
        .ask(DylibAsk {
            type_key: "tell".into(),
            payload: serde_remote_serialize(&DylibAsk {
                type_key: "add-seven".into(),
                payload: vec![12],
            })
            .unwrap(),
        })
        .await
        .unwrap()
        .0;
    assert_eq!(tell, if authorize { 0u32 } else { 1u32 }.to_le_bytes());
    if authorize {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !observed.lock().unwrap().contains(&12) {
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
    assert!(tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    let released = tokio::net::TcpListener::bind(&endpoint).await.is_ok();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    assert!(released);
    for (name, reply) in replies {
        assert_eq!(
            reply,
            if authorize && name == "guest" {
                expected(11)
            } else {
                1u32.to_le_bytes().to_vec()
            },
            "{name}"
        );
    }
    assert_eq!(
        *observed.lock().unwrap(),
        if authorize { vec![11, 12] } else { vec![] }
    );
}

#[tokio::test]
async fn stock_cli_binds_native_policy_for_real_tcp_ask_and_tell() {
    execute(true, false, true).await;
}
#[tokio::test]
async fn business_policy_path_cannot_authorize_default_cli() {
    execute(false, false, true).await;
}
#[tokio::test]
async fn stock_replay_uses_native_policy_and_checks_actual_reply_and_effect() {
    execute(true, true, true).await;
}
#[tokio::test]
async fn message_policy_does_not_authorize_native_execution() {
    execute(true, false, false).await;
}
