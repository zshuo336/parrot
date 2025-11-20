use super::*;
use parrot_api::trace::{self, SpanKind, SpanStatus, TraceRecorder};
use parrot_app::{assemble::ParrotAppHook, hook_phase};

fn setup() -> Setup {
    let mut setup = Setup::new();
    let image = fixture().with_file_name(format!(
        "libparrot_abi_recordingcomp.{}",
        std::env::consts::DLL_EXTENSION,
    ));
    let image = image.canonicalize().unwrap();
    let digest = format!(
        "sha256:{:x}",
        Sha256::digest(std::fs::read(&image).unwrap())
    );
    setup.artifact = ArtifactRef::Dylib {
        digest: digest.clone(),
        uri: url::Url::from_file_path(image).unwrap().into(),
        abi: 1,
    };
    // SAFETY: the test builds and owns the synchronous non-unwinding fixture.
    setup.spawner =
        unsafe { ThreadPropsSpawner::new(setup.engine.clone()).with_trusted_dylib(&digest) }
            .unwrap();
    setup.config.insert("lifecycle".into(), true.into());
    setup.config.insert(
        "lifecycle_events".into(),
        setup.temp.path().join("events").to_str().unwrap().into(),
    );
    setup
}

fn identity(trace: [u8; 16], span: [u8; 8]) -> String {
    trace
        .iter()
        .chain(span.iter())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn event(setup: &Setup, phase: &str) -> Vec<String> {
    let lines = setup.events();
    let events: Vec<_> = lines
        .lines()
        .filter(|line| line.starts_with(&format!("{phase}|")))
        .collect();
    assert_eq!(events.len(), 1, "one actual {phase}: {lines}");
    events[0].split('|').map(str::to_owned).collect()
}

struct Release(PathBuf);
impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"release");
    }
}

async fn entered(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

native_test!(each_guest_lifecycle_invocation_has_its_own_bounded_recording_budget, bounded_recording_contract);
async fn bounded_recording_contract() {
    let mut setup = setup();
    setup.config.insert("lifecycle_budget".into(), true.into());
    let recorder = TraceRecorder::new(1024).unwrap();
    let actors = recorder
        .root()
        .scope(setup.spawn(&["/user/guest-budget"]))
        .await
        .unwrap();
    recorder.root().in_scope(|| actors[0].stop()).await.unwrap();
    setup.engine.shutdown_internal().await.unwrap();
    recorder.close();
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.counts.overwritten, 0);
    assert!(snapshot.terminal);
    for phase in ["construct", "drain", "destroy"] {
        let records: Vec<_> = snapshot
            .records
            .iter()
            .filter(|span| {
                span.kind == SpanKind::NativeGuest
                    && span.target.starts_with(&format!("guest/{phase}"))
            })
            .collect();
        assert_eq!(records.len(), 256);
        assert_eq!(
            records
                .iter()
                .filter(|span| span.status == SpanStatus::Cancelled)
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|span| span.status == SpanStatus::Ok)
                .count(),
            255
        );
    }
    assert!(snapshot
        .records
        .iter()
        .all(|span| span.target != "over-lifecycle-budget"));
}

native_test!(cancelled_guest_constructor_waiter_keeps_records_until_actual_cleanup, cancelled_constructor_contract);
async fn cancelled_constructor_contract() {
    let mut setup = setup();
    let gate = setup.gate("construct_entered");
    let release = Release(setup.gate("construct_release"));
    let recorder = TraceRecorder::new(64).unwrap();
    let root = recorder.root();
    let spawner = setup.spawner.clone();
    let artifact = setup.artifact.clone();
    let config = setup.config.clone();
    let job = tokio::spawn(root.scope(async move {
        spawner
            .spawn_artifact(
                &artifact,
                &["/user/guest-constructor-gate".into()],
                Some(&config),
            )
            .await
    }));
    entered(&gate).await;
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    recorder.close();
    assert!(!recorder.snapshot().terminal);
    assert!(!setup.events().contains("released"));
    std::fs::write(&release.0, b"release").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !recorder.snapshot().terminal
            || !setup.events().lines().any(|line| line == "released")
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    setup.engine.shutdown_internal().await.unwrap();
    let snapshot = recorder.snapshot();
    let construct = snapshot
        .records
        .iter()
        .find(|span| span.target == "native/construct:/user/guest-constructor-gate")
        .unwrap();
    assert_eq!(construct.status, SpanStatus::Ok);
    for phase in ["construct", "drain", "destroy"] {
        assert_eq!(
            event(&setup, phase)[2],
            if phase == "construct" { "0" } else { "1" }
        );
    }
    let unfinished = snapshot
        .records
        .iter()
        .find(|span| span.target == "guest/construct/unfinished")
        .unwrap();
    assert_eq!(unfinished.status, SpanStatus::Cancelled);
    assert_eq!(unfinished.parent_span_id, Some(construct.span_id));
}

native_test!(guest_lifecycle_keeps_exact_phase_parents_nested_spans_and_cross_phase_handles, phase_parents_contract);
async fn phase_parents_contract() {
    let setup = setup();
    let recorder = TraceRecorder::new(128).unwrap();
    let creation = recorder.root();
    let message = recorder.root();
    let drain = recorder.root();
    let stop = recorder.root();
    let actors = creation
        .scope(setup.spawn(&["/user/guest-lifecycle"]))
        .await
        .unwrap();
    message.scope(ask(&actors[0], "lifecycle-stale", b"")).await;
    drain
        .scope(async {
            actors[0]
                .send(Box::new(ParrotAppHook(hook_phase::DRAIN)))
                .await
        })
        .await
        .unwrap();
    // Both cached hook commands must leave the original guest invocation alone.
    for phase in [hook_phase::DRAIN, hook_phase::STOP] {
        recorder
            .root()
            .scope(async { actors[0].send(Box::new(ParrotAppHook(phase))).await })
            .await
            .unwrap();
    }
    stop.in_scope(|| actors[0].stop()).await.unwrap();
    setup.engine.shutdown_internal().await.unwrap();
    recorder.close();
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.counts.overwritten, 0);
    assert!(snapshot.terminal);
    let mut previous = 0;
    for (phase, kind, target, trace) in [
        (
            "construct",
            SpanKind::NativeLifecycle,
            "native/construct:/user/guest-lifecycle",
            creation.trace_id(),
        ),
        (
            "message",
            SpanKind::DynamicHandler,
            "/user/guest-lifecycle",
            message.trace_id(),
        ),
        (
            "drain",
            SpanKind::NativeLifecycle,
            "native/drain:/user/guest-lifecycle",
            drain.trace_id(),
        ),
        (
            "destroy",
            SpanKind::NativeLifecycle,
            "native/retire:/user/guest-lifecycle",
            stop.trace_id(),
        ),
    ] {
        let parent = snapshot
            .records
            .iter()
            .find(|span| span.kind == kind && span.target == target && span.trace_id == trace)
            .unwrap();
        let observed = event(&setup, phase);
        assert_eq!(observed[1], identity(parent.trace_id, parent.span_id));
        assert_eq!(observed[2], "0", "guest recording enabled");
        let handle = observed[3].parse::<u64>().unwrap();
        assert!(handle > previous, "no cross-phase handle reuse");
        previous = handle;
        assert_eq!(observed[4], "4", "stale finish rejected");
        let outer = snapshot
            .records
            .iter()
            .find(|span| span.target == format!("guest/{phase}"))
            .unwrap();
        let nested = snapshot
            .records
            .iter()
            .find(|span| span.target == format!("guest/{phase}/nested"))
            .unwrap();
        let unfinished = snapshot
            .records
            .iter()
            .find(|span| span.target == format!("guest/{phase}/unfinished"))
            .unwrap();
        assert_eq!(outer.kind, SpanKind::NativeGuest);
        assert_eq!(outer.parent_span_id, Some(parent.span_id));
        assert_eq!(outer.trace_id, trace);
        assert_eq!(outer.status, SpanStatus::Ok);
        assert_eq!(nested.parent_span_id, Some(outer.span_id));
        assert_eq!(nested.trace_id, trace);
        assert_eq!(nested.status, SpanStatus::Ok);
        assert_eq!(unfinished.parent_span_id, Some(parent.span_id));
        assert_eq!(unfinished.status, SpanStatus::Cancelled);
    }
    assert_eq!(
        setup
            .events()
            .lines()
            .filter(|line| *line == "released")
            .count(),
        1
    );
}

native_test!(implicit_guest_drain_is_nested_under_retirement_and_errors_still_release_state, implicit_drain_contract);
async fn implicit_drain_contract() {
    for (phase, suffix, status) in [
        ("drain", "error", SpanStatus::Error),
        ("drain", "panic", SpanStatus::Panicked),
        ("destroy", "error", SpanStatus::Error),
        ("destroy", "panic", SpanStatus::Panicked),
    ] {
        let mut setup = setup();
        setup
            .config
            .insert(format!("{phase}_{suffix}"), true.into());
        let recorder = TraceRecorder::new(64).unwrap();
        let actors = recorder
            .root()
            .scope(setup.spawn(&["/user/guest-error"]))
            .await
            .unwrap();
        let stopped = recorder.root().in_scope(|| actors[0].stop()).await;
        assert!(
            stopped.is_err(),
            "real retirement failure must reach the waiter"
        );
        setup.engine.shutdown_internal().await.unwrap();
        recorder.close();
        let snapshot = recorder.snapshot();
        let retire = snapshot
            .records
            .iter()
            .find(|span| span.target == "native/retire:/user/guest-error")
            .unwrap();
        let drain = snapshot
            .records
            .iter()
            .find(|span| span.target == "native/drain:/user/guest-error")
            .unwrap();
        assert_eq!(drain.parent_span_id, Some(retire.span_id));
        assert_eq!(retire.status, status);
        assert_eq!(
            drain.status,
            if phase == "drain" {
                status
            } else {
                SpanStatus::Ok
            }
        );
        let unfinished = snapshot
            .records
            .iter()
            .find(|span| span.target == format!("guest/{phase}/unfinished"))
            .unwrap();
        assert_eq!(unfinished.status, status);
        assert_eq!(
            event(&setup, "drain")[1],
            identity(drain.trace_id, drain.span_id)
        );
        assert_eq!(
            event(&setup, "destroy")[1],
            identity(retire.trace_id, retire.span_id)
        );
        assert_eq!(
            setup
                .events()
                .lines()
                .filter(|line| *line == "released")
                .count(),
            1
        );
        assert!(snapshot.terminal);
    }
}

native_test!(failed_guest_constructor_finishes_spans_and_never_invents_an_instance, failed_constructor_contract);
async fn failed_constructor_contract() {
    for (suffix, status) in [
        ("error", SpanStatus::Error),
        ("panic", SpanStatus::Panicked),
    ] {
        let mut setup = setup();
        setup
            .config
            .insert(format!("construct_{suffix}"), true.into());
        let recorder = TraceRecorder::new(32).unwrap();
        assert!(recorder
            .root()
            .scope(setup.spawn(&["/user/guest-fail"]))
            .await
            .is_err());
        setup.engine.shutdown_internal().await.unwrap();
        recorder.close();
        let snapshot = recorder.snapshot();
        let construct = snapshot
            .records
            .iter()
            .find(|span| span.target == "native/construct:/user/guest-fail")
            .unwrap();
        let unfinished = snapshot
            .records
            .iter()
            .find(|span| span.target == "guest/construct/unfinished")
            .unwrap();
        assert_eq!(construct.status, status);
        assert_eq!(unfinished.status, status);
        assert_eq!(unfinished.parent_span_id, Some(construct.span_id));
        assert_eq!(
            setup.events().lines().count(),
            1,
            "no drain, destroy or state allocation on failure"
        );
        assert!(snapshot.terminal);
    }
}

native_test!(untraced_identity_only_and_closed_recorders_do_not_enable_guest_recording, disabled_recording_contract);
async fn disabled_recording_contract() {
    for mode in 0..3 {
        let setup = setup();
        let recorder = TraceRecorder::new(32).unwrap();
        let context = match mode {
            0 => None,
            1 => Some(parrot_api::trace::TraceContext::root()),
            _ => {
                let root = recorder.root();
                recorder.close();
                Some(root)
            }
        };
        let actors = trace::scope(context.clone(), setup.spawn(&["/user/guest-disabled"]))
            .await
            .unwrap();
        trace::in_scope(context.clone(), || actors[0].stop())
            .await
            .unwrap();
        setup.engine.shutdown_internal().await.unwrap();
        for phase in ["construct", "drain", "destroy"] {
            let event = event(&setup, phase);
            assert_eq!(event[2], "1");
            assert_eq!(event[3], "0");
            if let Some(context) = &context {
                assert!(event[1].starts_with(&identity(context.trace_id(), [0; 8])[..32]));
                assert_ne!(event[1], identity(context.trace_id(), context.span_id()));
            } else {
                assert_eq!(event[1], "none");
            }
        }
        assert!(recorder.snapshot().records.is_empty());
    }
}

native_test!(cancelled_guest_destroy_waiter_does_not_finish_recording_or_release_the_worker, cancelled_destroy_contract);
async fn cancelled_destroy_contract() {
    let mut setup = setup();
    let entered = setup.gate("destroy_entered");
    let release = setup.gate("destroy_release");
    let release = Release(release);
    let recorder = TraceRecorder::new(64).unwrap();
    let actors = recorder
        .root()
        .scope(setup.spawn(&["/user/guest-gate"]))
        .await
        .unwrap();
    {
        let stopping = recorder.root().in_scope(|| actors[0].stop());
        tokio::pin!(stopping);
        tokio::select! {
            result = &mut stopping => panic!("destroy gate returned early: {result:?}"),
            _ = async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !entered.exists() { tokio::time::sleep(Duration::from_millis(2)).await; }
                }).await.unwrap();
            } => {}
        }
    }
    // Close the recorder while the actual hook is still executing.
    recorder.close();
    assert!(!recorder.snapshot().terminal);
    assert!(!setup.events().lines().any(|line| line == "released"));
    std::fs::write(&release.0, b"release").unwrap();
    actors[0].stop().await.unwrap();
    setup.engine.shutdown_internal().await.unwrap();
    let snapshot = recorder.snapshot();
    assert!(snapshot.terminal);
    assert_eq!(
        snapshot
            .records
            .iter()
            .find(|span| span.target == "guest/destroy/unfinished")
            .unwrap()
            .status,
        SpanStatus::Cancelled
    );
    assert_eq!(
        setup
            .events()
            .lines()
            .filter(|line| *line == "released")
            .count(),
        1
    );
}
