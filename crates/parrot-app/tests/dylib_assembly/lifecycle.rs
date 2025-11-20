use super::*;
use parrot_api::trace::{self, SpanKind, SpanStatus, TraceRecorder};
use parrot_app::{assemble::ParrotAppHook, hook_phase};

native_test!(native_construction_records_actual_load_and_construct_beneath_actor_init, construction_recording_contract);
async fn construction_recording_contract() {
    let setup = Setup::new();
    let recorder = TraceRecorder::new(32).unwrap();
    let root = recorder.root();
    let actors = root
        .scope(setup.spawn(&["/user/native-lifecycle"]))
        .await
        .unwrap();
    let snapshot = recorder.snapshot();
    actors[0].stop().await.unwrap();
    setup.engine.shutdown_internal().await.unwrap();
    assert!(setup.events().contains("construct:/user/native-lifecycle"));
    let init = snapshot
        .records
        .iter()
        .find(|span| span.kind == SpanKind::ActorInit && span.target == "/user/native-lifecycle")
        .expect("actual engine init record");
    for phase in ["load", "construct"] {
        let target = format!("native/{phase}:/user/native-lifecycle");
        let spans: Vec<_> = snapshot
            .records
            .iter()
            .filter(|span| span.target == target)
            .collect();
        assert_eq!(spans.len(), 1, "one actual native {phase} invocation");
        let span = spans[0];
        assert_eq!(span.trace_id, root.trace_id());
        assert_eq!(span.parent_span_id, Some(init.span_id));
        assert_ne!(span.span_id, init.span_id);
        assert_eq!(span.status, SpanStatus::Ok);
    }
}

native_test!(native_cached_drain_keeps_its_handler_and_untraced_stop_inherits_no_previous_identity, cached_drain_contract);
async fn cached_drain_contract() {
    let setup = Setup::new();
    let recorder = TraceRecorder::new(64).unwrap();
    let creation = recorder.root();
    let first = recorder.root();
    let later = recorder.root();
    let actors = creation
        .scope(setup.spawn(&["/user/native-drain"]))
        .await
        .unwrap();
    let actor = &actors[0];
    first
        .scope(async { actor.send(Box::new(ParrotAppHook(hook_phase::DRAIN))).await })
        .await
        .unwrap();
    for phase in [hook_phase::DRAIN, hook_phase::STOP] {
        later
            .scope(async { actor.send(Box::new(ParrotAppHook(phase))).await })
            .await
            .unwrap();
    }
    trace::in_scope(None, || actor.stop()).await.unwrap();
    setup.engine.shutdown_internal().await.unwrap();
    recorder.close();
    let snapshot = recorder.snapshot();
    let native: Vec<_> = snapshot
        .records
        .iter()
        .filter(|span| span.kind == SpanKind::NativeLifecycle)
        .collect();
    assert_eq!(
        native.len(),
        3,
        "one load, constructor and actual drain; untraced retirement adds none"
    );
    let drain = native
        .iter()
        .find(|span| span.target == "native/drain:/user/native-drain")
        .unwrap();
    let handler = snapshot
        .records
        .iter()
        .find(|span| span.kind == SpanKind::DynamicHandler && span.trace_id == first.trace_id())
        .unwrap();
    assert_eq!(drain.parent_span_id, Some(handler.span_id));
    assert_eq!(drain.trace_id, first.trace_id());
    assert!(native.iter().all(|span| span.trace_id != later.trace_id()));
    assert_eq!(
        setup
            .events()
            .lines()
            .filter(|line| *line == "drain:/user/native-drain")
            .count(),
        1
    );
    assert_eq!(
        setup
            .events()
            .lines()
            .filter(|line| *line == "destroy:/user/native-drain")
            .count(),
        1
    );
    assert!(snapshot.terminal);
}

native_test!(native_load_failure_records_only_actual_load_and_no_constructor_or_drain, load_failure_contract);
async fn load_failure_contract() {
    let mut setup = Setup::new();
    let image = setup
        .temp
        .path()
        .join(format!("no-metadata.{}", std::env::consts::DLL_EXTENSION));
    let source = setup.temp.path().join("no-metadata.c");
    std::fs::write(
        &source,
        b"int parrot_lifecycle_missing_metadata(void) { return 7; }\n",
    )
    .unwrap();
    let output = std::process::Command::new("cc")
        .args(["-Wall", "-Wextra", "-Werror", "-fPIC"])
        .arg(if cfg!(target_os = "macos") {
            "-dynamiclib"
        } else {
            "-shared"
        })
        .arg(&source)
        .arg("-o")
        .arg(&image)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let digest = format!(
        "sha256:{:x}",
        Sha256::digest(std::fs::read(&image).unwrap())
    );
    setup.artifact = ArtifactRef::Dylib {
        digest: digest.clone(),
        uri: url::Url::from_file_path(&image).unwrap().into(),
        abi: 1,
    };
    // SAFETY: this test builds the library above; it exports no hooks or metadata.
    setup.spawner =
        unsafe { ThreadPropsSpawner::new(setup.engine.clone()).with_trusted_dylib(&digest) }
            .unwrap();
    let recorder = TraceRecorder::new(16).unwrap();
    let root = recorder.root();
    let error = root
        .scope(setup.spawn(&["/user/native-load-fail"]))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("PARROT_ABI_META"), "{error}");
    setup.engine.shutdown_internal().await.unwrap();
    recorder.close();
    let snapshot = recorder.snapshot();
    let native: Vec<_> = snapshot
        .records
        .iter()
        .filter(|span| span.kind == SpanKind::NativeLifecycle)
        .collect();
    assert_eq!(native.len(), 1);
    assert_eq!(native[0].target, "native/load:/user/native-load-fail");
    assert_eq!(native[0].status, SpanStatus::Error);
    let init = snapshot
        .records
        .iter()
        .find(|span| span.kind == SpanKind::ActorInit)
        .unwrap();
    assert_eq!(native[0].parent_span_id, Some(init.span_id));
    assert_eq!(native[0].trace_id, root.trace_id());
    assert!(setup.events().is_empty());
    assert!(setup
        .engine
        .get_actor_ref("/user/native-load-fail")
        .is_none());
    assert!(snapshot.terminal);
}

native_test!(native_constructor_errors_and_caught_panics_preserve_actual_phase_status, constructor_failure_contract);
async fn constructor_failure_contract() {
    for (key, value, expected) in [
        ("fail_path", "/user/native-fail", SpanStatus::Error),
        ("construct_panic", "yes", SpanStatus::Panicked),
    ] {
        let mut setup = Setup::new();
        setup.config.insert(key.into(), value.into());
        let recorder = TraceRecorder::new(32).unwrap();
        let root = recorder.root();
        assert!(root
            .scope(setup.spawn(&["/user/native-fail"]))
            .await
            .is_err());
        setup.engine.shutdown_internal().await.unwrap();
        recorder.close();
        let snapshot = recorder.snapshot();
        let native: Vec<_> = snapshot
            .records
            .iter()
            .filter(|span| span.kind == SpanKind::NativeLifecycle)
            .collect();
        assert_eq!(
            native.len(),
            3,
            "load, failed construct, actual empty-handle unload"
        );
        for (phase, status) in [
            ("load", SpanStatus::Ok),
            ("construct", expected),
            ("retire", SpanStatus::Ok),
        ] {
            let span = native
                .iter()
                .find(|span| span.target == format!("native/{phase}:/user/native-fail"))
                .unwrap();
            assert_eq!(span.status, status);
            assert_eq!(span.trace_id, root.trace_id());
        }
        assert!(snapshot.terminal);
        assert!(!setup.events().contains("destroy:"));
        assert!(setup.engine.get_actor_ref("/user/native-fail").is_none());
    }
}

native_test!(native_implicit_drain_error_and_panic_retire_once_without_success_records, implicit_drain_failure_contract);
async fn implicit_drain_failure_contract() {
    for (key, expected) in [
        ("drain_error", SpanStatus::Error),
        ("drain_panic", SpanStatus::Panicked),
    ] {
        let mut setup = Setup::new();
        setup.config.insert(key.into(), "yes".into());
        let actors = setup.spawn(&["/user/native-error"]).await.unwrap();
        let recorder = TraceRecorder::new(32).unwrap();
        let root = recorder.root();
        assert!(root.in_scope(|| actors[0].stop()).await.is_err());
        setup.wait_for("destroy:/user/native-error").await;
        let _ = setup.engine.shutdown_internal().await;
        recorder.close();
        let snapshot = recorder.snapshot();
        let native: Vec<_> = snapshot
            .records
            .iter()
            .filter(|span| span.kind == SpanKind::NativeLifecycle)
            .collect();
        assert_eq!(native.len(), 2);
        let retire = native
            .iter()
            .find(|span| span.target == "native/retire:/user/native-error")
            .unwrap();
        let drain = native
            .iter()
            .find(|span| span.target == "native/drain:/user/native-error")
            .unwrap();
        assert_eq!(drain.parent_span_id, Some(retire.span_id));
        for span in &native {
            assert_eq!(span.status, expected);
            assert_eq!(span.trace_id, root.trace_id());
        }
        assert_eq!(
            setup
                .events()
                .lines()
                .filter(|line| *line == "drain:/user/native-error")
                .count(),
            1
        );
        assert_eq!(
            setup
                .events()
                .lines()
                .filter(|line| *line == "destroy:/user/native-error")
                .count(),
            1
        );
        assert!(snapshot.terminal);
    }
}

native_test!(native_explicit_failed_drain_keeps_actual_status_and_destroys_once, explicit_drain_failure_contract);
async fn explicit_drain_failure_contract() {
    for (key, expected) in [
        ("drain_error", SpanStatus::Error),
        ("drain_panic", SpanStatus::Panicked),
    ] {
        let mut setup = Setup::new();
        setup.config.insert(key.into(), "yes".into());
        let actors = setup.spawn(&["/user/native-explicit-error"]).await.unwrap();
        let recorder = TraceRecorder::new(16).unwrap();
        let root = recorder.root();
        assert!(root
            .scope(async {
                actors[0]
                    .send(Box::new(ParrotAppHook(hook_phase::DRAIN)))
                    .await
            })
            .await
            .is_err());
        let _ = actors[0].stop().await;
        let _ = setup.engine.shutdown_internal().await;
        recorder.close();
        let snapshot = recorder.snapshot();
        let drain: Vec<_> = snapshot
            .records
            .iter()
            .filter(|span| span.target == "native/drain:/user/native-explicit-error")
            .collect();
        assert_eq!(drain.len(), 1);
        assert_eq!(drain[0].status, expected);
        assert_eq!(drain[0].trace_id, root.trace_id());
        let handler = snapshot
            .records
            .iter()
            .find(|span| span.kind == SpanKind::DynamicHandler && span.trace_id == root.trace_id())
            .unwrap();
        assert_eq!(drain[0].parent_span_id, Some(handler.span_id));
        for event in ["drain", "destroy"] {
            assert_eq!(
                setup
                    .events()
                    .lines()
                    .filter(|line| { *line == format!("{event}:/user/native-explicit-error") })
                    .count(),
                1
            );
        }
        assert!(snapshot.terminal);
    }
}

native_test!(native_untraced_identity_only_and_closed_recorders_preserve_real_work_without_records, disabled_recording_contract);
async fn disabled_recording_contract() {
    for mode in 0..3 {
        let setup = Setup::new();
        let recorder = TraceRecorder::new(16).unwrap();
        if mode == 2 {
            recorder.close();
        }
        let parent = match mode {
            0 => None,
            1 => Some(trace::TraceContext::root()),
            _ => Some(recorder.root()),
        };
        let actors = recorder
            .root()
            .scope(trace::scope(
                parent,
                setup.spawn(&["/user/native-disabled"]),
            ))
            .await
            .unwrap();
        assert_eq!(
            ask(&actors[0], "echo", b"still works").await,
            b"still works"
        );
        actors[0].stop().await.unwrap();
        setup.engine.shutdown_internal().await.unwrap();
        assert!(setup.events().contains("destroy:/user/native-disabled"));
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.started, 0);
        assert!(snapshot.records.is_empty());
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn cancelled_constructor_wait_keeps_native_recording_until_actual_constructor_return() {
    isolated_native(
        "enabled::lifecycle::cancelled_constructor_wait_keeps_native_recording_until_actual_constructor_return",
        || tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let mut setup = Setup::new();
            let probe = UnloadProbe::new(&mut setup, true);
            let recorder = TraceRecorder::new(32).unwrap();
            let root = recorder.root();
            let spawner = setup.spawner.clone();
            let artifact = setup.artifact.clone();
            let config = setup.config.clone();
            let job = tokio::spawn(trace::scope(Some(root.clone()), async move {
                spawner.spawn_artifact(&artifact, &["/user/unload".into()], Some(&config)).await
            }));
            probe.wait_for(0).await;
            let blocked = recorder.snapshot();
            assert!(blocked.counts.in_flight >= 2, "init and actual native constructor both retain owners");
            assert!(blocked.records.iter().all(|span| span.target != "native/construct:/user/unload"));
            job.abort();
            assert!(job.await.unwrap_err().is_cancelled());
            recorder.close();
            assert!(!recorder.snapshot().terminal);
            probe.release(1);
            probe.wait_for(5).await;
            probe.release(6);
            probe.retired(&setup).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while !recorder.snapshot().terminal { tokio::task::yield_now().await; }
            }).await.unwrap();
            let snapshot = recorder.snapshot();
            setup.engine.shutdown_internal().await.unwrap();
            let spans: Vec<_> = snapshot.records.iter().filter(|span| span.target == "native/construct:/user/unload").collect();
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].status, SpanStatus::Ok, "caller cancellation did not cancel synchronous native work");
            assert_eq!(spans[0].trace_id, root.trace_id());
            assert_eq!(spans[0].kind, SpanKind::NativeLifecycle);
        }),
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn cancelled_native_stop_records_actual_unload_through_finalizer_completion() {
    isolated_native(
        "enabled::lifecycle::cancelled_native_stop_records_actual_unload_through_finalizer_completion",
        || tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let mut setup = Setup::new();
            let probe = UnloadProbe::new(&mut setup, false);
            let actor = setup.spawn(&["/user/unload"]).await.unwrap().pop().unwrap();
            let recorder = TraceRecorder::new(32).unwrap();
            let root = recorder.root();
            let stop = tokio::spawn(trace::scope(Some(root.clone()), async move { actor.stop().await }));
            probe.wait_for(5).await;
            assert_eq!(probe.value(4), 1, "instance destroyed before dlclose finalizer");
            let blocked = recorder.snapshot();
            assert!(blocked.records.iter().all(|span| span.target != "native/retire:/user/unload"));
            stop.abort();
            assert!(stop.await.unwrap_err().is_cancelled());
            recorder.close();
            probe.release(6);
            probe.retired(&setup).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while !recorder.snapshot().terminal { tokio::task::yield_now().await; }
            }).await.unwrap();
            let snapshot = recorder.snapshot();
            setup.engine.shutdown_internal().await.unwrap();
            let spans: Vec<_> = snapshot.records.iter().filter(|span| span.target == "native/retire:/user/unload").collect();
            assert_eq!(spans.len(), 1, "actual unload must retain its recording owner after waiter cancellation");
            let before_stop = snapshot.records.iter().find(|span| span.kind == SpanKind::ActorBeforeStop).unwrap();
            assert_eq!(spans[0].trace_id, root.trace_id());
            assert_eq!(spans[0].parent_span_id, Some(before_stop.span_id));
            assert_eq!(spans[0].status, SpanStatus::Ok);
            assert!(snapshot.terminal);
            assert_eq!(snapshot.counts.cancelled, 0, "native work completed despite caller cancellation");
        }),
    );
}
