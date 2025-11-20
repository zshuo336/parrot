#![cfg(feature = "dylib")]

#[path = "support/messaging.rs"]
mod messaging;
use parrot::thread::ThreadActorSystem;
use parrot_abi::*;
use parrot_api::{
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey, ReceptionistStream},
    types::BoxedActorRef,
};
use parrot_app::{
    assemble::ParrotSpawner,
    host::{dylib::*, ThreadPropsSpawner},
    ArtifactRef,
};
use parrot_remote::{acl::AclRules, receptionist::ReceptionistFacade};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

const KEY: &str = "native/business";
fn key() -> ReceptionistKey {
    ReceptionistKey::new(KEY).unwrap()
}
fn artifact() -> ArtifactRef {
    ArtifactRef::Dylib {
        digest: digest(),
        uri: url::Url::from_file_path(messaging::fixture())
            .unwrap()
            .into(),
        abi: 1,
    }
}
fn digest() -> String {
    format!(
        "{:x}",
        Sha256::digest(std::fs::read(messaging::fixture()).unwrap())
    )
}
fn grant(operation: NativeServiceOperation) -> NativeServiceGrant {
    NativeServiceGrant {
        source_path: "/user/guest".into(),
        artifact_digest: digest(),
        key: KEY.into(),
        operation,
    }
}
fn grants() -> Vec<NativeServiceGrant> {
    vec![
        grant(NativeServiceOperation::Register),
        grant(NativeServiceOperation::Subscribe),
    ]
}
struct Harness {
    engine: Arc<ThreadActorSystem>,
    gateway: Arc<ReceptionistFacade>,
    spawner: ThreadPropsSpawner,
}
impl Harness {
    fn new(grants: Vec<NativeServiceGrant>) -> Self {
        Self::with_gateway(
            grants,
            Arc::new(ReceptionistFacade::new("native-services").with_acl(AclRules::permissive())),
        )
    }
    fn with_gateway(grants: Vec<NativeServiceGrant>, gateway: Arc<ReceptionistFacade>) -> Self {
        let engine = ThreadActorSystem::shared(Default::default());
        engine.set_receptionist_gateway(gateway.clone());
        // SAFETY: repository-owned fixture and dependencies obey the native ABI.
        let spawner =
            unsafe { ThreadPropsSpawner::new(engine.clone()).with_trusted_dylib(&digest()) }
                .unwrap()
                .with_dylib_services(grants)
                .unwrap();
        Self {
            engine,
            gateway,
            spawner,
        }
    }
    async fn spawn_with(
        &self,
        spawner: &ThreadPropsSpawner,
        config: Option<&toml::Table>,
    ) -> BoxedActorRef {
        spawner
            .spawn_artifact(&artifact(), &["/user/guest".into()], config)
            .await
            .unwrap()
            .remove(0)
    }
    async fn spawn(&self) -> BoxedActorRef {
        self.spawn_with(&self.spawner, None).await
    }
}
async fn call(actor: &BoxedActorRef, command: &str, payload: &[u8]) -> Vec<u8> {
    tokio::time::timeout(
        Duration::from_secs(10),
        actor.send(Box::new(DylibAsk {
            type_key: format!("service:{command}"),
            payload: payload.into(),
        })),
    )
    .await
    .unwrap()
    .unwrap()
    .downcast::<DylibReply>()
    .unwrap()
    .0
}
fn code(value: &[u8]) -> u32 {
    u32::from_le_bytes(value[..4].try_into().unwrap())
}
async fn ok(actor: &BoxedActorRef, command: &str, payload: &[u8]) -> Vec<u8> {
    let value = call(actor, command, payload).await;
    assert_eq!(code(&value), ABI_SERVICE_OK, "{command}: {value:?}");
    value[4..].to_vec()
}
async fn event(stream: &mut ReceptionistStream, registered: bool) {
    let event = tokio::time::timeout(Duration::from_secs(5), stream.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        if registered {
            ReceptionistEvent::Registered {
                key: key(),
                remote_path: "/user/guest".into(),
            }
        } else {
            ReceptionistEvent::Unregistered {
                key: key(),
                remote_path: "/user/guest".into(),
            }
        }
    );
}
async fn guarded(future: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(90), future)
        .await
        .expect("native discovery lifecycle deadline");
}

#[tokio::test]
async fn real_native_snapshot_live_events_timeout_and_overflow_preserve_order() {
    guarded(async {
        let h = Harness::new(grants());
        let mut observed = h.gateway.subscribe(key()).await.unwrap();
        let guest = h.spawn().await;
        ok(&guest, "register", b"").await;
        event(&mut observed, true).await;
        let handle = ok(&guest, "subscribe", b"").await;
        assert_eq!(handle, 1u64.to_le_bytes());
        assert_eq!(
            call(&guest, "small", &handle).await,
            [ABI_SERVICE_LIMIT.to_le_bytes(), 0u32.to_le_bytes()].concat()
        );
        assert_eq!(
            ok(&guest, "next", &handle).await,
            [
                ABI_SERVICE_REGISTERED.to_le_bytes().as_slice(),
                b"/user/guest"
            ]
            .concat()
        );
        assert_eq!(
            call(&guest, "poll", &handle).await,
            [ABI_SERVICE_TIMEOUT.to_le_bytes(), 0u32.to_le_bytes()].concat()
        );
        ok(&guest, "deregister", b"").await;
        event(&mut observed, false).await;
        assert_eq!(
            ok(&guest, "next", &handle).await,
            [
                ABI_SERVICE_UNREGISTERED.to_le_bytes().as_slice(),
                b"/user/guest"
            ]
            .concat()
        );
        ok(&guest, "close", &handle).await;
        assert_eq!(
            code(&call(&guest, "close", &handle).await),
            ABI_SERVICE_HANDLE
        );
        assert_eq!(
            code(&call(&guest, "next", &handle).await),
            ABI_SERVICE_HANDLE
        );
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn exact_authority_default_deny_and_business_config_cannot_grant_services() {
    guarded(async {
        let base = grant(NativeServiceOperation::Register);
        for authority in [
            vec![],
            vec![NativeServiceGrant {
                source_path: "/user/other".into(),
                ..base.clone()
            }],
            vec![NativeServiceGrant {
                artifact_digest: "0".repeat(64),
                ..base.clone()
            }],
            vec![NativeServiceGrant {
                key: "other/key".into(),
                ..base.clone()
            }],
            vec![grant(NativeServiceOperation::Subscribe)],
        ] {
            let h = Harness::new(authority);
            let mut observed = h.gateway.subscribe(key()).await.unwrap();
            let injected = toml::Table::from_iter([
                (
                    "native-service-policy".into(),
                    toml::Value::String("grants=all".into()),
                ),
                ("self_path".into(), "/user/other".into()),
            ]);
            let guest = h.spawn_with(&h.spawner, Some(&injected)).await;
            assert_eq!(
                code(&call(&guest, "register", b"").await),
                ABI_SERVICE_DENIED
            );
            assert_eq!(
                code(&call(&guest, "deregister", b"").await),
                ABI_SERVICE_DENIED
            );
            assert!(matches!(
                observed.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            h.engine.shutdown_internal().await.unwrap();
        }
        let h = Harness::new(vec![base]);
        let guest = h.spawn().await;
        assert_eq!(
            call(&guest, "subscribe", b"").await,
            [
                ABI_SERVICE_DENIED.to_le_bytes().as_slice(),
                &0u64.to_le_bytes()
            ]
            .concat()
        );
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn all_sixteen_stream_slots_are_recovered_after_close_and_actor_retirement() {
    guarded(async {
        let gateway = Arc::new(
            ReceptionistFacade::with_subscription_limits(
                "native-services",
                parrot_config::receptionist::SubscriptionLimits {
                    max_subscriptions: 16,
                    ..Default::default()
                },
            )
            .with_acl(AclRules::permissive()),
        );
        let h = Harness::with_gateway(grants(), gateway);
        let guest = h.spawn().await;
        for n in 1u64..=16 {
            assert_eq!(ok(&guest, "subscribe", b"").await, n.to_le_bytes());
        }
        assert_eq!(
            code(&call(&guest, "subscribe", b"").await),
            ABI_SERVICE_LIMIT
        );
        assert!(h.gateway.subscribe(key()).await.is_err());
        ok(&guest, "close", &5u64.to_le_bytes()).await;
        assert_eq!(ok(&guest, "subscribe", b"").await, 17u64.to_le_bytes());
        assert_eq!(
            code(&call(&guest, "close", &5u64.to_le_bytes()).await),
            ABI_SERVICE_HANDLE
        );
        assert!(h.gateway.subscribe(key()).await.is_err());
        guest.stop().await.unwrap();
        let mut recovered = Vec::new();
        for _ in 0..16 {
            recovered.push(h.gateway.subscribe(key()).await.unwrap());
        }
        assert!(h.gateway.subscribe(key()).await.is_err());
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn actual_actor_generation_withdraws_registration_before_path_reuse() {
    guarded(async {
        let h = Harness::new(grants());
        let mut observed = h.gateway.subscribe(key()).await.unwrap();
        let guest = h.spawn().await;
        ok(&guest, "register", b"").await;
        event(&mut observed, true).await;
        guest.stop().await.unwrap();
        event(&mut observed, false).await;
        let replacement = h.spawn().await;
        ok(&replacement, "register", b"").await;
        event(&mut observed, true).await;
        guest.stop().await.unwrap();
        assert_eq!(ok(&replacement, "subscribe", b"").await, 1u64.to_le_bytes());
        assert_eq!(
            ok(&replacement, "next", &1u64.to_le_bytes()).await,
            [
                ABI_SERVICE_REGISTERED.to_le_bytes().as_slice(),
                b"/user/guest"
            ]
            .concat()
        );
        assert!(matches!(
            observed.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        replacement.stop().await.unwrap();
        event(&mut observed, false).await;
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn discovery_and_messaging_share_attempt_budget_and_reset_per_invocation() {
    guarded(async {
        let h = Harness::new(grants());
        let guest = h.spawn().await;
        let invalid = ok(&guest, "invalid", b"").await;
        assert_eq!(invalid.len(), 65 * 4);
        for value in invalid[..64 * 4].as_chunks::<4>().0 {
            assert_eq!(code(value), ABI_SERVICE_INVALID);
        }
        assert_eq!(code(&invalid[64 * 4..]), ABI_SERVICE_LIMIT);
        let mixed = ok(&guest, "mixed", b"").await;
        for value in mixed[..63 * 4].as_chunks::<4>().0 {
            assert_eq!(code(value), ABI_SERVICE_INVALID);
        }
        assert_eq!(code(&mixed[63 * 4..]), ABI_MESSAGE_DENIED);
        assert_eq!(code(&mixed[64 * 4..]), ABI_MESSAGE_LIMIT);
        ok(&guest, "register", b"").await;
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn registry_acl_and_immutable_spawner_clones_remain_authoritative() {
    guarded(async {
        let h = Harness::with_gateway(grants(), Arc::new(ReceptionistFacade::new("denied")));
        let guest = h.spawn().await;
        assert_eq!(
            code(&call(&guest, "register", b"").await),
            ABI_SERVICE_FAILED
        );
        assert_eq!(
            code(&call(&guest, "subscribe", b"").await),
            ABI_SERVICE_FAILED
        );
        h.engine.shutdown_internal().await.unwrap();
        let h = Harness::new(vec![]);
        let original = h.spawner.clone();
        let authorized = h
            .spawner
            .clone()
            .with_dylib_services(grants())
            .unwrap()
            .with_dylib_messaging(vec![])
            .unwrap();
        let guest = h.spawn_with(&authorized, None).await;
        ok(&guest, "register", b"").await;
        guest.stop().await.unwrap();
        let replacement = h.spawn_with(&original, None).await;
        assert_eq!(
            code(&call(&replacement, "register", b"").await),
            ABI_SERVICE_DENIED
        );
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[test]
fn operator_policy_is_strict_normalized_and_bounds_exact_authority() {
    let original = grants();
    let policy = NativeServicePolicy::new(original.clone()).unwrap();
    let mut reverse = original.clone();
    reverse.reverse();
    for grant in &mut reverse {
        grant.artifact_digest = format!("sha256:{}", grant.artifact_digest.to_uppercase());
    }
    assert_eq!(policy, NativeServicePolicy::new(reverse).unwrap());
    let text = toml::to_string(&toml::Table::from_iter([(
        "grants".into(),
        toml::Value::try_from(&original).unwrap(),
    )]))
    .unwrap();
    assert_eq!(NativeServicePolicy::from_toml(&text).unwrap(), policy);
    for invalid in [
        String::new(),
        "grants=[]\nunknown=true".into(),
        text.replace("source_path =", "unknown = true\nsource_path ="),
        text.replace("\"register\"", "\"tell\""),
    ] {
        assert!(NativeServicePolicy::from_toml(&invalid).is_err());
    }
    assert!(NativeServicePolicy::new(vec![original[0].clone(); 2]).is_err());
    for path in [
        "/",
        "/user//guest",
        "/user/../guest",
        "/user/guest?x",
        "/user/guest\n",
    ] {
        assert!(NativeServicePolicy::new(vec![NativeServiceGrant {
            source_path: path.into(),
            ..original[0].clone()
        }])
        .is_err());
    }
    for key in ["", "a/../b", "a b", "a%20b"] {
        assert!(NativeServicePolicy::new(vec![NativeServiceGrant {
            key: key.into(),
            ..original[0].clone()
        }])
        .is_err());
    }
    let mut many: Vec<_> = (0..256)
        .map(|n| NativeServiceGrant {
            source_path: format!("/user/{n}"),
            ..original[0].clone()
        })
        .collect();
    NativeServicePolicy::new(many.clone()).unwrap();
    many.push(NativeServiceGrant {
        source_path: "/user/last".into(),
        ..original[0].clone()
    });
    assert!(NativeServicePolicy::new(many).is_err());
}

#[tokio::test]
async fn operator_file_is_bounded_and_immutable_after_loading() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("services.toml");
    let document =
        toml::Table::from_iter([("grants".into(), toml::Value::try_from(grants()).unwrap())]);
    std::fs::write(&path, toml::to_string(&document).unwrap()).unwrap();
    let loaded = NativeServicePolicy::load_file(&path).await.unwrap();
    std::fs::write(&path, "grants=[]").unwrap();
    assert_eq!(loaded.grants().len(), 2);
    assert_eq!(
        NativeServicePolicy::load_file(&path).await.unwrap(),
        NativeServicePolicy::default()
    );
    let mut exact = "grants=[]\n#".to_string();
    exact.extend(std::iter::repeat_n('x', 1_048_576 - exact.len()));
    assert!(NativeServicePolicy::from_toml(&exact).is_ok());
    exact.push('x');
    assert!(NativeServicePolicy::from_toml(&exact).is_err());
    for bytes in [vec![255], exact.into_bytes()] {
        std::fs::write(&path, bytes).unwrap();
        assert!(NativeServicePolicy::load_file(&path).await.is_err());
    }
    assert!(NativeServicePolicy::load_file(temp.path()).await.is_err());
    assert!(NativeServicePolicy::load_file(temp.path().join("missing"))
        .await
        .is_err());
}

#[tokio::test]
async fn actor_failure_withdraws_generation_and_recovers_every_subscription_slot() {
    guarded(async {
        let gateway = Arc::new(
            ReceptionistFacade::with_subscription_limits(
                "native-services",
                parrot_config::receptionist::SubscriptionLimits {
                    max_subscriptions: 16,
                    ..Default::default()
                },
            )
            .with_acl(AclRules::permissive()),
        );
        let h = Harness::with_gateway(grants(), gateway);
        let guest = h.spawn().await;
        ok(&guest, "register", b"").await;
        for _ in 0..16 {
            ok(&guest, "subscribe", b"").await;
        }
        assert!(h.gateway.subscribe(key()).await.is_err());
        let result = guest
            .send(Box::new(DylibAsk {
                type_key: "service:fail".into(),
                payload: vec![],
            }))
            .await;
        assert!(result.is_err());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    guest.retirement_status(),
                    parrot_api::address::RetirementStatus::Retired(_)
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            guest.retirement_status(),
            parrot_api::address::RetirementStatus::Retired(Ok(()))
        ));
        let mut recovered = Vec::new();
        for _ in 0..16 {
            let mut stream = h.gateway.subscribe(key()).await.unwrap();
            assert!(
                matches!(
                    stream.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ),
                "failed actor retained a service registration"
            );
            recovered.push(stream);
        }
        assert!(h.gateway.subscribe(key()).await.is_err());
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

struct Backend;
impl parrot_api::actor::Actor for Backend {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = parrot::thread::context::ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: parrot_api::types::BoxedMessage,
        _: &'a mut Self::Context,
    ) -> parrot_api::types::BoxedFuture<
        'a,
        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
    > {
        Box::pin(async move {
            let request = message.downcast::<DylibAsk>().unwrap();
            assert_eq!(request.type_key, "business");
            let mut reply = b"business:".to_vec();
            reply.extend_from_slice(&request.payload);
            Ok(Box::new(DylibReply(reply)) as parrot_api::types::BoxedMessage)
        })
    }
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
}

#[tokio::test]
async fn real_guest_discovers_backend_then_asks_with_separate_message_authority() {
    guarded(async {
        let h = Harness::new(vec![grant(NativeServiceOperation::Subscribe)]);
        let _backend = h
            .engine
            .spawn_at(Backend, "/user/backend", None, Default::default())
            .await
            .unwrap();
        h.gateway
            .register(key(), "/user/backend".into())
            .await
            .unwrap();
        let config: toml::Table = toml::from_str(
            "request='bin:parrot.dylib.Ask#v1'\nexpected='bin:parrot.dylib.Reply#v1'",
        )
        .unwrap();
        let payload = parrot_api::message::serde_remote_serialize(&DylibAsk {
            type_key: "business".into(),
            payload: b"verified".to_vec(),
        })
        .unwrap();
        let guest = h.spawn_with(&h.spawner, Some(&config)).await;
        assert_eq!(
            ok(&guest, "discover", &payload).await,
            ABI_MESSAGE_DENIED.to_le_bytes()
        );
        guest.stop().await.unwrap();
        let authorized = h
            .spawner
            .clone()
            .with_dylib_messaging(vec![NativeMessageGrant {
                source_path: "/user/guest".into(),
                artifact_digest: digest(),
                destination: "/user/backend".into(),
                request_type_key: "bin:parrot.dylib.Ask#v1".into(),
                operation: NativeMessageOperation::Ask {
                    reply_type_key: "bin:parrot.dylib.Reply#v1".into(),
                },
            }])
            .unwrap();
        let guest = h.spawn_with(&authorized, Some(&config)).await;
        let result = ok(&guest, "discover", &payload).await;
        assert_eq!(code(&result), ABI_MESSAGE_OK);
        let reply: DylibReply =
            parrot_api::message::serde_remote_deserialize(&result[4..]).unwrap();
        assert_eq!(reply.0, b"business:verified");
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn invalid_service_cli_arguments_never_create_capture_or_readiness() {
    let temp = tempfile::tempdir().unwrap();
    let policy = temp.path().join("policy.toml");
    std::fs::write(&policy, "grants=[]\nunknown=true").unwrap();
    let ready = temp.path().join("ready");
    let capture = temp.path().join("capture");
    let flag = std::ffi::OsString::from("--native-service-policy");
    for args in [
        vec![flag.clone()],
        vec![flag.clone(), "".into()],
        vec![flag.clone(), policy.clone().into_os_string()],
        vec![flag.clone(), temp.path().join("missing").into_os_string()],
        vec![
            flag.clone(),
            policy.clone().into_os_string(),
            flag,
            policy.into_os_string(),
        ],
    ] {
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"))
                .env("PARROT_APP_READY_FILE", &ready)
                .args(["run", "--manifest", "missing.toml", "--record"])
                .arg(&capture)
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("policy"));
        assert!(!ready.exists());
        assert!(!capture.exists());
    }
}
