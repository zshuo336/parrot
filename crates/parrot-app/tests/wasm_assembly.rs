#![cfg(feature = "host")]

use parrot::thread::system::ThreadActorSystem;
use parrot_api::types::BoxedActorRef;
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
    host::ThreadPropsSpawner,
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
        panic!("unexpected gateway")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("unexpected gateway")
    }
}

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../parrot-wasm/tests/fixtures/fixture-component.wasm")
        .canonicalize()
        .unwrap()
}
fn manifest() -> AppManifest {
    // Digest pinned to the checked-in, immutable WIT fixture.
    let digest = include_str!("support/wasm-fixture.sha256").trim();
    AppManifest::from_toml(&format!(
        r#"
name = "wasm-app"
version = "1.0.0"
[[components]]
name = "guest"
engine = "parrot"
instances = {{ Pool = 2 }}
artifact = {{ Wasm = {{ digest = "{digest}", uri = "file://{}" }} }}
[components.hooks]
on_start = "."
on_drain = "."
on_stop = "."
[components.config]
label = "中文"
limit = 7
items = [1, 2]
[components.config.nested]
enabled = true
"#,
        fixture().display()
    ))
    .unwrap()
}

#[cfg(feature = "wasm")]
#[tokio::test]
async fn manifest_wasm_pool_is_really_assembled_and_retired() {
    let manifest = manifest();
    let engine = ThreadActorSystem::shared(Default::default());
    let spawner = test_spawner(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(context.component_refs("guest").unwrap().len(), 2);
    for path in ["/user/guest-0", "/user/guest-1"] {
        assert!(engine.get_actor_ref(path).is_some());
    }

    use parrot_api::address::ActorRefExt;
    use parrot_app::host::wasm::{WasmAsk, WasmTell};
    for (index, actor) in context.component_refs("guest").unwrap().iter().enumerate() {
        assert!(actor.send(Box::new(())).await.is_err());
        assert_eq!(ask(actor, "started", &[]).await, [1]);
        assert_eq!(
            ask(actor, "self", &[]).await,
            format!("/user/guest-{index}").as_bytes()
        );
        assert_eq!(ask(actor, "counter", &[]).await, 0u64.to_le_bytes());
        actor
            .ask(WasmTell {
                type_key: "bump".into(),
                payload: vec![3 + index as u8],
            })
            .await
            .unwrap();
        assert_eq!(
            ask(actor, "counter", &[]).await,
            (3 + index as u64).to_le_bytes()
        );
        assert_eq!(ask(actor, "config", b"label").await, "中文".as_bytes());
        for key in ["limit", "items", "nested"] {
            let value = String::from_utf8(ask(actor, "config", key.as_bytes()).await).unwrap();
            let parsed: toml::Table = toml::from_str(&format!("value = {value}")).unwrap();
            assert_eq!(
                parsed["value"],
                manifest.components[0].config.as_ref().unwrap()[key]
            );
        }
    }
    context.drain(&deployer).await.unwrap();
    context.drain(&deployer).await.unwrap();
    for actor in context.component_refs("guest").unwrap() {
        let error = actor
            .ask(WasmAsk {
                type_key: "echo".into(),
                payload: vec![],
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("business admission is closed"),
            "{error}"
        );
        // Admission errors must not retire the actor or prevent STOP acknowledgement.
        let reply = actor
            .send(Box::new(parrot_app::assemble::ParrotAppHook(
                parrot_app::hook_phase::STOP,
            )))
            .await
            .unwrap();
        assert_eq!(
            reply
                .downcast_ref::<parrot_app::assemble::ParrotAppHook>()
                .unwrap()
                .0,
            parrot_app::hook_phase::STOP
        );
    }
    context.teardown(&deployer).await.unwrap();
    for path in ["/user/guest-0", "/user/guest-1"] {
        assert!(engine.get_actor_ref(path).is_none());
    }
    engine.shutdown_internal().await.unwrap();
}

// Business/lifecycle tests need finite headroom for OS preemption. The runtime
// suite separately proves epoch interruption with a controlled clock.
#[cfg(feature = "wasm")]
fn test_spawner(engine: std::sync::Arc<ThreadActorSystem>) -> ThreadPropsSpawner {
    ThreadPropsSpawner::new(engine).with_wasm_limits(
        parrot_wasm::WasmConfig {
            epoch_deadline: 1000,
            ..Default::default()
        },
        Default::default(),
    )
}

#[cfg(feature = "wasm")]
async fn ask(actor: &BoxedActorRef, key: &str, payload: &[u8]) -> Vec<u8> {
    use parrot_api::address::ActorRefExt;
    actor
        .ask(parrot_app::host::wasm::WasmAsk {
            type_key: key.into(),
            payload: payload.into(),
        })
        .await
        .unwrap()
        .0
}

#[cfg(not(feature = "wasm"))]
#[tokio::test]
async fn feature_disabled_rejects_wasm_before_spawning() {
    let manifest = manifest();
    let engine = ThreadActorSystem::shared(Default::default());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    let error = context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, DeployError::Artifact(ref text) if text.contains("wasm") && text.contains("not enabled")),
        "{error}"
    );
    assert!(context.started().is_empty());
    assert!(engine.get_actor_ref("/user/guest-0").is_none());
    engine.shutdown_internal().await.unwrap();
}

#[cfg(feature = "wasm")]
mod enabled {
    use super::*;
    use parrot_app::{assemble::ParrotSpawner, manifest::ArtifactRef};
    use parrot_wasm::{runtime::CompilationLimits, WasmConfig};
    use sha2::{Digest, Sha256};
    use std::{sync::Arc, time::Duration};

    fn traced_artifact() -> ArtifactRef {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../parrot-wasm/tests/fixtures/trace-component.wasm")
            .canonicalize()
            .unwrap();
        ArtifactRef::Wasm {
            digest: format!("sha256:{:x}", Sha256::digest(std::fs::read(&path).unwrap())),
            uri: url::Url::from_file_path(path).unwrap().into(),
        }
    }

    #[tokio::test]
    async fn wasm_guest_receives_exact_handler_parent_and_isolates_untraced_calls() {
        tokio::time::timeout(Duration::from_secs(30), async {
            use parrot_api::{
                address::ActorRefExt,
                trace::{self, SpanKind, SpanStatus, TraceRecorder},
            };
            use parrot_app::host::wasm::WasmAsk;
            let engine = ThreadActorSystem::shared(Default::default());
            let spawner = test_spawner(engine.clone());
            let actors = spawner
                .spawn_artifact(&traced_artifact(), &["/user/wasm-traced".into()], None)
                .await
                .unwrap();
            let actor = &actors[0];
            let recorder = TraceRecorder::new(16).unwrap();
            let first = recorder.root();
            let second = recorder.root();
            let unrelated = TraceRecorder::new(4).unwrap();
            assert!(ask(actor, "start-parent", &[]).await.is_empty());
            for parent in [Some(&first), None, Some(&second), Some(&first), None] {
                let before = recorder.snapshot().records.len();
                let operation = trace::in_scope(parent.cloned(), || {
                    actor.ask(WasmAsk {
                        type_key: "parent".into(),
                        payload: vec![],
                    })
                });
                let observed = unrelated.root().scope(operation).await.unwrap().0;
                let snapshot = recorder.snapshot();
                if let Some(parent) = parent {
                    assert_eq!(snapshot.records.len(), before + 1);
                    let record = &snapshot.records[before];
                    assert_eq!(record.kind, SpanKind::DynamicHandler);
                    assert_eq!(record.status, SpanStatus::Ok);
                    assert_eq!(record.trace_id, parent.trace_id());
                    assert_eq!(record.parent_span_id, Some(parent.span_id()));
                    assert_ne!(record.span_id, parent.span_id());
                    assert_eq!(
                        observed,
                        [record.trace_id.as_slice(), record.span_id.as_slice()].concat(),
                        "Wasm guest must receive the actual host handler as parent"
                    );
                } else {
                    assert!(
                        observed.is_empty(),
                        "untraced Wasm inherited a previous or polling context"
                    );
                    assert_eq!(snapshot.records.len(), before);
                }
                assert!(unrelated.snapshot().records.is_empty());
            }
            assert!(parrot_api::trace::TraceContext::current().is_none());
            engine.shutdown_internal().await.unwrap();
        })
        .await
        .expect("Wasm ASK causality exceeded its liveness deadline");
    }

    #[tokio::test]
    async fn wasm_tells_deliver_independent_parents_to_real_sibling_instances() {
        tokio::time::timeout(Duration::from_secs(30), async {
            use parrot_api::trace::{SpanStatus, TraceRecorder};
            use parrot_app::host::wasm::WasmTell;
            let engine = ThreadActorSystem::shared(Default::default());
            let spawner = test_spawner(engine.clone());
            let actors = spawner
                .spawn_artifact(
                    &traced_artifact(),
                    &["/user/wasm-a".into(), "/user/wasm-b".into()],
                    None,
                )
                .await
                .unwrap();
            let recorder = TraceRecorder::new(16).unwrap();
            let roots = [recorder.root(), recorder.root()];
            let (a, b) = tokio::join!(
                roots[0].in_scope(|| actors[0].deliver(Box::new(WasmTell {
                    type_key: "remember".into(),
                    payload: vec![],
                }))),
                roots[1].in_scope(|| actors[1].deliver(Box::new(WasmTell {
                    type_key: "remember".into(),
                    payload: vec![],
                }))),
            );
            a.unwrap();
            b.unwrap();
            let mut observations = Vec::new();
            for actor in &actors {
                observations.push(ask(actor, "saved-parent", &[]).await);
                assert!(ask(actor, "parent", &[]).await.is_empty());
            }
            let snapshot = recorder.snapshot();
            assert_eq!(snapshot.records.len(), 2);
            for (index, root) in roots.iter().enumerate() {
                let records: Vec<_> = snapshot
                    .records
                    .iter()
                    .filter(|record| record.trace_id == root.trace_id())
                    .collect();
                assert_eq!(records.len(), 1);
                let record = records[0];
                assert_eq!(record.status, SpanStatus::Ok);
                assert_eq!(record.parent_span_id, Some(root.span_id()));
                assert_eq!(
                    record.target,
                    format!("/user/wasm-{}", if index == 0 { "a" } else { "b" })
                );
                assert_eq!(
                    observations[index],
                    [record.trace_id.as_slice(), record.span_id.as_slice()].concat()
                );
            }
            engine.shutdown_internal().await.unwrap();
        })
        .await
        .expect("Wasm TELL causality exceeded its liveness deadline");
    }

    #[test]
    fn wasm_initialization_uses_engine_runtime_without_ambient_tokio() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let engine = runtime.block_on(async { ThreadActorSystem::shared(Default::default()) });
        let spawner = test_spawner(engine.clone());
        let artifact = manifest().components.remove(0).artifact;
        let actors = futures::executor::block_on(spawner.spawn_artifact(
            &artifact,
            &["/user/no-ambient".into()],
            None,
        ))
        .unwrap();
        runtime.block_on(async {
            assert_eq!(
                ask(&actors[0], "echo", b"engine runtime").await,
                b"engine runtime"
            );
            actors[0].stop().await.unwrap();
            engine.shutdown_internal().await.unwrap();
        });
    }

    #[test]
    fn drain_executes_guest_once_and_closes_business_admission() {
        use parrot_app::host::wasm::{WasmActor, WasmAsk};
        let runtime = parrot_wasm::runtime::WasmRuntime::new(WasmConfig {
            epoch_deadline: 1000,
            ..Default::default()
        })
        .unwrap();
        let component = runtime.instantiate(&fixture(), Default::default()).unwrap();
        let mut actor = WasmActor::new(component).unwrap();
        let admission = actor.message_admission();
        assert!(admission
            .prepare(Box::new(WasmAsk {
                type_key: "echo".into(),
                payload: vec![]
            }))
            .is_ok());
        actor.take_metrics();
        actor.drain().unwrap();
        assert!(
            actor.take_metrics().fuel_consumed > 0,
            "drain never entered the real guest"
        );
        actor.drain().unwrap();
        assert_eq!(
            actor.take_metrics().fuel_consumed,
            0,
            "duplicate drain executed guest code"
        );
        assert!(admission
            .prepare(Box::new(WasmAsk {
                type_key: "echo".into(),
                payload: vec![]
            }))
            .is_err());
        assert!(admission
            .prepare(Box::new(parrot_app::assemble::ParrotAppHook(
                parrot_app::hook_phase::STOP
            )))
            .is_ok());
    }

    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "parrot-wasm-app-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn artifact(&self, bytes: &[u8]) -> ArtifactRef {
            let path = self.0.join("component with spaces.wasm");
            std::fs::write(&path, bytes).unwrap();
            ArtifactRef::Wasm {
                digest: format!("sha256:{:x}", Sha256::digest(bytes)),
                uri: url::Url::from_file_path(path).unwrap().into(),
            }
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn invalid_artifact_metadata_is_rejected_before_any_component_starts() {
        let engine = ThreadActorSystem::shared(Default::default());
        let spawner = test_spawner(engine.clone());
        let deployer = LocalDeployer {
            topology: &LocalTopology,
            gateway_factory: &NoGateway,
            parrot_spawner: &spawner,
        };
        for (digest, uri) in [
            ("", "file:///tmp/x"),
            ("sha256:abc", "file:///tmp/x"),
            (
                include_str!("support/wasm-fixture.sha256").trim(),
                "https://example.com/a.wasm",
            ),
            (
                include_str!("support/wasm-fixture.sha256").trim(),
                "file:relative.wasm",
            ),
            (
                include_str!("support/wasm-fixture.sha256").trim(),
                "file:///tmp/x?query",
            ),
            (
                include_str!("support/wasm-fixture.sha256").trim(),
                "file:///tmp/x#fragment",
            ),
        ] {
            let mut manifest = manifest();
            let mut invalid = manifest.components[0].clone();
            invalid.name = "invalid".into();
            invalid.deps = vec!["guest".into()];
            invalid.artifact = ArtifactRef::Wasm {
                digest: digest.into(),
                uri: uri.into(),
            };
            manifest.components.push(invalid);
            let mut context = AssemblingContext::new();
            assert!(context
                .assemble(
                    &plan(&manifest, &LocalTopology).unwrap(),
                    &deployer,
                    &manifest,
                    &Default::default()
                )
                .await
                .is_err());
            assert!(context.started().is_empty());
            assert!(engine.get_actor_ref("/user/guest-0").is_none());
        }
        engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn content_digest_compile_and_sandbox_limits_fail_without_leaking_actors() {
        let engine = ThreadActorSystem::shared(Default::default());
        let temp = Temp::new();
        let paths = vec!["/user/rejected".into()];
        let good = manifest().components.remove(0).artifact;
        let mut wrong = good.clone();
        if let ArtifactRef::Wasm { digest, .. } = &mut wrong {
            *digest = "0".repeat(64);
        }
        let cases = [
            (
                wrong,
                WasmConfig::default(),
                CompilationLimits::default(),
                "digest mismatch",
            ),
            (
                temp.artifact(b"not a component"),
                WasmConfig::default(),
                CompilationLimits::default(),
                "compile",
            ),
            (
                good.clone(),
                WasmConfig::default(),
                CompilationLimits {
                    max_input_bytes: 8,
                    ..Default::default()
                },
                "byte limit",
            ),
            (
                good.clone(),
                WasmConfig {
                    memory_limit_mb: 1,
                    ..Default::default()
                },
                CompilationLimits::default(),
                "instantiate",
            ),
            (
                good.clone(),
                WasmConfig {
                    fuel_per_message: 1,
                    ..Default::default()
                },
                CompilationLimits::default(),
                "wasm on_start",
            ),
            (
                good,
                WasmConfig::default(),
                CompilationLimits {
                    max_input_bytes: 0,
                    ..Default::default()
                },
                "max_input_bytes",
            ),
        ];
        for (artifact, mut sandbox, limits, expected) in cases {
            sandbox.epoch_deadline = 1000;
            let spawner = test_spawner(engine.clone()).with_wasm_limits(sandbox, limits);
            let error = match spawner.spawn_artifact(&artifact, &paths, None).await {
                Ok(_) => panic!("accepted {expected}"),
                Err(e) => e,
            };
            assert!(
                error.to_string().contains(expected),
                "expected {expected}: {error}"
            );
            assert!(engine.get_actor_ref(&paths[0]).is_none());
        }
        engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn pool_failure_rolls_back_original_instances_and_preserves_path_owner() {
        let engine = ThreadActorSystem::shared(Default::default());
        let spawner = test_spawner(engine.clone());
        let artifact = manifest().components.remove(0).artifact;
        let existing = spawner
            .spawn_artifact(&artifact, &["/user/conflict-1".into()], None)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(ask(&existing, "state", &[]).await, 1u64.to_le_bytes());
        assert!(spawner
            .spawn_artifact(
                &artifact,
                &[
                    "/user/conflict-0".into(),
                    "/user/conflict-1".into(),
                    "/user/conflict-2".into()
                ],
                None
            )
            .await
            .is_err());
        assert!(engine.get_actor_ref("/user/conflict-0").is_none());
        assert!(engine.get_actor_ref("/user/conflict-2").is_none());
        assert_eq!(ask(&existing, "state", &[]).await, 2u64.to_le_bytes());
        existing.stop().await.unwrap();
        engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn later_component_content_failure_rolls_back_prior_wasm_pool() {
        let mut manifest = manifest();
        let temp = Temp::new();
        let mut broken = manifest.components[0].clone();
        broken.name = "broken".into();
        broken.deps = vec!["guest".into()];
        broken.artifact = temp.artifact(b"broken");
        manifest.components.push(broken);
        let engine = ThreadActorSystem::shared(Default::default());
        let spawner = test_spawner(engine.clone());
        let deployer = LocalDeployer {
            topology: &LocalTopology,
            gateway_factory: &NoGateway,
            parrot_spawner: &spawner,
        };
        let mut context = AssemblingContext::new();
        let error = context
            .assemble(
                &plan(&manifest, &LocalTopology).unwrap(),
                &deployer,
                &manifest,
                &Default::default(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("compile"), "{error}");
        assert!(context.started().is_empty());
        for path in ["/user/guest-0", "/user/guest-1", "/user/broken-0"] {
            assert!(engine.get_actor_ref(path).is_none());
        }
        engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_unclaimed_wasm_pool_is_retired_on_engine_runtime() {
        let engine = ThreadActorSystem::shared(Default::default());
        let spawner = test_spawner(engine.clone());
        let artifact = manifest().components.remove(0).artifact;
        let paths = vec!["/user/unclaimed-0".into(), "/user/unclaimed-1".into()];
        let mut pending = Box::pin(spawner.spawn_artifact(&artifact, &paths, None));
        assert!(futures::poll!(&mut pending).is_pending());
        tokio::time::timeout(Duration::from_secs(30), async {
            while engine.get_actor_ref(&paths[1]).is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Never poll the result again: dropping a buffered handoff is cancellation.
        drop(pending);
        tokio::time::timeout(Duration::from_secs(10), async {
            while paths.iter().any(|p| engine.get_actor_ref(p).is_some()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        engine.shutdown_internal().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_and_directory_artifacts_are_rejected_without_blocking_executor() {
        let temp = Temp::new();
        let fifo = temp.0.join("pipe");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let engine = ThreadActorSystem::shared(Default::default());
        let spawner = test_spawner(engine.clone());
        for path in [&fifo, &temp.0] {
            let artifact = ArtifactRef::Wasm {
                digest: "0".repeat(64),
                uri: url::Url::from_file_path(path).unwrap().into(),
            };
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                spawner.spawn_artifact(&artifact, &["/user/special".into()], None),
            )
            .await
            .expect("special file open blocked");
            let error = match result {
                Ok(_) => panic!("special file accepted"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("regular file"), "{error}");
        }
        engine.shutdown_internal().await.unwrap();
    }

    #[test]
    fn wire_codecs_match_external_bytes_and_reject_trailing_data() {
        use parrot_api::types::BoxedMessage;
        use parrot_app::host::wasm::{WasmAsk, WasmReply, WasmTell};
        let registry = parrot_remote::codec_registry::CodecRegistry::global();
        for (key, value, expected) in [
            (
                "bin:parrot.wasm.Ask#v1",
                Box::new(WasmAsk {
                    type_key: "echo".into(),
                    payload: vec![0, 255],
                }) as BoxedMessage,
                vec![4, b'e', b'c', b'h', b'o', 2, 0, 255],
            ),
            (
                "bin:parrot.wasm.Tell#v1",
                Box::new(WasmTell {
                    type_key: "bump".into(),
                    payload: vec![3],
                }) as BoxedMessage,
                vec![4, b'b', b'u', b'm', b'p', 1, 3],
            ),
            (
                "bin:parrot.wasm.Reply#v1",
                Box::new(WasmReply(vec![0, 255])) as BoxedMessage,
                vec![2, 0, 255],
            ),
        ] {
            let (actual_key, bytes) = registry.encode_outgoing(&value).unwrap();
            assert_eq!(actual_key, key);
            assert_eq!(bytes.as_ref(), expected);
            assert!(registry.decode_incoming(key, &expected).is_ok());
            let mut suffixed = expected;
            suffixed.push(0);
            assert!(registry.decode_incoming(key, &suffixed).is_err());
        }
    }

    #[tokio::test]
    async fn actual_cli_digest_failure_exits_nonzero_without_readiness() {
        let temp = Temp::new();
        let mut app = manifest();
        if let ArtifactRef::Wasm { digest, .. } = &mut app.components[0].artifact {
            *digest = "0".repeat(64);
        }
        let path = temp.0.join("application.toml");
        let readiness = temp.0.join("ready");
        std::fs::write(&path, toml::to_string(&app).unwrap()).unwrap();
        let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"))
            .current_dir(&temp.0)
            .env_remove("PARROT_CONFIG")
            .env("PARROT_APP_READY_FILE", &readiness)
            .args(["run", "--manifest"])
            .arg(path)
            .args(["--bind", "127.0.0.1:0"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("SHA-256 digest mismatch"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("PARROT_APP_READY="));
        assert!(!readiness.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actual_cli_serves_wasm_over_tcp_and_exits_cleanly_on_sigterm() {
        use parrot_api::address::ActorRefExt;
        use parrot_app::host::{
            wasm::{WasmAsk, WasmTell},
            HostLookup,
        };
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let temp = Temp::new();
        let mut app = manifest();
        // Exercise URI percent decoding as well as a packaged binary fixture.
        app.components[0].artifact = temp.artifact(&std::fs::read(fixture()).unwrap());
        let manifest_path = temp.0.join("application.toml");
        let readiness = temp.0.join("ready");
        std::fs::write(&manifest_path, toml::to_string(&app).unwrap()).unwrap();
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"))
            .current_dir(&temp.0)
            .env_remove("PARROT_CONFIG")
            .env("PARROT_APP_READY_FILE", &readiness)
            .args(["run", "--manifest"])
            .arg(manifest_path)
            .args(["--bind", "127.0.0.1:0"])
            // This is a wiring/fuel contract, not a 10ms scheduling benchmark.
            // Preserve finite epoch enforcement with an explicit generous budget.
            .args(["--wasm-epoch-ticks", "1000"])
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
                    "CLI exited before readiness"
                );
                if let Some(value) = line.strip_prefix("PARROT_APP_READY=") {
                    break value.trim().to_owned();
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(readiness).unwrap(), endpoint);
        let engine = ThreadActorSystem::shared(Default::default());
        let client = parrot_remote::RemoteActorSystem::new(
            parrot_remote::RemoteConfig::tcp("wasm-probe", None),
            Arc::new(HostLookup { ts: engine.clone() }),
        )
        .unwrap();
        client.start().await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            client.connect(&parrot_remote::NodeAddr::tcp(
                "app-host",
                endpoint.parse().unwrap(),
            )),
        )
        .await
        .unwrap()
        .unwrap();
        let actor: BoxedActorRef =
            Box::new(client.remote_ref("parrot://app-host/user/guest-0").unwrap());
        assert_eq!(ask(&actor, "echo", &[0, 255]).await, [0, 255]);
        assert_eq!(ask(&actor, "config", b"label").await, "中文".as_bytes());
        actor
            .ask(WasmTell {
                type_key: "bump".into(),
                payload: vec![9],
            })
            .await
            .unwrap();
        assert_eq!(ask(&actor, "counter", &[]).await, 9u64.to_le_bytes());
        // Fuel errors travel across the same wire and preserve the default
        // supervision policy: a dynamically created guest is retired on failure.
        let failed: BoxedActorRef =
            Box::new(client.remote_ref("parrot://app-host/user/guest-1").unwrap());
        let error = failed
            .ask(WasmAsk {
                type_key: "spin".into(),
                payload: vec![],
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("OverQuota"), "{error}");
        assert_eq!(
            ask(&actor, "echo", b"other instance lives").await,
            b"other instance lives"
        );
        assert!(tokio::process::Command::new("kill")
            .args(["-TERM", &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success());
        let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "CLI exit: {status}");
        client.shutdown().await.unwrap();
        engine.shutdown_internal().await.unwrap();
        assert!(
            tokio::net::TcpListener::bind(&endpoint).await.is_ok(),
            "listener leaked after CLI exit"
        );
    }
}

#[tokio::test]
async fn cli_epoch_budget_rejects_invalid_or_unavailable_configuration() {
    for value in ["0", "4294967296", "-1", "not-a-number"] {
        let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"))
            .args([
                "run",
                "--manifest",
                "/not-read.toml",
                "--wasm-epoch-ticks",
                value,
            ])
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("--wasm-epoch-ticks"));
        assert!(!String::from_utf8_lossy(&result.stdout).contains("PARROT_APP_READY="));
    }
    if !cfg!(feature = "wasm") {
        let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"))
            .args([
                "run",
                "--manifest",
                "/not-read.toml",
                "--wasm-epoch-ticks",
                "1000",
            ])
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("requires the wasm feature"));
    }
}
