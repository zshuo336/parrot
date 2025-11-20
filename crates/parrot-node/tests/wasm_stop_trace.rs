//! Node management ownership reaches the real guest drain through engine stop.
#![cfg(feature = "wasm")]
use futures::FutureExt;
use parrot_api::{
    address::ActorRefExt,
    trace::{SpanKind, SpanStatus, TraceRecorder},
};
use parrot_node::{NodeComponentExecutor, WasmAsk};
use parrot_remote::admin_v2::{
    AdminArtifactRef, AdminInstancePolicy, AdminReplyV2, ComponentDeploy, ComponentExecutor,
};
use sha2::{Digest, Sha256};
use std::{future::Future, time::Duration};

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .expect("Node Wasm stop trace deadline")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_owns_required_recovery_before_actual_guest_drain() {
    let engine =
        parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
            default_supervisor_strategy: parrot::thread::SupervisorStrategy::Resume,
            ..Default::default()
        });
    let executor =
        NodeComponentExecutor::with_host_options(engine.clone(), Some(1000), &Default::default())
            .unwrap();
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../parrot-wasm/tests/fixtures/recording-component.wasm")
        .canonicalize()
        .unwrap();
    let command = ComponentDeploy {
        name: "recovery-stop".into(),
        version: "1.0.0".into(),
        artifact: AdminArtifactRef::Wasm {
            digest: format!(
                "sha256:{:x}",
                Sha256::digest(std::fs::read(&artifact).unwrap())
            ),
            uri: url::Url::from_file_path(artifact).unwrap().into(),
        },
        instances: AdminInstancePolicy::Singleton,
        config: None,
    };
    let result = std::panic::AssertUnwindSafe(async {
        assert!(matches!(
            bounded(executor.deploy(1, &command)).await,
            AdminReplyV2::Deployed { .. }
        ));
        let recorder = TraceRecorder::new(32).unwrap();
        let actor = engine.get_actor_ref("/user/recovery-stop").unwrap();
        assert!(bounded(recorder.root().scope(async {
            actor
                .ask(WasmAsk {
                    type_key: "trap".into(),
                    payload: vec![],
                })
                .await
        }))
        .await
        .is_err());
        let root = recorder.root();
        assert!(matches!(
            bounded(root.scope(executor.stop(2, "/user/recovery-stop"))).await,
            AdminReplyV2::Stopped { .. }
        ));
        assert!(engine.get_actor_ref("/user/recovery-stop").is_none());
        recorder.close();
        let snapshot = recorder.snapshot();
        let named = |target: &str| {
            snapshot
                .records
                .iter()
                .find(|record| record.target == target && record.trace_id == root.trace_id())
                .unwrap()
        };
        let management = named("node/stop");
        let before_stop = snapshot
            .records
            .iter()
            .find(|record| record.kind == SpanKind::ActorBeforeStop)
            .unwrap();
        let drain = named("wasm/on-drain:/user/recovery-stop");
        let recovery = named("wasm/recovery-start:/user/recovery-stop");
        let startup = named("guest/on-start");
        let guest_drain = named("guest/on-drain");
        assert_eq!(management.parent_span_id, Some(root.span_id()));
        assert_eq!(before_stop.parent_span_id, Some(management.span_id));
        assert_eq!(drain.parent_span_id, Some(before_stop.span_id));
        assert_eq!(recovery.parent_span_id, Some(drain.span_id));
        assert_eq!(startup.parent_span_id, Some(recovery.span_id));
        assert_eq!(guest_drain.parent_span_id, Some(drain.span_id));
        assert!(startup.sequence < guest_drain.sequence);
        for record in [
            management,
            before_stop,
            drain,
            recovery,
            startup,
            guest_drain,
        ] {
            assert_eq!(record.trace_id, root.trace_id());
            assert_eq!(record.status, SpanStatus::Ok);
        }
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 8);
        assert_eq!(snapshot.counts.completed, 8);
        assert_eq!(snapshot.counts.overwritten, 0);
    })
    .catch_unwind()
    .await;
    bounded(engine.shutdown_internal()).await.unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_and_drain_reach_actual_wasm_lifecycle_and_guest_children() {
    // Trace ownership needs a successful guest call, independent of the default
    // one-tick sandbox race. Exercise the real Node host-policy configuration.
    let settings = parrot_node::configuration::NodeSettings {
        resolved: parrot_config::ParrotConfig::builder()
            .remote_node_id("wasm-stop-trace")
            .remote_bind("127.0.0.1:0")
            .build()
            .unwrap(),
        actors: vec![],
        ready_file: None,
        wasm_epoch_ticks: Some(1000),
    };
    let node = bounded(parrot_node::runtime::RunningNode::start(&settings))
        .await
        .unwrap();
    let engine = &node.thread;
    let executor = NodeComponentExecutor::new(engine.clone());
    let result = std::panic::AssertUnwindSafe(async {
        for drain in [false, true] {
            let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../parrot-wasm/tests/fixtures/recording-component.wasm")
                .canonicalize()
                .unwrap();
            let name = if drain { "trace-drain" } else { "trace-stop" };
            let path = format!("/user/{name}");
            let command = ComponentDeploy {
                name: name.into(),
                version: "1.0.0".into(),
                artifact: AdminArtifactRef::Wasm {
                    digest: format!(
                        "sha256:{:x}",
                        Sha256::digest(std::fs::read(&artifact).unwrap())
                    ),
                    uri: url::Url::from_file_path(artifact).unwrap().into(),
                },
                instances: AdminInstancePolicy::Singleton,
                config: None,
            };
            let recorder = TraceRecorder::new(32).unwrap();
            let deploy_root = recorder.root();
            let deployed = bounded(deploy_root.scope(executor.deploy(1, &command))).await;
            assert!(
                matches!(deployed, AdminReplyV2::Deployed { .. }),
                "{deployed:?}"
            );
            let actor = engine.get_actor_ref(&path).unwrap();
            // An actual guest call proves initialization finished before stop.
            bounded(actor.ask(WasmAsk {
                type_key: "start-identity".into(),
                payload: vec![],
            }))
            .await
            .unwrap();
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
            let snapshot = recorder.snapshot();
            assert_eq!(snapshot.counts.in_flight, 0);
            let management = snapshot
                .records
                .iter()
                .find(|record| {
                    record.kind == SpanKind::ManagedTask
                        && record.target == if drain { "node/drain" } else { "node/stop" }
                })
                .expect("missing actual Node management owner");
            let host = snapshot
                .records
                .iter()
                .find(|record| {
                    record.kind == SpanKind::WasmLifecycle
                        && record.target == format!("wasm/on-drain:{path}")
                })
                .expect("engine stop lost identity before real Wasm on_drain");
            let hook = snapshot
                .records
                .iter()
                .find(|record| record.kind == SpanKind::ActorBeforeStop && record.target == path)
                .expect("missing actual engine before_stop hook");
            let guest = snapshot
                .records
                .iter()
                .find(|record| {
                    record.kind == SpanKind::WasmGuest && record.target == "guest/on-drain"
                })
                .expect("real guest drain did not record its child");
            assert_eq!(management.parent_span_id, Some(stop_root.span_id()));
            assert_eq!(hook.parent_span_id, Some(management.span_id));
            assert_eq!(host.parent_span_id, Some(hook.span_id));
            assert_eq!(guest.parent_span_id, Some(host.span_id));
            assert_ne!(stop_root.trace_id(), deploy_root.trace_id());
            for record in [management, hook, host, guest] {
                assert_eq!(record.trace_id, stop_root.trace_id());
                assert_eq!(record.status, SpanStatus::Ok);
            }
            let deploy = snapshot
                .records
                .iter()
                .find(|record| {
                    record.kind == SpanKind::ManagedTask && record.target == "node/deploy"
                })
                .expect("missing actual Node deployment owner");
            let init = snapshot
                .records
                .iter()
                .find(|record| record.kind == SpanKind::ActorInit && record.target == path)
                .expect("missing actual engine initialization hook");
            let startup = snapshot
                .records
                .iter()
                .find(|record| {
                    record.kind == SpanKind::WasmLifecycle
                        && record.target == format!("wasm/on-start:{path}")
                })
                .expect("missing actual guest startup");
            assert_eq!(deploy.parent_span_id, Some(deploy_root.span_id()));
            assert_eq!(init.parent_span_id, Some(deploy.span_id));
            assert_eq!(startup.parent_span_id, Some(init.span_id));
            for record in [deploy, init, startup] {
                assert_eq!(record.trace_id, deploy_root.trace_id());
                assert_eq!(record.status, SpanStatus::Ok);
            }
            assert_eq!(
                snapshot
                    .records
                    .iter()
                    .filter(|record| {
                        matches!(record.kind, SpanKind::ActorInit | SpanKind::ActorBeforeStop)
                    })
                    .count(),
                2
            );
            assert_eq!(
                snapshot
                    .records
                    .iter()
                    .filter(|record| record.kind == SpanKind::WasmLifecycle)
                    .count(),
                2
            );
            assert_eq!(
                snapshot
                    .records
                    .iter()
                    .filter(|record| record.kind == SpanKind::WasmGuest)
                    .count(),
                2
            );
        }
    })
    .catch_unwind()
    .await;
    bounded(node.shutdown()).await.unwrap();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}
