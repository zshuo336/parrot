#![cfg(feature = "host")]

#[tokio::test]
async fn cli_native_trust_flag_rejects_malformed_or_unavailable_authorization() {
    let mut values = vec!["".to_string(), "xyz".into(), "g".repeat(64)];
    if !cfg!(feature = "dylib") {
        values.push("0".repeat(64));
    }
    for value in values {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"))
            .args([
                "run",
                "--manifest",
                "/not-read.toml",
                "--trust-native-sha256",
                &value,
            ])
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("--trust-native-sha256"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("PARROT_APP_READY="));
    }
}

#[cfg(not(feature = "dylib"))]
#[tokio::test]
async fn native_runtime_is_explicitly_feature_gated() {
    use parrot_app::{assemble::ParrotSpawner, host::ThreadPropsSpawner, manifest::ArtifactRef};
    let engine = parrot::thread::system::ThreadActorSystem::shared(Default::default());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let error = spawner
        .spawn_artifact(
            &ArtifactRef::Dylib {
                digest: "0".repeat(64),
                uri: "file:///missing.so".into(),
                abi: 1,
            },
            &["/user/disabled".into()],
            None,
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("runtime is not enabled"),
        "{error}"
    );
    engine.shutdown_internal().await.unwrap();
}

#[cfg(feature = "dylib")]
mod enabled {
    use parrot::thread::system::ThreadActorSystem;
    use parrot_api::{address::ActorRefExt, types::BoxedActorRef};
    use parrot_app::{
        assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner},
        host::{
            dylib::{DylibAsk, DylibTell, MAX_MESSAGE_BYTES, MAX_WORKERS},
            ThreadPropsSpawner,
        },
        manifest::{AppManifest, ArtifactRef, ComponentSpec},
        planner::{plan, LocalTopology},
    };
    use sha2::{Digest, Sha256};
    use std::{
        path::PathBuf,
        sync::{Arc, OnceLock},
        time::Duration,
    };

    fn fixture() -> &'static PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();
        PATH.get_or_init(|| {
            let root =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../parrot-abi/tests/fixtures");
            let target = root.join("target");
            let output = std::process::Command::new("cargo")
                .args(["build", "--locked", "--release", "--target-dir"])
                .arg(&target)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture build: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            target
                .join("release")
                .join(format!(
                    "libparrot_abi_hostcomp.{}",
                    std::env::consts::DLL_EXTENSION
                ))
                .canonicalize()
                .unwrap()
        })
    }
    struct Setup {
        temp: tempfile::TempDir,
        engine: Arc<ThreadActorSystem>,
        spawner: ThreadPropsSpawner,
        artifact: ArtifactRef,
        config: toml::Table,
    }
    impl Setup {
        fn new() -> Self {
            Self::with_timeout(Duration::from_secs(10))
        }
        fn with_timeout(shutdown_timeout: Duration) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let path = temp
                .path()
                .join(format!("native 中文.{}", std::env::consts::DLL_EXTENSION));
            std::fs::copy(fixture(), &path).unwrap();
            let digest = format!("sha256:{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
            let artifact = ArtifactRef::Dylib {
                digest: digest.clone(),
                uri: url::Url::from_file_path(&path).unwrap().into(),
                abi: 1,
            };
            let engine =
                ThreadActorSystem::shared(parrot::thread::config::ThreadActorSystemConfig {
                    shutdown_timeout,
                    ..Default::default()
                });
            // SAFETY: this test owns and builds the fixture and all its dependencies.
            // It implements the documented non-unwinding, thread-independent ABI.
            let spawner =
                unsafe { ThreadPropsSpawner::new(engine.clone()).with_trusted_dylib(&digest) }
                    .unwrap();
            let mut config: toml::Table =
                toml::from_str("label = '中文'\nitems = [1, 2]\n[inside]\nenabled = true").unwrap();
            config.insert(
                "trace".into(),
                toml::Value::String(temp.path().join("events").to_str().unwrap().into()),
            );
            Self {
                temp,
                engine,
                spawner,
                artifact,
                config,
            }
        }
        async fn spawn(&self, paths: &[&str]) -> Result<Vec<BoxedActorRef>, DeployError> {
            self.spawner
                .spawn_artifact(
                    &self.artifact,
                    &paths.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                    Some(&self.config),
                )
                .await
        }
        fn events(&self) -> String {
            std::fs::read_to_string(self.temp.path().join("events")).unwrap_or_default()
        }
        async fn wait_for(&self, event: &str) {
            tokio::time::timeout(Duration::from_secs(15), async {
                while !self.events().lines().any(|line| line == event) {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("missing event {event}: {}", self.events()));
        }
        fn gate(&mut self, key: &str) -> PathBuf {
            let path = self.temp.path().join(key);
            self.config.insert(
                key.into(),
                toml::Value::String(path.to_str().unwrap().into()),
            );
            path
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

    #[tokio::test]
    async fn native_guest_receives_exact_handler_parent_and_isolates_untraced_calls() {
        tokio::time::timeout(Duration::from_secs(30), async {
            use parrot_api::trace::{self, SpanKind, SpanStatus, TraceRecorder};
            let setup = Setup::new();
            let actors = setup.spawn(&["/user/traced"]).await.unwrap();
            let actor = &actors[0];
            let recorder = TraceRecorder::new(16).unwrap();
            let first = recorder.root();
            let second = recorder.root();
            let unrelated = TraceRecorder::new(4).unwrap();
            for parent in [Some(&first), None, Some(&second), Some(&first), None] {
                let before = recorder.snapshot().records.len();
                let operation = trace::in_scope(parent.cloned(), || {
                    actor.ask(DylibAsk {
                        type_key: "trace-parent".into(),
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
                        "guest must receive the actual host handler as parent"
                    );
                } else {
                    assert!(
                        observed.is_empty(),
                        "untraced call inherited a prior or polling context"
                    );
                    assert_eq!(snapshot.records.len(), before);
                }
                assert!(unrelated.snapshot().records.is_empty());
            }
            assert!(parrot_api::trace::TraceContext::current().is_none());
            setup.engine.shutdown_internal().await.unwrap();
        })
        .await
        .expect("native ASK causality exceeded its liveness deadline");
    }

    #[tokio::test]
    async fn native_tells_deliver_independent_parents_to_real_sibling_instances() {
        tokio::time::timeout(Duration::from_secs(30), async {
            use parrot_api::trace::{SpanStatus, TraceRecorder};
            let setup = Setup::new();
            let actors = setup
                .spawn(&["/user/trace-a", "/user/trace-b"])
                .await
                .unwrap();
            let recorder = TraceRecorder::new(16).unwrap();
            let roots = [recorder.root(), recorder.root()];
            let (a, b) = tokio::join!(
                roots[0].in_scope(|| actors[0].deliver(Box::new(DylibTell {
                    type_key: "trace-parent-set".into(),
                    payload: vec![]
                }))),
                roots[1].in_scope(|| actors[1].deliver(Box::new(DylibTell {
                    type_key: "trace-parent-set".into(),
                    payload: vec![]
                }))),
            );
            a.unwrap();
            b.unwrap();
            let mut observations = Vec::new();
            for actor in &actors {
                observations.push(ask(actor, "saved-parent", &[]).await);
                assert!(ask(actor, "trace-parent", &[]).await.is_empty());
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
                    format!("/user/trace-{}", if index == 0 { "a" } else { "b" })
                );
                assert_eq!(
                    observations[index],
                    [record.trace_id.as_slice(), record.span_id.as_slice()].concat()
                );
            }
            setup.engine.shutdown_internal().await.unwrap();
        })
        .await
        .expect("native TELL causality exceeded its liveness deadline");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_native_ask_keeps_owned_parent_until_return_without_leaking_to_next_call() {
        tokio::time::timeout(Duration::from_secs(30), async {
            use parrot_api::trace::{SpanStatus, TraceRecorder};
            let mut setup = Setup::new();
            let gate = setup.gate("message_gate");
            let actors = setup.spawn(&["/user/trace-cancel"]).await.unwrap();
            let recorder = TraceRecorder::new(8).unwrap();
            let root = recorder.root();
            let actor = actors[0].clone_boxed();
            let context = root.clone();
            let task = tokio::spawn(async move {
                context
                    .in_scope(|| {
                        actor.ask(DylibAsk {
                            type_key: "trace-gate".into(),
                            payload: vec![],
                        })
                    })
                    .await
            });
            setup.wait_for("message-enter:/user/trace-cancel").await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            let pending = recorder.snapshot();
            assert!(pending.records.is_empty());
            assert_eq!(pending.counts.started, 1);
            assert_eq!(
                pending.counts.in_flight, 1,
                "native work still owns its handler context"
            );
            std::fs::write(gate, b"open").unwrap();
            let observed = ask(&actors[0], "saved-parent", &[]).await;
            let snapshot = recorder.snapshot();
            assert_eq!(snapshot.records.len(), 1);
            let record = &snapshot.records[0];
            assert_eq!(record.trace_id, root.trace_id());
            assert_eq!(
                record.status,
                SpanStatus::Ok,
                "native work still completes after the caller leaves"
            );
            assert_eq!(record.parent_span_id, Some(root.span_id()));
            assert_eq!(
                observed,
                [record.trace_id.as_slice(), record.span_id.as_slice()].concat()
            );
            assert!(ask(&actors[0], "trace-parent", &[]).await.is_empty());
            setup.engine.shutdown_internal().await.unwrap();
        })
        .await
        .expect("native cancelled ASK causality exceeded its liveness deadline");
    }

    #[tokio::test]
    async fn manifest_pool_uses_real_native_state_config_hooks_and_teardown() {
        let setup = Setup::new();
        let manifest = app_manifest(&setup);
        let deployer = LocalDeployer {
            topology: &LocalTopology,
            gateway_factory: &NoGateway,
            parrot_spawner: &setup.spawner,
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
        for (index, actor) in context.component_refs("guest").unwrap().iter().enumerate() {
            assert_eq!(ask(actor, "messages", &[]).await, 1u64.to_le_bytes());
            let envelope: toml::Table =
                toml::from_str(std::str::from_utf8(&ask(actor, "config", &[]).await).unwrap())
                    .unwrap();
            assert_eq!(
                envelope["self_path"].as_str(),
                Some(format!("/user/guest-{index}").as_str())
            );
            assert_eq!(envelope["config"].as_table().unwrap(), &setup.config);
            actor
                .deliver(Box::new(DylibTell {
                    type_key: "echo".into(),
                    payload: vec![1],
                }))
                .await
                .unwrap();
            assert_eq!(ask(actor, "messages", &[]).await, 4u64.to_le_bytes());
        }
        context.drain(&deployer).await.unwrap();
        context.drain(&deployer).await.unwrap();
        for actor in context.component_refs("guest").unwrap() {
            let error = actor
                .ask(DylibAsk {
                    type_key: "echo".into(),
                    payload: vec![],
                })
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("business admission is closed"),
                "{error}"
            );
            assert!(actor.is_alive().await);
        }
        context.teardown(&deployer).await.unwrap();
        let events = setup.events();
        for path in ["/user/guest-0", "/user/guest-1"] {
            assert_eq!(
                events
                    .lines()
                    .filter(|line| *line == format!("drain:{path}"))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .lines()
                    .filter(|line| *line == format!("destroy:{path}"))
                    .count(),
                1
            );
            assert!(setup.engine.get_actor_ref(path).is_none());
        }
        assert!(
            !events.contains("duplicate") && !events.contains("without-drain"),
            "{events}"
        );
        setup.engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn trust_metadata_and_byte_validation_precede_native_execution() {
        let setup = Setup::new();
        let untrusted = ThreadPropsSpawner::new(setup.engine.clone());
        let error = untrusted
            .spawn_artifact(
                &setup.artifact,
                &["/user/untrusted".into()],
                Some(&setup.config),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("trust authorization"), "{error}");
        let ArtifactRef::Dylib { digest, uri, .. } = &setup.artifact else {
            unreachable!()
        };
        for artifact in [
            ArtifactRef::Dylib {
                digest: "".into(),
                uri: uri.clone(),
                abi: 1,
            },
            ArtifactRef::Dylib {
                digest: digest.clone(),
                uri: uri.clone(),
                abi: 2,
            },
            ArtifactRef::Dylib {
                digest: digest.clone(),
                uri: format!("{uri}?q"),
                abi: 1,
            },
            ArtifactRef::Dylib {
                digest: digest.clone(),
                uri: "https://example.com/x".into(),
                abi: 1,
            },
        ] {
            assert!(setup
                .spawner
                .spawn_artifact(&artifact, &["/user/invalid".into()], Some(&setup.config))
                .await
                .is_err());
        }
        std::fs::write(
            url::Url::parse(uri).unwrap().to_file_path().unwrap(),
            b"replaced input",
        )
        .unwrap();
        let error = setup.spawn(&["/user/replaced"]).await.unwrap_err();
        assert!(
            matches!(error.primary_error(), DeployError::ArtifactDigest(_)),
            "{error}"
        );
        assert_eq!(setup.events(), "");
        setup.engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn pool_failure_retires_prior_instances_in_reverse_order() {
        let mut setup = Setup::new();
        setup.config.insert(
            "fail_path".into(),
            toml::Value::String("/user/pool-2".into()),
        );
        assert!(setup
            .spawn(&["/user/pool-0", "/user/pool-1", "/user/pool-2"])
            .await
            .is_err());
        assert_eq!(
            setup
                .events()
                .lines()
                .filter(|l| l.starts_with("destroy:"))
                .collect::<Vec<_>>(),
            ["destroy:/user/pool-1", "destroy:/user/pool-0"]
        );
        for path in ["/user/pool-0", "/user/pool-1", "/user/pool-2"] {
            assert!(setup.engine.get_actor_ref(path).is_none());
        }
        setup.engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn occupied_path_preserves_owner_and_destroys_unregistered_instance() {
        let setup = Setup::new();
        let original = setup.spawn(&["/user/same"]).await.unwrap().pop().unwrap();
        assert!(setup.spawn(&["/user/first", "/user/same"]).await.is_err());
        assert_eq!(ask(&original, "echo", b"still here").await, b"still here");
        assert!(setup.engine.get_actor_ref("/user/first").is_none());
        assert_eq!(
            setup
                .events()
                .lines()
                .filter(|l| *l == "destroy:/user/same")
                .count(),
            1
        );
        original.stop().await.unwrap();
        assert_eq!(
            setup
                .events()
                .lines()
                .filter(|l| *l == "destroy:/user/same")
                .count(),
            2
        );
        setup.engine.shutdown_internal().await.unwrap();
    }

    #[test]
    fn cancelling_constructor_wait_does_not_lose_native_ownership() {
        // dlclose shares a process-wide lock with unrelated dlopen calls. Keep
        // this five-second ownership assertion independent of other tests'
        // artifact verification; concurrent loading/retirement has its own test.
        isolated_native(
            "enabled::cancelling_constructor_wait_does_not_lose_native_ownership",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(cancelled_constructor_contract());
            },
        );
    }

    async fn cancelled_constructor_contract() {
        let mut setup = Setup::new();
        let gate = setup.gate("construct_gate");
        let spawner = setup.spawner.clone();
        let artifact = setup.artifact.clone();
        let config = setup.config.clone();
        let job = tokio::spawn(async move {
            spawner
                .spawn_artifact(
                    &artifact,
                    &["/user/cancel-0".into(), "/user/cancel-1".into()],
                    Some(&config),
                )
                .await
        });
        setup.wait_for("construct-enter:/user/cancel-0").await;
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        std::fs::write(gate, b"release").unwrap();
        setup.wait_for("destroy:/user/cancel-0").await;
        // destroy is an event inside the native hook, before the worker receipt
        // and the engine's registry retirement. Observe that separate contract without
        // keeping the current-thread runtime continuously ready or hammering its registry.
        // Keep the original five-second bound; this observer must not initiate cleanup.
        let observed = tokio::time::timeout(Duration::from_secs(5), async {
            while setup.engine.get_actor_ref("/user/cancel-0").is_some() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
        if observed.is_err() {
            eprintln!(
                "CANCEL_REGISTRY_TIMEOUT events={:?} paths={:?} pending={:?}",
                setup.events(),
                setup.engine.actor_paths(),
                setup
                    .spawner
                    .pending_cleanup()
                    .iter()
                    .map(|actor| (&actor.path, &actor.error))
                    .collect::<Vec<_>>()
            );
            panic!("original five-second registry retirement deadline expired");
        }
        let events = setup.events();
        assert!(!events.contains("cancel-1"));
        for event in [
            "construct:/user/cancel-0",
            "drain:/user/cancel-0",
            "destroy:/user/cancel-0",
        ] {
            assert_eq!(
                events.lines().filter(|line| *line == event).count(),
                1,
                "native lifecycle must occur exactly once: {events}"
            );
        }
        assert!(
            !events.contains("duplicate-drain") && !events.contains("destroy-without-drain"),
            "{events}"
        );
        assert!(
            setup.spawner.pending_cleanup().is_empty(),
            "unexpected retained cleanup after cancellation"
        );
        assert!(setup.engine.get_actor_ref("/user/cancel-0").is_none());
        setup.engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn native_message_and_cleanup_gates_do_not_block_async_runtime() {
        let mut setup = Setup::new();
        let message_gate = setup.gate("message_gate");
        let drain_gate = setup.gate("drain_gate");
        let actor = setup.spawn(&["/user/gated"]).await.unwrap().pop().unwrap();
        let sender = actor.clone_boxed();
        let job = tokio::spawn(async move {
            sender
                .ask(DylibAsk {
                    type_key: "gate".into(),
                    payload: vec![],
                })
                .await
        });
        setup.wait_for("message-enter:/user/gated").await;
        // This test runs on one Tokio thread. Reaching here proves native work
        // did not occupy it. A sibling also performs real work concurrently.
        let sibling = setup
            .spawn(&["/user/sibling"])
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(ask(&sibling, "echo", b"responsive").await, b"responsive");
        let stopper = actor.clone_boxed();
        let stop = tokio::spawn(async move { stopper.stop().await });
        assert!(!setup.events().contains("drain-enter:/user/gated"));
        std::fs::write(message_gate, b"release").unwrap();
        job.await.unwrap().unwrap();
        setup.wait_for("drain-enter:/user/gated").await;
        stop.abort();
        assert!(stop.await.unwrap_err().is_cancelled());
        assert!(!setup.events().contains("destroy:/user/gated"));
        std::fs::write(drain_gate, b"release").unwrap();
        setup.wait_for("destroy:/user/gated").await;
        sibling.stop().await.unwrap();
        setup.engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn native_capacity_is_reserved_for_whole_pool_and_shared_by_clones() {
        let setup = Setup::new();
        let paths: Vec<_> = (0..MAX_WORKERS + 1)
            .map(|i| format!("/user/p-{i}"))
            .collect();
        let error = setup
            .spawner
            .spawn_artifact(&setup.artifact, &paths, Some(&setup.config))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("worker capacity"));
        assert!(setup.events().is_empty());
        let actors = setup
            .spawner
            .spawn_artifact(&setup.artifact, &paths[..MAX_WORKERS], Some(&setup.config))
            .await
            .unwrap();
        let error = setup
            .spawner
            .clone()
            .spawn_artifact(
                &setup.artifact,
                &["/user/overflow".into()],
                Some(&setup.config),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("worker capacity"));
        for actor in actors {
            actor.stop().await.unwrap();
        }
        let actor = setup.spawn(&["/user/reused"]).await.unwrap().pop().unwrap();
        actor.stop().await.unwrap();
        setup.engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn message_bounds_reject_without_invoking_native_code_or_retiring_actor() {
        let setup = Setup::new();
        let actor = setup
            .spawn(&["/user/bounded"])
            .await
            .unwrap()
            .pop()
            .unwrap();
        let error = actor
            .ask(DylibAsk {
                type_key: "echo".into(),
                payload: vec![0; MAX_MESSAGE_BYTES + 1],
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("byte limit"));
        assert!(actor.send(Box::new(())).await.is_err());
        assert_eq!(ask(&actor, "messages", &[]).await, 1u64.to_le_bytes());
        actor.stop().await.unwrap();
        setup.engine.shutdown_internal().await.unwrap();
    }

    fn app_manifest(setup: &Setup) -> AppManifest {
        AppManifest::from_toml(&format!("name='native-app'\nversion='1.0.0'\n[[components]]\nname='guest'\nengine='parrot'\ninstances={{Pool=2}}\nartifact={}\nconfig={}\n[components.hooks]\non_start='.'\non_drain='.'\non_stop='.'\n", toml::Value::try_from(&setup.artifact).unwrap(), toml::Value::Table(setup.config.clone()))).unwrap()
    }

    #[test]
    fn native_spawn_uses_engine_runtime_without_an_ambient_tokio_runtime() {
        isolated_native(
            "enabled::native_spawn_uses_engine_runtime_without_an_ambient_tokio_runtime",
            no_ambient_runtime_contract,
        );
    }

    fn no_ambient_runtime_contract() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let setup = runtime.block_on(async { Setup::new() });
        let actor = futures::executor::block_on(setup.spawn(&["/user/no-ambient"]))
            .unwrap()
            .pop()
            .unwrap();
        runtime.block_on(async {
            assert_eq!(
                ask(&actor, "echo", b"engine runtime").await,
                b"engine runtime"
            );
            actor.stop().await.unwrap();
            assert!(setup.events().contains("destroy:/user/no-ambient"));
            setup.engine.shutdown_internal().await.unwrap();
        });
    }

    #[tokio::test]
    async fn a_pool_keeps_one_verified_snapshot_when_the_source_changes_during_construction() {
        let mut setup = Setup::new();
        let gate = setup.gate("construct_gate");
        let spawner = setup.spawner.clone();
        let artifact = setup.artifact.clone();
        let config = setup.config.clone();
        let job = tokio::spawn(async move {
            spawner
                .spawn_artifact(
                    &artifact,
                    &["/user/snapshot-0".into(), "/user/snapshot-1".into()],
                    Some(&config),
                )
                .await
        });
        setup.wait_for("construct-enter:/user/snapshot-0").await;
        let ArtifactRef::Dylib { uri, .. } = &setup.artifact else {
            unreachable!()
        };
        std::fs::write(
            url::Url::parse(uri).unwrap().to_file_path().unwrap(),
            b"input changed after preparation",
        )
        .unwrap();
        std::fs::write(gate, b"release").unwrap();
        let actors = job.await.unwrap().unwrap();
        for actor in actors {
            assert_eq!(
                ask(&actor, "echo", b"verified bytes").await,
                b"verified bytes"
            );
            actor.stop().await.unwrap();
        }
        setup.engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn cli_requires_operator_authorization_and_rejects_digest_mismatch() {
        let setup = Setup::new();
        let path = setup.temp.path().join("app.toml");
        std::fs::write(&path, toml::to_string(&app_manifest(&setup)).unwrap()).unwrap();
        let ready = setup.temp.path().join("ready");
        for mismatch in [false, true] {
            let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"));
            cmd.current_dir(setup.temp.path())
                .env_remove("PARROT_CONFIG")
                .env("PARROT_APP_READY_FILE", &ready)
                .args(["run", "--manifest"])
                .arg(&path)
                .args(["--bind", "127.0.0.1:0"])
                .kill_on_drop(true);
            if mismatch {
                let ArtifactRef::Dylib { digest, uri, .. } = &setup.artifact else {
                    unreachable!()
                };
                cmd.args(["--trust-native-sha256", digest]);
                std::fs::write(
                    url::Url::parse(uri).unwrap().to_file_path().unwrap(),
                    b"mutated",
                )
                .unwrap();
            }
            let output = tokio::time::timeout(Duration::from_secs(30), cmd.output())
                .await
                .unwrap()
                .unwrap();
            assert!(!output.status.success());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error.contains(if mismatch {
                    "sha256 mismatch"
                } else {
                    "trust authorization"
                }),
                "{error}"
            );
            assert!(!ready.exists());
            assert!(!String::from_utf8_lossy(&output.stdout).contains("PARROT_APP_READY="));
            assert!(setup.events().is_empty());
        }
        setup.engine.shutdown_internal().await.unwrap();
    }

    async fn failure_retirement_before_drain(cleanup_fails: bool) {
        use futures::FutureExt;
        let mut setup = Setup::new();
        let gate = setup.gate("drain_gate");
        if cleanup_fails {
            setup
                .config
                .insert("drain_error".into(), toml::Value::String("yes".into()));
        }
        let mut manifest = app_manifest(&setup);
        manifest.components[0].instances = parrot_app::manifest::InstancePolicy::Pool(1);
        let deployer = LocalDeployer {
            topology: &LocalTopology,
            gateway_factory: &NoGateway,
            parrot_spawner: &setup.spawner,
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
        let failed = context.component_ref("guest").unwrap().clone_boxed();
        let sibling = setup
            .spawner
            .spawn_artifact(&setup.artifact, &["/user/sibling".into()], None)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let error = failed
            .ask(DylibAsk {
                type_key: "error".into(),
                payload: vec![],
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("native code 2"), "{error}");
        assert_eq!(
            ask(&sibling, "echo", b"sibling still alive").await,
            b"sibling still alive"
        );
        // A real native destructor path is now blocked. This proves retirement
        // has begun without depending on how fast SIGTERM races its hook send.
        setup.wait_for("drain-enter:/user/guest-0").await;
        assert!(!failed.is_alive().await);
        let mut drain = Box::pin(context.drain(&deployer));
        let early = drain.as_mut().now_or_never();
        let completed_before_release = early.is_some();
        let early_diagnostic = format!("{early:?}");
        let events_before_release = setup.events();
        // Always release the native worker, including the old-code counterexample.
        std::fs::write(gate, b"release").unwrap();
        let drained = match early {
            Some(result) => result,
            None => drain.as_mut().await,
        };
        drop(drain);
        let teardown = context.teardown(&deployer).await;
        sibling.stop().await.unwrap();
        let shutdown = setup.engine.shutdown_internal().await;
        assert!(
            !completed_before_release,
            "drain returned before native retirement: {early_diagnostic}; {events_before_release}"
        );
        assert!(!events_before_release.contains("destroy:/user/guest-0"));
        let events = setup.events();
        for phase in ["drain", "destroy"] {
            assert_eq!(
                events
                    .lines()
                    .filter(|line| *line == format!("{phase}:/user/guest-0"))
                    .count(),
                1,
                "{events}"
            );
        }
        assert!(
            !events.contains("duplicate-drain") && !events.contains("destroy-without-drain"),
            "{events}"
        );
        if cleanup_fails {
            assert!(drained
                .unwrap_err()
                .to_string()
                .contains("native drain failed with codes [2]"));
            assert!(teardown
                .unwrap_err()
                .to_string()
                .contains("native drain failed with codes [2]"));
            if let Err(error) = shutdown {
                assert!(
                    error
                        .to_string()
                        .contains("native drain failed with codes [2]"),
                    "{error}"
                );
            }
        } else {
            drained.unwrap();
            teardown.unwrap();
            shutdown.unwrap();
        }
    }

    #[tokio::test]
    async fn native_failure_retirement_is_joined_before_drain_success() {
        failure_retirement_before_drain(false).await;
    }

    #[tokio::test]
    async fn native_failure_retirement_error_is_not_a_successful_drain() {
        failure_retirement_before_drain(true).await;
    }

    #[cfg(unix)]
    async fn cli_native_shutdown(after_business_failure: bool) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let setup = Setup::new();
        let path = setup.temp.path().join("app.toml");
        std::fs::write(&path, toml::to_string(&app_manifest(&setup)).unwrap()).unwrap();
        let ArtifactRef::Dylib { digest, .. } = &setup.artifact else {
            unreachable!()
        };
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"))
            .current_dir(setup.temp.path())
            .env_remove("PARROT_CONFIG")
            .args(["run", "--manifest"])
            .arg(&path)
            .args(["--bind", "127.0.0.1:0", "--trust-native-sha256", digest])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut reader = tokio::io::BufReader::new(child.stdout.take().unwrap().take(65536));
        let endpoint = tokio::time::timeout(Duration::from_secs(30), async {
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
        let client = parrot_remote::RemoteActorSystem::new(
            parrot_remote::RemoteConfig::tcp("native-probe", None),
            Arc::new(parrot_app::host::HostLookup {
                ts: setup.engine.clone(),
            }),
        )
        .unwrap();
        client.start().await.unwrap();
        client
            .connect(&parrot_remote::NodeAddr::tcp(
                "app-host",
                endpoint.parse().unwrap(),
            ))
            .await
            .unwrap();
        let actor: BoxedActorRef =
            Box::new(client.remote_ref("parrot://app-host/user/guest-0").unwrap());
        assert_eq!(ask(&actor, "echo", &[0, 255]).await, [0, 255]);
        let config: toml::Table =
            toml::from_str(std::str::from_utf8(&ask(&actor, "config", &[]).await).unwrap())
                .unwrap();
        assert_eq!(config["config"].as_table(), Some(&setup.config));
        actor
            .ask(DylibTell {
                type_key: "echo".into(),
                payload: vec![3],
            })
            .await
            .unwrap();
        assert_eq!(ask(&actor, "messages", &[]).await, 4u64.to_le_bytes());
        if after_business_failure {
            let failed: BoxedActorRef =
                Box::new(client.remote_ref("parrot://app-host/user/guest-1").unwrap());
            let error = failed
                .ask(DylibAsk {
                    type_key: "error".into(),
                    payload: vec![],
                })
                .await
                .unwrap_err();
            assert!(error.to_string().contains("native code 2"), "{error}");
            // A business failure is not a graceful hook acknowledgement. Wait
            // for physical destruction and removal of the old published route
            // before asking the CLI to perform a successful normal shutdown.
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let result = failed
                        .ask(DylibAsk {
                            type_key: "echo".into(),
                            payload: vec![],
                        })
                        .await;
                    if matches!(
                        result,
                        Err(parrot_api::errors::ActorError::ActorNotFound(_))
                    ) && setup
                        .events()
                        .lines()
                        .any(|line| line == "destroy:/user/guest-1")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .expect("failed native instance must retire and withdraw its route");
            assert_eq!(
                ask(&actor, "echo", b"sibling still alive").await,
                b"sibling still alive"
            );
        }
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
        assert!(status.success(), "CLI exit {status}");
        for path in ["/user/guest-0", "/user/guest-1"] {
            assert_eq!(
                setup
                    .events()
                    .lines()
                    .filter(|l| *l == format!("destroy:{path}"))
                    .count(),
                1
            );
            assert_eq!(
                setup
                    .events()
                    .lines()
                    .filter(|l| *l == format!("drain:{path}"))
                    .count(),
                1
            );
        }
        client.shutdown().await.unwrap();
        setup.engine.shutdown_internal().await.unwrap();
        assert!(tokio::net::TcpListener::bind(&endpoint).await.is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_serves_real_native_messages_over_tcp_and_sigterm_waits_for_destroy() {
        cli_native_shutdown(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_native_business_failure_over_tcp_retires_before_successful_sigterm() {
        cli_native_shutdown(true).await;
    }

    // Loader operations share process-wide resources. Isolate timing-sensitive
    // contracts and deliberate stalls; bound and reap every owned subprocess.
    fn isolated_native(test: &str, body: impl FnOnce()) {
        if std::env::var("PARROT_NATIVE_CHILD").as_deref() == Ok(test) {
            body();
            return;
        }
        fixture();
        let log = tempfile::NamedTempFile::new().unwrap();
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                }
                let _ = self.0.wait();
            }
        }
        let output = log.reopen().unwrap();
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env("PARROT_NATIVE_CHILD", test)
                .stdout(output.try_clone().unwrap())
                .stderr(output)
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                let output = std::fs::read_to_string(log.path()).unwrap();
                assert!(status.success(), "{test}: {status}\n{output}");
                assert!(
                    output.contains("1 passed; 0 failed"),
                    "child did not execute contract: {output}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native subprocess exceeded deadline: {}",
                std::fs::read_to_string(log.path()).unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    struct UnloadProbe(Arc<[std::sync::atomic::AtomicU64; 9]>);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    impl UnloadProbe {
        fn new(setup: &mut Setup, block_constructor: bool) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            let probe = Self(Arc::new(std::array::from_fn(|_| AtomicU64::new(0))));
            probe.0[1].store(u64::from(!block_constructor), Ordering::SeqCst);
            let image = setup
                .temp
                .path()
                .join(format!("unload.{}", std::env::consts::DLL_EXTENSION));
            let source =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_unload.c");
            let output = std::process::Command::new("cc")
                .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-fPIC"])
                .arg(if cfg!(target_os = "macos") {
                    "-dynamiclib"
                } else {
                    "-shared"
                })
                .arg(source)
                .arg("-o")
                .arg(&image)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "C fixture: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let digest = format!(
                "sha256:{:x}",
                Sha256::digest(std::fs::read(&image).unwrap())
            );
            setup.artifact = ArtifactRef::Dylib {
                digest: digest.clone(),
                uri: url::Url::from_file_path(&image).unwrap().into(),
                abi: 1,
            };
            // SAFETY: this test builds the independent C fixture. The only
            // external storage is this owned AtomicU64 array; Drop releases
            // both gates and pins storage if the last callback is still pending.
            setup.spawner = unsafe {
                ThreadPropsSpawner::new(setup.engine.clone()).with_trusted_dylib(&digest)
            }
            .unwrap();
            setup.config.clear();
            setup.config.insert(
                "probe_address".into(),
                toml::Value::Integer(i64::try_from(probe.0.as_ptr() as usize).unwrap()),
            );
            probe
        }
        fn value(&self, slot: usize) -> u64 {
            self.0[slot].load(std::sync::atomic::Ordering::SeqCst)
        }
        fn release(&self, slot: usize) {
            self.0[slot].store(1, std::sync::atomic::Ordering::SeqCst);
        }
        async fn wait_for(&self, slot: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.value(slot) == 0 {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("native probe slot {slot} did not advance"));
        }
        async fn retired(&self, setup: &Setup) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while setup.engine.get_actor_ref("/user/unload").is_some() {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .expect("registry must retire after unload without a second stop request");
            for slot in [0, 2, 3, 4, 5, 7] {
                assert_eq!(
                    self.value(slot),
                    1,
                    "native phase {slot} must occur exactly once"
                );
            }
            assert_eq!(
                self.value(8),
                0,
                "fixture watchdog must not be needed to release the worker"
            );
            assert!(setup.spawner.pending_cleanup().is_empty());
        }
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    impl Drop for UnloadProbe {
        fn drop(&mut self) {
            self.release(1);
            self.release(6);
            if self.value(7) == 0 {
                // On failed assertions an image may still hold the pointer.
                // Do not turn a test failure into a use-after-free in native code.
                std::mem::forget(self.0.clone());
            }
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_unload_timeout_retains_owner_without_ambient_runtime() {
        isolated_native(
            "enabled::native_unload_timeout_retains_owner_without_ambient_runtime",
            || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap();
                let mut setup =
                    runtime.block_on(async { Setup::with_timeout(Duration::from_millis(150)) });
                let probe = UnloadProbe::new(&mut setup, false);
                let actor = futures::executor::block_on(setup.spawn(&["/user/unload"]))
                    .unwrap()
                    .pop()
                    .unwrap();
                runtime.block_on(async {
                    assert_eq!(
                        ask(&actor, "echo", b"before unload").await,
                        b"before unload"
                    );
                    let result = tokio::time::timeout(Duration::from_secs(5), actor.stop())
                        .await
                        .unwrap();
                    assert!(
                        matches!(
                            result,
                            Err(parrot_api::errors::ActorError::TimeoutDetail(_))
                        ),
                        "{result:?}"
                    );
                    probe.wait_for(5).await;
                    assert_eq!(probe.value(4), 1, "destroy must precede image finalizer");
                    assert_eq!(probe.value(7), 0);
                    assert!(
                        setup.engine.get_actor_ref("/user/unload").is_some(),
                        "stop timeout released path before dlclose finished"
                    );
                    probe.release(6);
                    probe.retired(&setup).await;
                    setup.engine.shutdown_internal().await.unwrap();
                });
            },
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_unload_survives_cancellation_of_stop_wait() {
        isolated_native(
            "enabled::native_unload_survives_cancellation_of_stop_wait",
            || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let mut setup = Setup::new();
                    let probe = UnloadProbe::new(&mut setup, false);
                    let actor = setup.spawn(&["/user/unload"]).await.unwrap().pop().unwrap();
                    let stop = tokio::spawn(async move { actor.stop().await });
                    probe.wait_for(5).await;
                    assert!(
                        !stop.is_finished(),
                        "stop acknowledged before actual unload"
                    );
                    stop.abort();
                    assert!(stop.await.unwrap_err().is_cancelled());
                    assert!(setup.engine.get_actor_ref("/user/unload").is_some());
                    assert_eq!(probe.value(7), 0);
                    probe.release(6);
                    probe.retired(&setup).await;
                    setup.engine.shutdown_internal().await.unwrap();
                });
            },
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_unload_survives_cancellation_of_constructor_wait() {
        isolated_native(
            "enabled::native_unload_survives_cancellation_of_constructor_wait",
            || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let mut setup = Setup::new();
                    let probe = UnloadProbe::new(&mut setup, true);
                    let spawner = setup.spawner.clone();
                    let artifact = setup.artifact.clone();
                    let config = setup.config.clone();
                    let job = tokio::spawn(async move {
                        spawner
                            .spawn_artifact(
                                &artifact,
                                &["/user/unload".into(), "/user/not-started".into()],
                                Some(&config),
                            )
                            .await
                    });
                    probe.wait_for(0).await;
                    job.abort();
                    assert!(job.await.unwrap_err().is_cancelled());
                    probe.release(1);
                    probe.wait_for(5).await;
                    assert_eq!(probe.value(4), 1);
                    assert!(setup.engine.get_actor_ref("/user/unload").is_some());
                    // Deliberately exceed the original observation window while
                    // dlclose is blocked. A destroy event is not retirement proof.
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    assert!(setup.engine.get_actor_ref("/user/unload").is_some());
                    assert_eq!(probe.value(7), 0);
                    probe.release(6);
                    probe.retired(&setup).await;
                    assert!(setup.engine.get_actor_ref("/user/not-started").is_none());
                    setup.engine.shutdown_internal().await.unwrap();
                });
            },
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_unload_after_business_failure_retains_the_retirement_owner() {
        isolated_native(
            "enabled::native_unload_after_business_failure_retains_the_retirement_owner",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let mut setup = Setup::with_timeout(Duration::from_millis(150));
                        let probe = UnloadProbe::new(&mut setup, false);
                        let actor = setup.spawn(&["/user/unload"]).await.unwrap().pop().unwrap();
                        let error = actor
                            .ask(DylibAsk {
                                type_key: "error".into(),
                                payload: vec![],
                            })
                            .await
                            .unwrap_err();
                        assert!(error.to_string().contains("native code 2"), "{error}");
                        probe.wait_for(5).await;
                        assert_eq!(probe.value(4), 1);
                        // Failure skips before_stop. Actual retirement must therefore
                        // still be owned by disposal, not by the successful-stop hook.
                        let result = tokio::time::timeout(Duration::from_secs(5), actor.stop())
                            .await
                            .unwrap();
                        assert!(
                            matches!(
                                result,
                                Err(parrot_api::errors::ActorError::TimeoutDetail(_))
                            ),
                            "failure retirement escaped its native owner: {result:?}"
                        );
                        assert!(setup.engine.get_actor_ref("/user/unload").is_some());
                        assert_eq!(probe.value(7), 0);
                        probe.release(6);
                        probe.retired(&setup).await;
                        setup.engine.shutdown_internal().await.unwrap();
                    });
            },
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_concurrent_loading_and_retirement_preserve_every_owner() {
        isolated_native(
            "enabled::native_concurrent_loading_and_retirement_preserve_every_owner",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let mut setups: Vec<_> = (0..4)
                            .map(|_| Setup::with_timeout(Duration::from_millis(150)))
                            .collect();
                        let probes: Vec<_> = setups
                            .iter_mut()
                            .map(|setup| UnloadProbe::new(setup, false))
                            .collect();
                        // Only arm the unload stall once all instances are published.
                        // A startup failure must be able to roll back and report its
                        // own error instead of waiting for the later stop-phase gate.
                        for probe in &probes {
                            probe.release(6);
                        }
                        // Every image has an independent identity and native state. The
                        // spawns overlap in the same process, exercising the real loader.
                        let actors = futures::future::try_join_all(
                            setups.iter().map(|setup| setup.spawn(&["/user/unload"])),
                        )
                        .await
                        .unwrap();
                        for probe in &probes {
                            assert_eq!(probe.value(5), 0, "startup must not have retired an image");
                            probe.0[6].store(0, std::sync::atomic::Ordering::SeqCst);
                        }
                        for actor in &actors {
                            assert_eq!(ask(&actor[0], "echo", b"concurrent").await, b"concurrent");
                        }
                        // A finalizer can hold the process loader lock. Even an async
                        // thread can need that lock for its first TLS destructor
                        // registration. Release from an already-running OS thread,
                        // not from the runtime whose progress we deliberately obstruct.
                        let observed: Vec<_> = probes.iter().map(|probe| probe.0.clone()).collect();
                        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
                        let release = std::thread::spawn(move || {
                            use std::sync::atomic::Ordering;
                            ready_tx.send(()).unwrap();
                            let deadline = std::time::Instant::now() + Duration::from_secs(5);
                            let mut entered = false;
                            while std::time::Instant::now() < deadline {
                                if observed
                                    .iter()
                                    .any(|state| state[5].load(Ordering::SeqCst) == 1)
                                {
                                    entered = true;
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(2));
                            }
                            if entered {
                                std::thread::sleep(Duration::from_millis(300));
                            }
                            let held = observed.iter().any(|state| {
                                state[5].load(Ordering::SeqCst) == 1
                                    && state[7].load(Ordering::SeqCst) == 0
                            });
                            for state in &observed {
                                state[6].store(1, Ordering::SeqCst);
                            }
                            (entered, held)
                        });
                        ready_rx.recv().unwrap();
                        let results = futures::future::join_all(actors.iter().zip(&probes).map(
                            |(actor, probe)| async move {
                                let result = actor[0].stop().await;
                                (result, probe.value(7))
                            },
                        ))
                        .await;
                        assert_eq!(release.join().unwrap(), (true, true));
                        // A loader lock can delay timeout delivery. Both a completed
                        // stop and an expired wait are valid; neither loses ownership.
                        for (result, unloaded_at_return) in results {
                            if result.is_ok() {
                                assert_eq!(unloaded_at_return, 1, "stop succeeded before unload");
                            }
                            assert!(
                                result.is_ok()
                                    || matches!(
                                        result,
                                        Err(parrot_api::errors::ActorError::TimeoutDetail(_))
                                    ),
                                "{result:?}"
                            );
                        }
                        for (setup, probe) in setups.iter().zip(&probes) {
                            probe.retired(setup).await;
                            setup.engine.shutdown_internal().await.unwrap();
                        }
                    });
            },
        );
    }

    #[test]
    fn native_wire_codecs_are_stable_and_reject_unconsumed_suffixes() {
        use parrot_api::types::BoxedMessage;
        use parrot_app::host::dylib::DylibReply;
        let registry = parrot_remote::codec_registry::CodecRegistry::global();
        for (key, value, expected) in [
            (
                "bin:parrot.dylib.Ask#v1",
                Box::new(DylibAsk {
                    type_key: "echo".into(),
                    payload: vec![0, 255],
                }) as BoxedMessage,
                vec![4, b'e', b'c', b'h', b'o', 2, 0, 255],
            ),
            (
                "bin:parrot.dylib.Tell#v1",
                Box::new(DylibTell {
                    type_key: "echo".into(),
                    payload: vec![7],
                }) as BoxedMessage,
                vec![4, b'e', b'c', b'h', b'o', 1, 7],
            ),
            (
                "bin:parrot.dylib.Reply#v1",
                Box::new(DylibReply(vec![0, 255])) as BoxedMessage,
                vec![2, 0, 255],
            ),
        ] {
            let (actual, bytes) = registry.encode_outgoing(&value).unwrap();
            assert_eq!(actual, key);
            assert_eq!(bytes.as_ref(), expected);
            assert!(registry.decode_incoming(key, &expected).is_ok());
            let mut suffixed = expected;
            suffixed.push(0);
            assert!(registry.decode_incoming(key, &suffixed).is_err());
        }
    }
}
