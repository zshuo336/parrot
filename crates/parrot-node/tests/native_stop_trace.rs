//! Node management identity reaches actual native worker calls and retirement.
#![cfg(feature = "dylib")]
use futures::FutureExt;
use parrot_api::{
    address::ActorRefExt,
    trace::{SpanKind, SpanStatus, TraceRecorder},
};
use parrot_node::{DylibAsk, NativeTrust, NodeComponentExecutor};
use parrot_remote::admin_v2::{
    AdminArtifactRef, AdminInstancePolicy, AdminReplyV2, ComponentDeploy, ComponentExecutor,
};
use sha2::{Digest, Sha256};
use std::{future::Future, path::PathBuf, time::Duration};

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .expect("Node native lifecycle deadline")
}

fn fixture() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../parrot-abi/tests/fixtures");
    let output = std::process::Command::new("cargo")
        .args(["build", "--locked", "--release", "--target-dir"])
        .arg(root.join("target"))
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    root.join("target/release")
        .join(format!(
            "libparrot_abi_hostcomp.{}",
            std::env::consts::DLL_EXTENSION
        ))
        .canonicalize()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_deploy_stop_and_drain_keep_exact_native_worker_lifecycle_parents() {
    exercise(fixture(), false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_deploy_stop_and_drain_keep_exact_native_guest_lifecycle_parents() {
    exercise(
        fixture().with_file_name(format!(
            "libparrot_abi_recordingcomp.{}",
            std::env::consts::DLL_EXTENSION,
        )),
        true,
    )
    .await;
}

async fn exercise(image: PathBuf, guest: bool) {
    let digest = format!(
        "sha256:{:x}",
        Sha256::digest(std::fs::read(&image).unwrap())
    );
    // SAFETY: this test builds and owns the non-unwinding repository fixture.
    let trust = unsafe { NativeTrust::authorize(std::slice::from_ref(&digest)) }.unwrap();
    let engine = parrot::thread::system::ThreadActorSystem::shared(Default::default());
    let executor = NodeComponentExecutor::with_native_trust(engine.clone(), &trust).unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        for drain in [false, true] {
            let name = if drain { "native-drain" } else { "native-stop" };
            let path = format!("/user/{name}");
            let command = ComponentDeploy {
                name: name.into(),
                version: "1.0.0".into(),
                artifact: AdminArtifactRef::Dylib {
                    digest: digest.clone(),
                    uri: url::Url::from_file_path(&image).unwrap().into(),
                    abi: 1,
                },
                instances: AdminInstancePolicy::Singleton,
                config: guest.then(|| b"lifecycle = true\n".to_vec()),
            };
            let recorder = TraceRecorder::new(32).unwrap();
            let deploy_root = recorder.root();
            let deployed = bounded(deploy_root.scope(executor.deploy(1, &command))).await;
            assert!(
                matches!(deployed, AdminReplyV2::Deployed { .. }),
                "{deployed:?}"
            );
            let actor = engine.get_actor_ref(&path).unwrap();
            assert_eq!(
                bounded(actor.ask(DylibAsk {
                    type_key: "echo".into(),
                    payload: b"initialized".to_vec(),
                }))
                .await
                .unwrap()
                .0,
                b"initialized"
            );
            let stop_root = recorder.root();
            if drain {
                assert!(matches!(
                    bounded(stop_root.scope(executor.drain(2, &path, 5_000))).await,
                    AdminReplyV2::Drained {
                        drained: 1,
                        aborted: 0,
                        ..
                    }
                ));
            } else {
                assert!(matches!(
                    bounded(stop_root.scope(executor.stop(2, &path))).await,
                    AdminReplyV2::Stopped { .. }
                ));
            }
            assert!(engine.get_actor_ref(&path).is_none());
            recorder.close();
            let snapshot = recorder.snapshot();
            assert!(snapshot.terminal);
            assert_eq!(snapshot.counts.overwritten, 0);
            let find = |kind, target: &str| {
                let spans: Vec<_> = snapshot
                    .records
                    .iter()
                    .filter(|span| span.kind == kind && span.target == target)
                    .collect();
                assert_eq!(spans.len(), 1, "one actual {kind:?} {target}");
                spans[0]
            };
            let deploy = find(SpanKind::ManagedTask, "node/deploy");
            let init = find(SpanKind::ActorInit, &path);
            assert_eq!(deploy.parent_span_id, Some(deploy_root.span_id()));
            assert_eq!(init.parent_span_id, Some(deploy.span_id));
            for phase in ["load", "construct"] {
                let native = find(SpanKind::NativeLifecycle, &format!("native/{phase}:{path}"));
                assert_eq!(native.trace_id, deploy_root.trace_id());
                assert_eq!(native.parent_span_id, Some(init.span_id));
                assert_ne!(native.span_id, init.span_id);
                assert_eq!(native.status, SpanStatus::Ok);
            }
            let management = find(
                SpanKind::ManagedTask,
                if drain { "node/drain" } else { "node/stop" },
            );
            let before_stop = find(SpanKind::ActorBeforeStop, &path);
            let retire = find(SpanKind::NativeLifecycle, &format!("native/retire:{path}"));
            let native_drain = find(SpanKind::NativeLifecycle, &format!("native/drain:{path}"));
            assert_eq!(management.parent_span_id, Some(stop_root.span_id()));
            assert_eq!(before_stop.parent_span_id, Some(management.span_id));
            assert_eq!(retire.parent_span_id, Some(before_stop.span_id));
            assert_eq!(native_drain.parent_span_id, Some(retire.span_id));
            assert_ne!(deploy_root.trace_id(), stop_root.trace_id());
            for span in [management, before_stop, retire, native_drain] {
                assert_eq!(span.trace_id, stop_root.trace_id());
                assert_eq!(span.status, SpanStatus::Ok);
            }
            assert_eq!(
                snapshot
                    .records
                    .iter()
                    .filter(|span| span.kind == SpanKind::NativeLifecycle)
                    .count(),
                4
            );
            if guest {
                for (phase, host_phase, trace) in [
                    ("construct", "construct", deploy_root.trace_id()),
                    ("drain", "drain", stop_root.trace_id()),
                    ("destroy", "retire", stop_root.trace_id()),
                ] {
                    let host = find(
                        SpanKind::NativeLifecycle,
                        &format!("native/{host_phase}:{path}"),
                    );
                    let outer = find(SpanKind::NativeGuest, &format!("guest/{phase}"));
                    let nested = find(SpanKind::NativeGuest, &format!("guest/{phase}/nested"));
                    let unfinished =
                        find(SpanKind::NativeGuest, &format!("guest/{phase}/unfinished"));
                    assert_eq!(outer.trace_id, trace);
                    assert_eq!(outer.parent_span_id, Some(host.span_id));
                    assert_eq!(nested.parent_span_id, Some(outer.span_id));
                    assert_eq!(unfinished.parent_span_id, Some(host.span_id));
                    assert_eq!(outer.status, SpanStatus::Ok);
                    assert_eq!(nested.status, SpanStatus::Ok);
                    assert_eq!(unfinished.status, SpanStatus::Cancelled);
                }
                assert_eq!(
                    snapshot
                        .records
                        .iter()
                        .filter(|span| span.kind == SpanKind::NativeGuest)
                        .count(),
                    9
                );
            }
        }
    })
    .catch_unwind()
    .await;
    bounded(engine.shutdown_internal()).await.unwrap();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}
