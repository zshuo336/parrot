#![cfg(feature = "wasm")]

use parrot::thread::{ThreadActorConfig, ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    message::{serde_remote_deserialize, serde_remote_serialize},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{ParrotAppHook, ParrotSpawner},
    host::{
        wasm::{WasmAsk, WasmMessageGrant, WasmMessageOperation, WasmReply},
        ThreadPropsSpawner,
    },
    manifest::ArtifactRef,
};
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const REQUEST: &str = "bin:wasm.messaging.Request#v1";
const RESPONSE: &str = "bin:wasm.messaging.Response#v1";
const LARGE: &str = "bin:wasm.messaging.Large#v1";
const HOOK_ALIAS: &str = "bin:wasm.messaging.HookAlias#v1";

#[path = "wasm_messaging/initialization.rs"]
mod initialization;

#[cfg(unix)]
#[path = "wasm_messaging/policy_process.rs"]
mod policy_process;

#[derive(serde::Serialize, serde::Deserialize)]
struct Request(u64);
#[derive(serde::Serialize, serde::Deserialize)]
struct Response(u64);
#[derive(serde::Serialize, serde::Deserialize)]
struct Large(Vec<u8>);
macro_rules! wire {
    ($ty:ty, $key:expr) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key, type_id: std::any::TypeId::of::<$ty>(),
                encode: |message| serde_remote_serialize(message.downcast_ref::<$ty>().ok_or("wrong wire type")?),
                decode: |payload| {
                    Ok(Box::new(serde_remote_deserialize::<$ty>(payload)?) as BoxedMessage)
                },
            }
        }
    };
}
wire!(Request, REQUEST);
wire!(Response, RESPONSE);
wire!(Large, LARGE);
struct HookAlias;
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: HOOK_ALIAS, type_id: std::any::TypeId::of::<HookAlias>(),
        encode: |_| Err("alias is inbound only".into()),
        decode: |_| Ok(Box::new(ParrotAppHook(parrot_app::hook_phase::STOP)) as BoxedMessage),
    }
}

#[derive(Default)]
struct Observed {
    values: Mutex<Vec<u64>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
struct Native(Arc<Observed>);
impl Actor for Native {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = message.downcast_ref::<Request>().unwrap().0;
            self.0.values.lock().unwrap().push(value);
            if value == 42 {
                self.0.entered.notify_one();
                self.0.release.notified().await;
            }
            if value == 251 {
                return Err(ActorError::MessageHandlingError(
                    "actual native failure".into(),
                ));
            }
            if value == 250 {
                return Ok(Box::new(Large(vec![7; 1024 * 1024 + 1])) as BoxedMessage);
            }
            Ok(Box::new(Response(value + 7)) as BoxedMessage)
        })
    }
}

fn artifact() -> ArtifactRef {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../parrot-wasm/tests/fixtures/messaging-component.wasm")
        .canonicalize()
        .unwrap();
    ArtifactRef::Wasm {
        digest: format!(
            "sha256:{:x}",
            Sha256::digest(include_bytes!(
                "../../parrot-wasm/tests/fixtures/messaging-component.wasm"
            ))
        ),
        uri: url::Url::from_file_path(path).unwrap().into(),
    }
}
fn grant(operation: WasmMessageOperation) -> WasmMessageGrant {
    let ArtifactRef::Wasm { digest, .. } = artifact() else {
        unreachable!()
    };
    WasmMessageGrant {
        source_path: "/user/guest".into(),
        artifact_digest: digest,
        destination: "/user/native".into(),
        request_type_key: REQUEST.into(),
        operation,
    }
}
fn ask_grant() -> WasmMessageGrant {
    grant(WasmMessageOperation::Ask {
        reply_type_key: RESPONSE.into(),
    })
}
fn config(values: &[(&str, &str)]) -> toml::Table {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), toml::Value::String((*value).into())))
        .collect()
}
struct Harness {
    engine: Arc<ThreadActorSystem>,
    guest: BoxedActorRef,
    observed: Arc<Observed>,
}
impl Harness {
    async fn new(grants: Vec<WasmMessageGrant>, config: toml::Table) -> Self {
        Self::with_system_config(grants, config, Default::default()).await
    }
    async fn with_system_config(
        grants: Vec<WasmMessageGrant>,
        config: toml::Table,
        system_config: parrot::thread::ThreadActorSystemConfig,
    ) -> Self {
        let engine = ThreadActorSystem::shared(system_config);
        let observed = Arc::new(Observed::default());
        engine
            .spawn_at(
                Native(observed.clone()),
                "/user/native",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        let spawner = ThreadPropsSpawner::new(engine.clone())
            .with_wasm_messaging(grants)
            .unwrap()
            .with_wasm_limits(
                parrot_wasm::WasmConfig {
                    fuel_per_message: 20_000_000,
                    epoch_deadline: 10_000,
                    ..Default::default()
                },
                Default::default(),
            );
        let mut actors = spawner
            .spawn_artifact(&artifact(), &["/user/guest".into()], Some(&config))
            .await
            .unwrap();
        Self {
            engine,
            guest: actors.remove(0),
            observed,
        }
    }
    async fn call(&self, key: &str, bytes: &[u8]) -> Vec<u8> {
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            self.guest.send(Box::new(WasmAsk {
                type_key: key.into(),
                payload: bytes.into(),
            })),
        )
        .await
        .expect("guest response deadline")
        .unwrap();
        result.downcast::<WasmReply>().unwrap().0
    }
    async fn value(&self, value: u64) -> Vec<u8> {
        self.call("ask", &serde_remote_serialize(&Request(value)).unwrap())
            .await
    }
    async fn stop(self) {
        self.observed.release.notify_one();
        self.engine.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn actual_recovery_startup_messages_inherit_the_recovery_lifecycle_and_recorder() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let harness = Harness::with_system_config(
            vec![grant(WasmMessageOperation::Tell)],
            config(&[("start-value", "3")]),
            parrot::thread::ThreadActorSystemConfig {
                default_supervisor_strategy: parrot::thread::SupervisorStrategy::Resume,
                ..Default::default()
            },
        )
        .await;
        assert!(harness
            .guest
            .send(Box::new(WasmAsk {
                type_key: "trap".into(),
                payload: vec![]
            }))
            .await
            .is_err());
        let recorder = parrot_api::trace::TraceRecorder::new(32).unwrap();
        let root = recorder.root();
        assert!(root.scope(harness.call("echo", b"")).await.is_empty());
        // The real native mailbox processes this barrier after both startup
        // TELLs, so exported records cannot race the observation.
        harness
            .engine
            .resolve_actor("/user/native")
            .unwrap()
            .send(Box::new(Request(9)))
            .await
            .unwrap();
        recorder.close();
        let snapshot = recorder.snapshot();
        let host = snapshot
            .records
            .iter()
            .find(|record| record.target == "/user/guest")
            .unwrap();
        let recovery = snapshot
            .records
            .iter()
            .find(|record| record.kind == parrot_api::trace::SpanKind::WasmLifecycle)
            .unwrap();
        let native = snapshot
            .records
            .iter()
            .find(|record| record.target == "/user/native")
            .unwrap();
        let startup = snapshot
            .records
            .iter()
            .find(|record| record.target == "guest/start-await")
            .unwrap();
        assert_eq!(host.parent_span_id, Some(root.span_id()));
        assert_eq!(recovery.parent_span_id, Some(host.span_id));
        assert_eq!(recovery.target, "wasm/recovery-start:/user/guest");
        assert_eq!(native.parent_span_id, Some(recovery.span_id));
        assert_eq!(startup.parent_span_id, Some(recovery.span_id));
        for record in [host, recovery, native, startup] {
            assert_eq!(record.trace_id, root.trace_id());
        }
        assert_eq!(native.status, parrot_api::trace::SpanStatus::Ok);
        assert_eq!(recovery.status, parrot_api::trace::SpanStatus::Ok);
        assert_eq!(startup.status, parrot_api::trace::SpanStatus::Cancelled);
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 4);
        assert_eq!(snapshot.counts.overwritten, 0);
        assert_eq!(*harness.observed.values.lock().unwrap(), [3, 3, 9]);
        harness.stop().await;
    })
    .await
    .expect("recovery messaging and retirement deadline");
}
fn response(value: u64) -> Vec<u8> {
    [
        RESPONSE.as_bytes(),
        &[0],
        serde_remote_serialize(&Response(value)).unwrap().as_slice(),
    ]
    .concat()
}

#[tokio::test]
async fn real_guest_asks_real_native_actor_and_preserves_reply_codec_and_trace() {
    let harness = Harness::new(vec![ask_grant()], Default::default()).await;
    let recorder = parrot_api::trace::TraceRecorder::new(32).unwrap();
    let root = recorder.root();
    assert_eq!(root.scope(harness.value(11)).await, response(18));
    assert_eq!(*harness.observed.values.lock().unwrap(), [11]);
    let snapshot = recorder.snapshot();
    let guest = snapshot
        .records
        .iter()
        .find(|span| span.kind == parrot_api::trace::SpanKind::WasmGuest)
        .unwrap();
    let native = snapshot
        .records
        .iter()
        .find(|span| span.target == "/user/native")
        .unwrap();
    let host = snapshot
        .records
        .iter()
        .find(|span| span.target == "/user/guest")
        .unwrap();
    assert_eq!(guest.parent_span_id, Some(host.span_id));
    assert_eq!(native.parent_span_id, Some(host.span_id));
    assert_eq!(native.trace_id, root.trace_id());
    assert_eq!(native.status, parrot_api::trace::SpanStatus::Ok);
    assert!(harness.call("parent", b"").await.is_empty());
    harness.stop().await;
}

#[tokio::test]
async fn suspended_native_reply_allows_unrelated_actor_progress() {
    let harness = Harness::new(vec![ask_grant()], Default::default()).await;
    harness
        .engine
        .spawn_at(
            Native(Arc::new(Observed::default())),
            "/user/unrelated",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let mut call = Box::pin(harness.value(42));
    tokio::select! {
        result = &mut call => panic!("guest did not suspend: {result:?}"),
        () = harness.observed.entered.notified() => {}
    }
    let unrelated = harness.engine.resolve_actor("/user/unrelated").unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), unrelated.send(Box::new(Request(3))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.downcast_ref::<Response>().unwrap().0, 10);
    harness.observed.release.notify_one();
    assert_eq!(call.await, response(49));
    harness.stop().await;
}

#[tokio::test]
async fn default_deny_and_source_and_digest_binding_do_not_reach_native_actor() {
    for grants in [
        vec![],
        vec![WasmMessageGrant {
            source_path: "/user/sibling".into(),
            ..ask_grant()
        }],
        vec![WasmMessageGrant {
            artifact_digest: "00".repeat(32),
            ..ask_grant()
        }],
    ] {
        let harness = Harness::new(grants, Default::default()).await;
        assert_eq!(harness.value(1).await, b"error:Disabled");
        assert!(harness.observed.values.lock().unwrap().is_empty());
        harness.stop().await;
    }
}

#[tokio::test]
async fn denied_targets_types_and_operations_are_rejected_before_decode() {
    let harness = Harness::new(vec![ask_grant()], config(&[("mode", "tell")])).await;
    // Use malformed bytes, so a decoder-first implementation returns Codec.
    for key in ["ask", "denied-target", "denied-type", "self"] {
        assert_eq!(harness.call(key, &[255]).await, b"error:Denied", "{key}");
    }
    assert!(harness.observed.values.lock().unwrap().is_empty());
    harness.stop().await;
    let harness = Harness::new(vec![grant(WasmMessageOperation::Tell)], Default::default()).await;
    assert_eq!(harness.value(2).await, b"error:Denied");
    harness.stop().await;
}

#[tokio::test]
async fn malformed_request_size_timeout_and_call_quota_do_not_dispatch() {
    let harness = Harness::new(vec![ask_grant()], Default::default()).await;
    for key in ["zero-timeout", "big-timeout", "long-path", "long-key"] {
        assert_eq!(
            harness.call(key, b"").await,
            b"error:InvalidRequest",
            "{key}"
        );
    }
    for key in ["oversize", "quota"] {
        assert_eq!(harness.call(key, b"").await, b"error:Limit", "{key}");
    }
    assert!(harness.observed.values.lock().unwrap().is_empty());
    // A new invocation owns a fresh quota, even after 64 denied attempts.
    assert_eq!(harness.value(4).await, response(11));
    harness.stop().await;
}

#[tokio::test]
async fn authorized_unknown_target_unknown_codec_and_bad_payload_have_distinct_errors() {
    for (grant, settings, error) in [
        (
            WasmMessageGrant {
                destination: "/user/missing".into(),
                ..ask_grant()
            },
            config(&[("destination", "/user/missing")]),
            "error:UnknownTarget",
        ),
        (
            WasmMessageGrant {
                request_type_key: "bin:unknown.Request#v1".into(),
                ..ask_grant()
            },
            config(&[("request-key", "bin:unknown.Request#v1")]),
            "error:UnknownType",
        ),
        (ask_grant(), Default::default(), "error:Codec"),
    ] {
        let harness = Harness::new(vec![grant], settings).await;
        let bytes = if error == "error:Codec" {
            vec![255]
        } else {
            serde_remote_serialize(&Request(8)).unwrap()
        };
        assert_eq!(harness.call("ask", &bytes).await, error.as_bytes());
        assert!(harness.observed.values.lock().unwrap().is_empty());
        harness.stop().await;
    }
}

#[tokio::test]
async fn unexpected_reply_and_oversized_reply_are_rejected() {
    let harness = Harness::new(
        vec![grant(WasmMessageOperation::Ask {
            reply_type_key: LARGE.into(),
        })],
        Default::default(),
    )
    .await;
    assert_eq!(harness.value(1).await, b"error:UnexpectedReply");
    assert_eq!(harness.value(250).await, b"error:Limit");
    assert_eq!(*harness.observed.values.lock().unwrap(), [1, 250]);
    harness.stop().await;
}

#[tokio::test]
async fn actual_actor_failure_reaches_guest_as_failed() {
    let harness = Harness::new(vec![ask_grant()], Default::default()).await;
    assert_eq!(harness.value(251).await, b"error:Failed");
    assert_eq!(*harness.observed.values.lock().unwrap(), [251]);
    harness.stop().await;
}

#[tokio::test]
async fn timeout_releases_guest_wait_without_claiming_actor_rollback() {
    let harness = Harness::new(vec![ask_grant()], config(&[("timeout-ms", "100")])).await;
    assert_eq!(harness.value(42).await, b"error:Timeout");
    assert_eq!(*harness.observed.values.lock().unwrap(), [42]);
    harness.observed.release.notify_one();
    assert_eq!(harness.value(2).await, response(9));
    // The handled timeout is a guest result, not a poisoned Store.
    assert_eq!(harness.call("count", b"").await, 2u64.to_le_bytes());
    harness.stop().await;
}

#[tokio::test]
async fn tell_acknowledges_admission_while_native_handler_is_still_waiting() {
    let harness = Harness::new(
        vec![grant(WasmMessageOperation::Tell)],
        config(&[("mode", "tell")]),
    )
    .await;
    assert_eq!(harness.value(42).await, b"admitted");
    tokio::time::timeout(Duration::from_secs(5), harness.observed.entered.notified())
        .await
        .unwrap();
    // A second guest invocation completes while the target is still blocked.
    assert_eq!(harness.call("count", b"").await, 1u64.to_le_bytes());
    harness.observed.release.notify_one();
    harness.stop().await;
}

#[tokio::test]
async fn lifecycle_guest_calls_are_real_and_drain_is_idempotent() {
    let harness = Harness::new(
        vec![grant(WasmMessageOperation::Tell)],
        config(&[("start-value", "3"), ("drain-value", "5")]),
    )
    .await;
    assert_eq!(harness.call("started", b"").await, [1]);
    for phase in [
        parrot_app::hook_phase::DRAIN,
        parrot_app::hook_phase::DRAIN,
        parrot_app::hook_phase::STOP,
    ] {
        harness
            .guest
            .send(Box::new(ParrotAppHook(phase)))
            .await
            .unwrap();
    }
    // A FIFO query to the native target witnesses all prior admissions.
    let target = harness.engine.resolve_actor("/user/native").unwrap();
    target.send(Box::new(Request(9))).await.unwrap();
    assert_eq!(*harness.observed.values.lock().unwrap(), [3, 5, 9]);
    assert!(harness
        .guest
        .send(Box::new(WasmAsk {
            type_key: "echo".into(),
            payload: vec![]
        }))
        .await
        .is_err());
    harness.stop().await;
}

#[tokio::test]
async fn grants_validate_before_deployment_and_clones_keep_default_deny() {
    let engine = ThreadActorSystem::shared(Default::default());
    // This checks authority cloning, independently of the epoch interruption contract.
    let original = ThreadPropsSpawner::new(engine.clone()).with_wasm_limits(
        parrot_wasm::WasmConfig {
            epoch_deadline: 1000,
            ..Default::default()
        },
        Default::default(),
    );
    for invalid in [
        WasmMessageGrant {
            artifact_digest: "broken".into(),
            ..ask_grant()
        },
        WasmMessageGrant {
            destination: "/user/guest".into(),
            ..ask_grant()
        },
        WasmMessageGrant {
            request_type_key: "json:x".into(),
            ..ask_grant()
        },
    ] {
        assert!(original.clone().with_wasm_messaging(vec![invalid]).is_err());
    }
    assert!(original
        .clone()
        .with_wasm_messaging(vec![ask_grant(); 257])
        .is_err());
    assert!(original
        .clone()
        .with_wasm_messaging(vec![
            ask_grant(),
            grant(WasmMessageOperation::Ask {
                reply_type_key: LARGE.into()
            })
        ])
        .is_err());
    let _authorized = original
        .clone()
        .with_wasm_messaging(vec![ask_grant()])
        .unwrap();
    let guest = original
        .spawn_artifact(&artifact(), &["/user/guest".into()], None)
        .await
        .unwrap()
        .remove(0);
    let response = guest
        .send(Box::new(WasmAsk {
            type_key: "ask".into(),
            payload: vec![1],
        }))
        .await
        .unwrap();
    assert_eq!(
        response.downcast_ref::<WasmReply>().unwrap().0,
        b"error:Disabled"
    );
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn full_manifest_plan_assembly_and_teardown_use_bound_guest_authority() {
    use parrot_app::{
        assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
        manifest::{AppManifest, ComponentSpec},
        planner::{plan, LocalTopology},
    };
    struct NoGateway;
    #[async_trait::async_trait]
    impl GatewayFactory for NoGateway {
        async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
            Ok(())
        }
        async fn start_gateway(
            &self,
            _: &ComponentSpec,
            _: &[String],
            _: &str,
        ) -> Result<Vec<BoxedActorRef>, DeployError> {
            panic!("local guest must not start a gateway")
        }
        async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
            panic!("unexpected gateway stop")
        }
    }
    let mut app = AppManifest::from_toml(
        r#"
name = "messaging-app"
version = "1.0.0"
[[components]]
name = "guest"
engine = "parrot"
artifact = { Wasm = { digest = "", uri = "" } }
[components.hooks]
on_start = "."
on_drain = "."
on_stop = "."
"#,
    )
    .unwrap();
    app.components[0].artifact = artifact();
    let engine = ThreadActorSystem::shared(Default::default());
    let observed = Arc::new(Observed::default());
    engine
        .spawn_at(
            Native(observed.clone()),
            "/user/native",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let spawner = ThreadPropsSpawner::new(engine.clone())
        .with_wasm_messaging(vec![ask_grant()])
        .unwrap()
        .with_wasm_limits(
            parrot_wasm::WasmConfig {
                epoch_deadline: 10_000,
                ..Default::default()
            },
            Default::default(),
        );
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&app, &LocalTopology).unwrap(),
            &deployer,
            &app,
            &Default::default(),
        )
        .await
        .unwrap();
    let guest = context.component_ref("guest").unwrap();
    let reply = guest
        .send(Box::new(WasmAsk {
            type_key: "ask".into(),
            payload: serde_remote_serialize(&Request(17)).unwrap(),
        }))
        .await
        .unwrap();
    assert_eq!(reply.downcast_ref::<WasmReply>().unwrap().0, response(24));
    assert_eq!(*observed.values.lock().unwrap(), [17]);
    context.drain(&deployer).await.unwrap();
    context.teardown(&deployer).await.unwrap();
    assert!(engine.get_actor_ref("/user/guest").is_none());
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn timeout_covers_waiting_for_full_mailbox_admission() {
    for mode in ["ask", "tell"] {
        full_mailbox_timeout(mode).await;
    }
}

async fn full_mailbox_timeout(mode: &str) {
    use parrot::thread::config::BackpressureStrategy;
    let engine = ThreadActorSystem::shared(Default::default());
    let observed = Arc::new(Observed::default());
    let native = engine
        .spawn_at(
            Native(observed.clone()),
            "/user/native",
            None,
            ThreadActorConfig {
                mailbox_capacity: Some(1),
                backpressure_strategy: Some(BackpressureStrategy::Block),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    native.deliver(Box::new(Request(42))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), observed.entered.notified())
        .await
        .unwrap();
    native.deliver(Box::new(Request(6))).await.unwrap();
    let spawner = ThreadPropsSpawner::new(engine.clone())
        .with_wasm_messaging(vec![ask_grant(), grant(WasmMessageOperation::Tell)])
        .unwrap()
        .with_wasm_limits(
            parrot_wasm::WasmConfig {
                fuel_per_message: 20_000_000,
                epoch_deadline: 10_000,
                ..Default::default()
            },
            Default::default(),
        );
    let guest = spawner
        .spawn_artifact(
            &artifact(),
            &["/user/guest".into()],
            Some(&config(&[("timeout-ms", "100"), ("mode", mode)])),
        )
        .await
        .unwrap()
        .remove(0);
    let harness = Harness {
        engine,
        guest,
        observed,
    };
    assert_eq!(harness.value(99).await, b"error:Timeout");
    assert_eq!(*harness.observed.values.lock().unwrap(), [42]);
    harness.observed.release.notify_one();
    native.send(Box::new(Request(8))).await.unwrap();
    assert_eq!(
        *harness.observed.values.lock().unwrap(),
        [42, 6, 8],
        "timed-out admission must not enqueue later"
    );
    harness.stop().await;
}

#[tokio::test]
async fn business_codec_alias_cannot_acquire_lifecycle_control() {
    let grant = WasmMessageGrant {
        request_type_key: HOOK_ALIAS.into(),
        ..ask_grant()
    };
    let harness = Harness::new(vec![grant], config(&[("request-key", HOOK_ALIAS)])).await;
    assert_eq!(harness.value(7).await, b"error:Denied");
    assert!(harness.observed.values.lock().unwrap().is_empty());
    assert!(harness.engine.get_actor_ref("/user/native").is_some());
    harness.stop().await;
}

#[tokio::test]
async fn guest_ask_routes_over_actual_tcp_through_explicit_remote_host() {
    use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
    struct Lookup(std::sync::Weak<ThreadActorSystem>);
    #[async_trait::async_trait]
    impl LocalLookup for Lookup {
        async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
            self.0.upgrade()?.resolve_actor(path)
        }
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let server_engine = ThreadActorSystem::shared(Default::default());
        let observed = Arc::new(Observed::default());
        server_engine
            .spawn_at(
                Native(observed.clone()),
                "/user/native",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let server = RemoteActorSystem::new(
            RemoteConfig::tcp("wasm-messaging-peer", Some("127.0.0.1:0".parse().unwrap())),
            Arc::new(Lookup(Arc::downgrade(&server_engine))),
        )
        .unwrap();
        server.start().await.unwrap();
        let engine = ThreadActorSystem::shared(Default::default());
        let client = RemoteActorSystem::new(
            RemoteConfig::tcp("wasm-messaging-client", None),
            Arc::new(Lookup(Arc::downgrade(&engine))),
        )
        .unwrap();
        client.start().await.unwrap();
        client
            .connect(&NodeAddr::tcp(
                "wasm-messaging-peer",
                server.local_addr().unwrap(),
            ))
            .await
            .unwrap();
        let destination = "parrot://wasm-messaging-peer/user/native";
        let spawner = ThreadPropsSpawner::with_remote(engine.clone(), client.clone())
            .with_wasm_messaging(vec![WasmMessageGrant {
                destination: destination.into(),
                ..ask_grant()
            }])
            .unwrap()
            .with_wasm_limits(
                parrot_wasm::WasmConfig {
                    fuel_per_message: 20_000_000,
                    epoch_deadline: 10_000,
                    ..Default::default()
                },
                Default::default(),
            );
        let guest = spawner
            .spawn_artifact(
                &artifact(),
                &["/user/guest".into()],
                Some(&config(&[("destination", destination)])),
            )
            .await
            .unwrap()
            .remove(0);
        let harness = Harness {
            engine,
            guest,
            observed,
        };
        assert_eq!(harness.value(19).await, response(26));
        assert_eq!(*harness.observed.values.lock().unwrap(), [19]);
        harness.stop().await;
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
        server_engine.shutdown_internal().await.unwrap();
    })
    .await
    .expect("guest TCP roundtrip and retirement deadline");
}
