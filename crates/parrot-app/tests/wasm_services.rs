#![cfg(feature = "wasm")]

use parrot::thread::ThreadActorSystem;
use parrot_api::{
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey, ReceptionistStream},
    types::BoxedActorRef,
};
use parrot_app::{
    assemble::ParrotSpawner,
    host::{
        wasm::{WasmAsk, WasmReply, WasmServiceGrant, WasmServiceOperation, WasmServicePolicy},
        ThreadPropsSpawner,
    },
    manifest::ArtifactRef,
};
use parrot_remote::{acl::AclRules, receptionist::ReceptionistFacade};
use sha2::{Digest, Sha256};
use std::{future::Future, sync::Arc, time::Duration};

const KEY: &str = "edge/guest-service";

#[cfg(unix)]
#[path = "support/native_path.rs"]
mod native_path;
#[cfg(unix)]
#[path = "support/policy_fifo.rs"]
mod policy_fifo;

#[tokio::test]
async fn service_policy_file_is_bounded_strict_and_immutable_after_loading() {
    #[derive(serde::Serialize)]
    struct Document {
        grants: Vec<WasmServiceGrant>,
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("operator services.toml");
    std::fs::write(&path, toml::to_string(&Document { grants: all() }).unwrap()).unwrap();
    let loaded = WasmServicePolicy::load_file(&path).await.unwrap();
    std::fs::write(&path, "grants = []").unwrap();
    assert_eq!(loaded.grants().len(), 2);
    assert_eq!(
        WasmServicePolicy::load_file(&path).await.unwrap(),
        WasmServicePolicy::default()
    );
    let mut bytes = b"grants = []\n#".to_vec();
    bytes.resize(WasmServicePolicy::MAX_FILE_BYTES, b'x');
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        WasmServicePolicy::load_file(&path).await.unwrap(),
        WasmServicePolicy::default()
    );
    bytes.push(b'x');
    for invalid in [bytes, vec![255], b"grants = []\nunknown = true".to_vec()] {
        std::fs::write(&path, invalid).unwrap();
        assert!(WasmServicePolicy::load_file(&path).await.is_err());
    }
    assert!(WasmServicePolicy::load_file(temp.path()).await.is_err());
    assert!(WasmServicePolicy::load_file(temp.path().join("missing"))
        .await
        .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn service_policy_native_paths_and_symlinks_preserve_policy() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(native_path::file_name());
    std::fs::write(&path, "grants = []").unwrap();
    assert_eq!(
        WasmServicePolicy::load_file(&path).await.unwrap(),
        WasmServicePolicy::default()
    );
    let link = temp.path().join("link");
    symlink(&path, &link).unwrap();
    assert_eq!(
        WasmServicePolicy::load_file(link).await.unwrap(),
        WasmServicePolicy::default()
    );
}

#[cfg(unix)]
#[test]
fn service_policy_fifo_is_rejected_without_waiting_for_a_writer() {
    policy_fifo::rejects_without_writer(
        "service_policy_fifo_is_rejected_without_waiting_for_a_writer",
        WasmServicePolicy::load_file,
    );
}

#[cfg(unix)]
#[path = "wasm_services/process.rs"]
mod process;
fn key() -> ReceptionistKey {
    ReceptionistKey::new(KEY).unwrap()
}
fn artifact() -> ArtifactRef {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../parrot-wasm/tests/fixtures/services-component.wasm")
        .canonicalize()
        .unwrap();
    ArtifactRef::Wasm {
        digest: format!(
            "{:x}",
            Sha256::digest(include_bytes!(
                "../../parrot-wasm/tests/fixtures/services-component.wasm"
            ))
        ),
        uri: url::Url::from_file_path(path).unwrap().into(),
    }
}
fn grant(operation: WasmServiceOperation) -> WasmServiceGrant {
    let ArtifactRef::Wasm { digest, .. } = artifact() else {
        unreachable!()
    };
    WasmServiceGrant {
        source_path: "/user/guest".into(),
        artifact_digest: digest,
        key: KEY.into(),
        operation,
    }
}
fn all() -> Vec<WasmServiceGrant> {
    vec![
        grant(WasmServiceOperation::Register),
        grant(WasmServiceOperation::Subscribe),
    ]
}
fn config(values: &[(&str, &str)]) -> toml::Table {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), toml::Value::String((*value).into())))
        .collect()
}
async fn guarded(future: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .expect("service lifecycle deadline");
}
struct Harness {
    engine: Arc<ThreadActorSystem>,
    gateway: Arc<ReceptionistFacade>,
    spawner: ThreadPropsSpawner,
}
impl Harness {
    fn new(grants: Vec<WasmServiceGrant>) -> Self {
        Self::with_gateway(
            grants,
            Arc::new(ReceptionistFacade::new("guest-services").with_acl(AclRules::permissive())),
        )
    }
    fn with_gateway(grants: Vec<WasmServiceGrant>, gateway: Arc<ReceptionistFacade>) -> Self {
        Self::with_supervision(grants, gateway, Default::default())
    }
    fn with_supervision(
        grants: Vec<WasmServiceGrant>,
        gateway: Arc<ReceptionistFacade>,
        strategy: parrot::thread::config::SupervisorStrategy,
    ) -> Self {
        let engine = ThreadActorSystem::shared(parrot::thread::config::ThreadActorSystemConfig {
            default_supervisor_strategy: strategy,
            ..Default::default()
        });
        engine.set_receptionist_gateway(gateway.clone());
        let spawner = ThreadPropsSpawner::new(engine.clone())
            .with_wasm_services(grants)
            .unwrap()
            .with_wasm_limits(
                parrot_wasm::WasmConfig {
                    fuel_per_message: 20_000_000,
                    epoch_deadline: 10_000,
                    ..Default::default()
                },
                Default::default(),
            );
        Self {
            engine,
            gateway,
            spawner,
        }
    }
    async fn spawn(&self, settings: Option<&toml::Table>) -> BoxedActorRef {
        self.spawner
            .spawn_artifact(&artifact(), &["/user/guest".into()], settings)
            .await
            .unwrap()
            .remove(0)
    }
}
async fn call(actor: &BoxedActorRef, command: &str, payload: &[u8]) -> Vec<u8> {
    actor
        .send(Box::new(WasmAsk {
            type_key: command.into(),
            payload: payload.into(),
        }))
        .await
        .unwrap()
        .downcast::<WasmReply>()
        .unwrap()
        .0
}
async fn event(stream: &mut ReceptionistStream, registered: bool, path: &str) {
    let value = tokio::time::timeout(Duration::from_secs(5), stream.recv())
        .await
        .unwrap_or_else(|error| {
            panic!("service event deadline: registered={registered} path={path}: {error}")
        })
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
fn empty(stream: &mut ReceptionistStream) {
    assert!(matches!(
        stream.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn actual_guest_registration_snapshot_live_withdrawal_and_close() {
    guarded(async {
        let h = Harness::new(all());
        let mut observed = h.gateway.subscribe(key()).await.unwrap();
        let guest = h.spawn(None).await;
        assert_eq!(call(&guest, "register", b"").await, b"ok");
        event(&mut observed, true, "/user/guest").await;
        let handle = call(&guest, "subscribe", b"").await;
        assert_eq!(handle.len(), 8);
        assert_eq!(
            call(&guest, "next", &handle).await,
            b"registered:/user/guest"
        );
        assert_eq!(call(&guest, "poll", &handle).await, b"error:Timeout");
        assert_eq!(call(&guest, "deregister", b"").await, b"ok");
        event(&mut observed, false, "/user/guest").await;
        assert_eq!(
            call(&guest, "next", &handle).await,
            b"unregistered:/user/guest"
        );
        assert_eq!(call(&guest, "close", &handle).await, b"ok");
        assert_eq!(call(&guest, "next", &handle).await, b"error:InvalidHandle");
        assert_eq!(call(&guest, "register", b"").await, b"ok");
        event(&mut observed, true, "/user/guest").await;
        guest.stop().await.unwrap();
        event(&mut observed, false, "/user/guest").await;
        empty(&mut observed);
        let mut snapshot = h.gateway.subscribe(key()).await.unwrap();
        empty(&mut snapshot);
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn authority_is_exact_default_denied_and_business_config_cannot_grant_it() {
    guarded(async {
        for grants in [
            vec![],
            vec![WasmServiceGrant {
                source_path: "/user/other".into(),
                ..grant(WasmServiceOperation::Register)
            }],
            vec![WasmServiceGrant {
                artifact_digest: "00".repeat(32),
                ..grant(WasmServiceOperation::Register)
            }],
        ] {
            let h = Harness::new(grants);
            let mut observed = h.gateway.subscribe(key()).await.unwrap();
            let guest = h
                .spawn(Some(&config(&[
                    ("grants", "allow-all"),
                    ("service-key", KEY),
                ])))
                .await;
            assert_eq!(call(&guest, "register", b"").await, b"error:Disabled");
            assert_eq!(call(&guest, "subscribe", b"").await, b"error:Disabled");
            empty(&mut observed);
            h.engine.shutdown_internal().await.unwrap();
        }
        let h = Harness::new(vec![grant(WasmServiceOperation::Subscribe)]);
        let guest = h.spawn(None).await;
        assert_eq!(call(&guest, "register", b"").await, b"error:Denied");
        assert_eq!(
            call(&guest, "subscribe", b"edge/forbidden").await,
            b"error:Denied"
        );
        assert_eq!(call(&guest, "deregister", b"").await, b"error:Denied");
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn startup_registration_failure_retires_before_same_path_retry() {
    guarded(async {
        let h = Harness::new(all());
        let mut observed = h.gateway.subscribe(key()).await.unwrap();
        let result = h
            .spawner
            .spawn_artifact(
                &artifact(),
                &["/user/guest".into()],
                Some(&config(&[
                    ("register-on-start", KEY),
                    ("trap-on-start", "true"),
                ])),
            )
            .await;
        assert!(result.is_err());
        assert!(h.engine.resolve_actor("/user/guest").is_none());
        assert!(h.spawner.pending_cleanup().is_empty());
        event(&mut observed, true, "/user/guest").await;
        event(&mut observed, false, "/user/guest").await;
        empty(&mut observed);
        let guest = h.spawn(Some(&config(&[("register-on-start", KEY)]))).await;
        event(&mut observed, true, "/user/guest").await;
        assert_eq!(call(&guest, "echo", b"replacement").await, b"replacement");
        guest.stop().await.unwrap();
        event(&mut observed, false, "/user/guest").await;
        empty(&mut observed);
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn late_pool_conflict_retires_only_the_actual_service_owner() {
    guarded(async {
        let h = Harness::new(vec![
            grant(WasmServiceOperation::Register),
            WasmServiceGrant {
                source_path: "/user/taken".into(),
                ..grant(WasmServiceOperation::Register)
            },
        ]);
        let original = h
            .spawner
            .spawn_artifact(&artifact(), &["/user/taken".into()], None)
            .await
            .unwrap()
            .remove(0);
        let mut observed = h.gateway.subscribe(key()).await.unwrap();
        let result = h
            .spawner
            .spawn_artifact(
                &artifact(),
                &["/user/guest".into(), "/user/taken".into()],
                Some(&config(&[("register-on-start", KEY)])),
            )
            .await;
        assert!(result.is_err());
        event(&mut observed, true, "/user/guest").await;
        event(&mut observed, false, "/user/guest").await;
        empty(&mut observed);
        assert!(h.engine.resolve_actor("/user/guest").is_none());
        assert!(h.spawner.pending_cleanup().is_empty());
        assert_eq!(call(&original, "echo", b"untouched").await, b"untouched");
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn guest_handle_limit_close_and_actor_stop_release_actual_gateway_capacity() {
    guarded(async {
        let gateway = Arc::new(
            ReceptionistFacade::with_subscription_limits(
                "guest-services",
                parrot_config::receptionist::SubscriptionLimits {
                    max_subscriptions: 16,
                    ..Default::default()
                },
            )
            .with_acl(AclRules::permissive()),
        );
        let h = Harness::with_gateway(vec![grant(WasmServiceOperation::Subscribe)], gateway);
        let guest = h.spawn(None).await;
        let mut handles = Vec::new();
        for n in 1u64..=16 {
            let handle = call(&guest, "subscribe", b"").await;
            assert_eq!(handle, n.to_le_bytes());
            handles.push(handle);
        }
        assert_eq!(call(&guest, "subscribe", b"").await, b"error:Limit");
        assert!(h.gateway.subscribe(key()).await.is_err());
        assert_eq!(call(&guest, "close", &handles[4]).await, b"ok");
        assert_eq!(call(&guest, "subscribe", b"").await, 17u64.to_le_bytes());
        assert_eq!(
            call(&guest, "close", &handles[4]).await,
            b"error:InvalidHandle"
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
async fn host_acl_remains_authoritative_and_new_policies_preserve_clone_isolation() {
    guarded(async {
        let denied = Arc::new(ReceptionistFacade::new("guest-services"));
        let h = Harness::with_gateway(all(), denied);
        let guest = h.spawn(None).await;
        assert_eq!(call(&guest, "register", b"").await, b"error:Failed");
        assert_eq!(call(&guest, "subscribe", b"").await, b"error:Failed");
        h.engine.shutdown_internal().await.unwrap();
        let mut h = Harness::new(vec![]);
        let original = h.spawner.clone();
        h.spawner = h
            .spawner
            .with_wasm_services(all())
            .unwrap()
            .with_wasm_messaging(vec![])
            .unwrap();
        let guest = h.spawn(None).await;
        assert_eq!(call(&guest, "register", b"").await, b"ok");
        guest.stop().await.unwrap();
        let guest = original
            .spawn_artifact(&artifact(), &["/user/guest".into()], None)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(call(&guest, "register", b"").await, b"error:Disabled");
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[test]
fn service_policies_are_strict_canonical_and_independent_of_business_tables() {
    let policy = WasmServicePolicy::new(all()).unwrap();
    let mut reversed = all();
    reversed.reverse();
    for grant in &mut reversed {
        grant.artifact_digest = format!("sha256:{}", grant.artifact_digest.to_uppercase());
    }
    assert_eq!(policy, WasmServicePolicy::new(reversed).unwrap());
    let text = toml::to_string(&toml::Table::from_iter([(
        "grants".into(),
        toml::Value::try_from(all()).unwrap(),
    )]))
    .unwrap();
    assert_eq!(policy, WasmServicePolicy::from_toml(&text).unwrap());
    assert!(WasmServicePolicy::from_toml(&(text.clone() + "\nextra = true")).is_err());
    assert!(WasmServicePolicy::from_toml("grants = []\nunknown = true").is_err());
    assert!(WasmServicePolicy::from_toml("").is_err());
    assert!(WasmServicePolicy::new(vec![grant(WasmServiceOperation::Register); 2]).is_err());
    assert!(WasmServicePolicy::new(vec![WasmServiceGrant {
        key: "edge/../bad".into(),
        ..grant(WasmServiceOperation::Register)
    }])
    .is_err());
    let grants: Vec<_> = (0..256)
        .map(|n| WasmServiceGrant {
            source_path: format!("/user/guest-{n}"),
            ..grant(WasmServiceOperation::Register)
        })
        .collect();
    assert_eq!(
        WasmServicePolicy::new(grants.clone())
            .unwrap()
            .grants()
            .len(),
        256
    );
    assert!(WasmServicePolicy::new(
        [
            grants,
            vec![WasmServiceGrant {
                source_path: "/user/last".into(),
                ..grant(WasmServiceOperation::Register)
            }]
        ]
        .concat()
    )
    .is_err());
}

#[tokio::test]
async fn resumed_store_trap_retires_streams_but_registration_lives_until_actor_retirement() {
    guarded(async {
        let gateway = Arc::new(
            ReceptionistFacade::with_subscription_limits(
                "guest-services",
                parrot_config::receptionist::SubscriptionLimits {
                    max_subscriptions: 2,
                    ..Default::default()
                },
            )
            .with_acl(AclRules::permissive()),
        );
        let h = Harness::with_supervision(
            all(),
            gateway,
            parrot::thread::config::SupervisorStrategy::Resume,
        );
        let mut observed = h.gateway.subscribe(key()).await.unwrap();
        let guest = h.spawn(None).await;
        assert_eq!(call(&guest, "register", b"").await, b"ok");
        event(&mut observed, true, "/user/guest").await;
        let handle = call(&guest, "subscribe", b"").await;
        assert!(h.gateway.subscribe(key()).await.is_err());
        assert!(guest
            .send(Box::new(WasmAsk {
                type_key: "trap".into(),
                payload: vec![]
            }))
            .await
            .is_err());
        let mut recovered = h.gateway.subscribe(key()).await.unwrap();
        event(&mut recovered, true, "/user/guest").await;
        empty(&mut observed);
        assert_eq!(call(&guest, "next", &handle).await, b"error:InvalidHandle");
        assert_eq!(
            call(&guest, "echo", b"same-admitted-actor").await,
            b"same-admitted-actor"
        );
        guest.stop().await.unwrap();
        event(&mut observed, false, "/user/guest").await;
        event(&mut recovered, false, "/user/guest").await;
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn admitted_guest_keeps_its_gateway_and_generation_cleanup_after_gateway_replacement() {
    guarded(async {
        let h = Harness::new(all());
        let mut original = h.gateway.subscribe(key()).await.unwrap();
        let guest = h.spawn(None).await;
        let replacement =
            Arc::new(ReceptionistFacade::new("replacement").with_acl(AclRules::permissive()));
        let mut new_gateway = replacement.subscribe(key()).await.unwrap();
        h.engine.set_receptionist_gateway(replacement.clone());
        assert_eq!(call(&guest, "register", b"").await, b"ok");
        event(&mut original, true, "/user/guest").await;
        empty(&mut new_gateway);
        guest.stop().await.unwrap();
        event(&mut original, false, "/user/guest").await;
        empty(&mut new_gateway);
        let next = h.spawn(None).await;
        assert_eq!(call(&next, "register", b"").await, b"ok");
        event(&mut new_gateway, true, "/user/guest").await;
        empty(&mut original);
        next.stop().await.unwrap();
        event(&mut new_gateway, false, "/user/guest").await;
        empty(&mut original);
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn default_supervision_retires_trapped_guest_registration_and_all_gateway_capacity() {
    guarded(async {
        let gateway = Arc::new(
            ReceptionistFacade::with_subscription_limits(
                "guest-services",
                parrot_config::receptionist::SubscriptionLimits {
                    max_subscriptions: 17,
                    ..Default::default()
                },
            )
            .with_acl(AclRules::permissive()),
        );
        let h = Harness::with_gateway(all(), gateway);
        let mut observed = h.gateway.subscribe(key()).await.unwrap();
        let guest = h.spawn(None).await;
        assert_eq!(call(&guest, "register", b"").await, b"ok");
        event(&mut observed, true, "/user/guest").await;
        for n in 1u64..=16 {
            assert_eq!(call(&guest, "subscribe", b"").await, n.to_le_bytes());
        }
        assert!(h.gateway.subscribe(key()).await.is_err());
        assert!(guest
            .send(Box::new(WasmAsk {
                type_key: "trap".into(),
                payload: vec![]
            }))
            .await
            .is_err());
        event(&mut observed, false, "/user/guest").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while h.engine.resolve_actor("/user/guest").is_some() || guest.is_alive().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let replacement = h.spawn(None).await;
        assert_eq!(call(&replacement, "register", b"").await, b"ok");
        event(&mut observed, true, "/user/guest").await;
        for n in 1u64..=16 {
            assert_eq!(call(&replacement, "subscribe", b"").await, n.to_le_bytes());
        }
        assert!(h.gateway.subscribe(key()).await.is_err());
        assert_eq!(call(&replacement, "subscribe", b"").await, b"error:Limit");
        replacement.stop().await.unwrap();
        event(&mut observed, false, "/user/guest").await;
        let mut recovered = Vec::new();
        for _ in 0..16 {
            let mut stream = h.gateway.subscribe(key()).await.unwrap();
            empty(&mut stream);
            recovered.push(stream);
        }
        assert!(h.gateway.subscribe(key()).await.is_err());
        h.engine.shutdown_internal().await.unwrap();
    })
    .await;
}
