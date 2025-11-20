use super::*;
use futures::FutureExt;
use parrot_api::address::ActorRefExt;

fn spawner(engine: &Arc<ThreadActorSystem>) -> ThreadPropsSpawner {
    ThreadPropsSpawner::new(engine.clone())
        .with_wasm_messaging(vec![grant(WasmMessageOperation::Tell)])
        .unwrap()
        .with_wasm_limits(
            parrot_wasm::WasmConfig {
                fuel_per_message: 20_000_000,
                epoch_deadline: 10_000,
                ..Default::default()
            },
            Default::default(),
        )
}

#[tokio::test]
async fn conflicting_path_never_executes_guest_start_and_preserves_original_actor() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let engine = ThreadActorSystem::shared(Default::default());
        let observed = Arc::new(Observed::default());
        let native = engine
            .spawn_at(
                Native(observed.clone()),
                "/user/native",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let original = Arc::new(Observed::default());
        let existing = engine
            .spawn_at(
                Native(original.clone()),
                "/user/guest",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let result = spawner(&engine)
            .spawn_artifact(
                &artifact(),
                &["/user/guest".into()],
                Some(&config(&[("start-value", "3"), ("drain-value", "5")])),
            )
            .await;
        native.send(Box::new(Request(9))).await.unwrap();
        existing.send(Box::new(Request(11))).await.unwrap();
        let values = observed.values.lock().unwrap().clone();
        let original_values = original.values.lock().unwrap().clone();
        let still_registered = engine.resolve_actor("/user/guest").is_some();
        engine.shutdown_internal().await.unwrap();
        assert!(result.is_err());
        assert_eq!(
            values,
            [9],
            "a rejected deployment must not execute guest startup"
        );
        assert_eq!(original_values, [11]);
        assert!(still_registered);
    })
    .await
    .expect("conflict and retirement deadline");
}

#[tokio::test]
async fn concurrent_same_path_only_runs_the_winning_guests_start() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let engine = ThreadActorSystem::shared(Default::default());
        let observed = Arc::new(Observed::default());
        let native = engine
            .spawn_at(
                Native(observed.clone()),
                "/user/native",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let one = spawner(&engine);
        let two = spawner(&engine);
        let artifact = artifact();
        let paths = ["/user/guest".into()];
        let first = config(&[("start-value", "3")]);
        let second = config(&[("start-value", "5")]);
        let (a, b) = tokio::join!(
            one.spawn_artifact(&artifact, &paths, Some(&first)),
            two.spawn_artifact(&artifact, &paths, Some(&second)),
        );
        native.send(Box::new(Request(9))).await.unwrap();
        let values = observed.values.lock().unwrap().clone();
        let winner = if a.is_ok() { 3 } else { 5 };
        let actor = engine.resolve_actor("/user/guest").unwrap();
        let started = actor
            .ask(WasmAsk {
                type_key: "started".into(),
                payload: vec![],
            })
            .await
            .unwrap()
            .0;
        engine.shutdown_internal().await.unwrap();
        assert_ne!(a.is_ok(), b.is_ok());
        assert_eq!(values, [winner, 9], "only the owner may run a startup hook");
        assert_eq!(started, [1]);
    })
    .await
    .expect("concurrent publication and retirement deadline");
}

#[derive(Debug)]
struct OwnershipProbe {
    engine: std::sync::Weak<ThreadActorSystem>,
    observations: Arc<Mutex<Vec<(u64, bool)>>>,
    attempted: Option<Arc<tokio::sync::Notify>>,
}
impl parrot::thread::mailbox::admission::MessageAdmission for OwnershipProbe {
    fn prepare(
        &self,
        message: BoxedMessage,
    ) -> Result<BoxedMessage, parrot::thread::error::MailboxError> {
        if let Some(request) = message.downcast_ref::<Request>() {
            let owned = self
                .engine
                .upgrade()
                .unwrap()
                .resolve_actor("/user/guest")
                .is_some();
            self.observations.lock().unwrap().push((request.0, owned));
            if request.0 == 3 {
                if let Some(attempted) = &self.attempted {
                    attempted.notify_one();
                }
            }
        }
        Ok(message)
    }
}

#[tokio::test]
async fn actual_startup_callback_observes_the_reserved_actor_identity() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let engine = ThreadActorSystem::shared(Default::default());
        let observations = Arc::new(Mutex::new(Vec::new()));
        let native = engine
            .spawn_at(
                Native(Arc::new(Observed::default())),
                "/user/native",
                None,
                ThreadActorConfig {
                    message_admission: Some(Arc::new(OwnershipProbe {
                        engine: Arc::downgrade(&engine),
                        observations: observations.clone(),
                        attempted: None,
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let recorder = parrot_api::trace::TraceRecorder::new(64).unwrap();
        let root = recorder.root();
        let guest = root
            .scope(spawner(&engine).spawn_artifact(
                &artifact(),
                &["/user/guest".into()],
                Some(&config(&[("start-value", "3")])),
            ))
            .await
            .unwrap()
            .remove(0);
        native.send(Box::new(Request(9))).await.unwrap();
        let observed = observations.lock().unwrap().clone();
        let started = guest
            .ask(WasmAsk {
                type_key: "started".into(),
                payload: vec![],
            })
            .await
            .unwrap()
            .0;
        let snapshot = recorder.snapshot();
        engine.shutdown_internal().await.unwrap();
        assert_eq!(observed, [(3, true), (9, true)]);
        assert_eq!(started, [1]);
        let startup: Vec<_> = snapshot
            .records
            .iter()
            .filter(|record| {
                record.kind == parrot_api::trace::SpanKind::WasmLifecycle
                    && record.target == "wasm/on-start:/user/guest"
            })
            .collect();
        assert_eq!(startup.len(), 1);
        let initialization: Vec<_> = snapshot
            .records
            .iter()
            .filter(|record| {
                record.kind == parrot_api::trace::SpanKind::ActorInit
                    && record.target == "/user/guest"
            })
            .collect();
        assert_eq!(initialization.len(), 1);
        assert_eq!(initialization[0].trace_id, root.trace_id());
        assert_eq!(initialization[0].parent_span_id, Some(root.span_id()));
        assert_eq!(initialization[0].status, parrot_api::trace::SpanStatus::Ok);
        assert_eq!(startup[0].trace_id, root.trace_id());
        assert_eq!(startup[0].parent_span_id, Some(initialization[0].span_id));
        assert_eq!(startup[0].status, parrot_api::trace::SpanStatus::Ok);
    })
    .await
    .expect("startup ownership and retirement deadline");
}

#[tokio::test]
async fn deployment_waits_for_actual_startup_while_native_mailbox_is_full() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let engine = ThreadActorSystem::shared(Default::default());
        let observed = Arc::new(Observed::default());
        let attempted = Arc::new(tokio::sync::Notify::new());
        let native = engine
            .spawn_at(
                Native(observed.clone()),
                "/user/native",
                None,
                ThreadActorConfig {
                    mailbox_capacity: Some(1),
                    backpressure_strategy: Some(
                        parrot::thread::config::BackpressureStrategy::Block,
                    ),
                    message_admission: Some(Arc::new(OwnershipProbe {
                        engine: Arc::downgrade(&engine),
                        observations: Arc::default(),
                        attempted: Some(attempted.clone()),
                    })),
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
        let spawner = spawner(&engine);
        let artifact = artifact();
        let paths = ["/user/guest".into()];
        let config = config(&[("start-value", "3"), ("timeout-ms", "10000")]);
        let mut deployment = Box::pin(spawner.spawn_artifact(&artifact, &paths, Some(&config)));
        assert!(deployment.as_mut().now_or_never().is_none());
        tokio::time::timeout(Duration::from_secs(5), attempted.notified())
            .await
            .unwrap();
        let early = tokio::time::timeout(Duration::from_millis(200), deployment.as_mut()).await;
        let returned_early = early.is_ok();
        let was_reserved = engine.resolve_actor("/user/guest").is_some();
        observed.release.notify_one();
        let guest = match early {
            Ok(result) => result,
            Err(_) => deployment.await,
        }
        .unwrap()
        .remove(0);
        let started = guest
            .ask(WasmAsk {
                type_key: "started".into(),
                payload: vec![],
            })
            .await
            .unwrap()
            .0;
        native.send(Box::new(Request(9))).await.unwrap();
        let values = observed.values.lock().unwrap().clone();
        engine.shutdown_internal().await.unwrap();
        assert!(
            !returned_early,
            "publication is not successful guest initialization"
        );
        assert!(
            was_reserved,
            "startup must wait under its admitted identity"
        );
        assert_eq!(started, [1]);
        assert_eq!(values, [42, 6, 3, 9]);
    })
    .await
    .expect("gated startup and retirement deadline");
}

#[tokio::test]
async fn failed_startup_retires_its_original_identity_before_same_path_retry() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let engine = ThreadActorSystem::shared(Default::default());
        let observed = Arc::new(Observed::default());
        let native = engine
            .spawn_at(
                Native(observed.clone()),
                "/user/native",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let spawner = spawner(&engine);
        let failed = spawner
            .spawn_artifact(
                &artifact(),
                &["/user/guest".into()],
                Some(&config(&[("start-value", "not-a-number")])),
            )
            .await;
        let was_retired = engine.resolve_actor("/user/guest").is_none();
        let no_pending = spawner.pending_cleanup().is_empty();
        let guest = spawner
            .spawn_artifact(
                &artifact(),
                &["/user/guest".into()],
                Some(&config(&[("start-value", "3")])),
            )
            .await
            .unwrap()
            .remove(0);
        let started = guest
            .ask(WasmAsk {
                type_key: "started".into(),
                payload: vec![],
            })
            .await
            .unwrap()
            .0;
        native.send(Box::new(Request(9))).await.unwrap();
        let values = observed.values.lock().unwrap().clone();
        engine.shutdown_internal().await.unwrap();
        assert!(failed.is_err());
        assert!(was_retired);
        assert!(no_pending);
        assert_eq!(started, [1]);
        assert_eq!(values, [3, 9]);
    })
    .await
    .expect("startup failure and retry deadline");
}

#[tokio::test]
async fn late_pool_conflict_only_drains_the_member_that_really_started() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let engine = ThreadActorSystem::shared(Default::default());
        let observed = Arc::new(Observed::default());
        let native = engine
            .spawn_at(
                Native(observed.clone()),
                "/user/native",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let original_values = Arc::new(Observed::default());
        let original = engine
            .spawn_at(
                Native(original_values.clone()),
                "/user/taken",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let spawner = spawner(&engine)
            .with_wasm_messaging(vec![
                grant(WasmMessageOperation::Tell),
                WasmMessageGrant {
                    source_path: "/user/taken".into(),
                    ..grant(WasmMessageOperation::Tell)
                },
            ])
            .unwrap();
        let result = spawner
            .spawn_artifact(
                &artifact(),
                &["/user/guest".into(), "/user/taken".into()],
                Some(&config(&[("start-value", "3"), ("drain-value", "5")])),
            )
            .await;
        native.send(Box::new(Request(9))).await.unwrap();
        original.send(Box::new(Request(11))).await.unwrap();
        let values = observed.values.lock().unwrap().clone();
        let originals = original_values.values.lock().unwrap().clone();
        let was_retired = engine.resolve_actor("/user/guest").is_none();
        let was_preserved = engine.resolve_actor("/user/taken").is_some();
        let no_pending = spawner.pending_cleanup().is_empty();
        engine.shutdown_internal().await.unwrap();
        assert!(result.is_err());
        assert_eq!(values, [3, 5, 9]);
        assert_eq!(originals, [11]);
        assert!(was_retired && was_preserved && no_pending);
    })
    .await
    .expect("pool rollback and retirement deadline");
}

#[tokio::test]
async fn conflict_is_rejected_before_a_trapping_component_initializer_runs() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let engine = ThreadActorSystem::shared(Default::default());
        let observed = Arc::new(Observed::default());
        let original = engine.spawn_at(Native(observed.clone()), "/user/guest", None, Default::default()).await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("initializer.wat");
        let bytes = b"(component (core module $m (func $init unreachable) (start $init)) (core instance (instantiate $m)))";
        std::fs::write(&path, bytes).unwrap();
        let artifact = ArtifactRef::Wasm { digest: format!("{:x}", Sha256::digest(bytes)), uri: url::Url::from_file_path(&path).unwrap().into() };
        let result = spawner(&engine).spawn_artifact(&artifact, &["/user/guest".into()], None).await;
        let error = match result { Ok(_) => panic!("conflicting component was accepted"), Err(error) => error.to_string() };
        let unoccupied = spawner(&engine).spawn_artifact(&artifact, &["/user/trapping".into()], None).await;
        let trap_retired = engine.resolve_actor("/user/trapping").is_none();
        original.send(Box::new(Request(13))).await.unwrap();
        let values = observed.values.lock().unwrap().clone();
        engine.shutdown_internal().await.unwrap();
        assert!(error.contains("already exists"), "{error}");
        assert!(matches!(unoccupied, Err(parrot_app::DeployError::Artifact(_))));
        assert!(trap_retired);
        assert_eq!(values, [13]);
    }).await.expect("initializer rejection and retirement deadline");
}
