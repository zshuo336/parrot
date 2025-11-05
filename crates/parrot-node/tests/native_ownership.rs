#![cfg(feature = "dylib")]
#[path = "support/native.rs"]
mod native;
use native::NativeFixture;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::address::ActorRefExt;
use parrot_node::{DylibAsk, DylibTell, NodeComponentExecutor};
use parrot_remote::admin_v2::{AdminReplyV2 as R, ComponentExecutor};
use std::{sync::Arc, time::Duration};

fn executor(fixture: &NativeFixture) -> (Arc<ThreadActorSystem>, NodeComponentExecutor) {
    let engine = ThreadActorSystem::shared(Default::default());
    let executor =
        NodeComponentExecutor::with_native_trust(engine.clone(), &fixture.trust()).unwrap();
    (engine, executor)
}
async fn value(engine: &ThreadActorSystem, path: &str) -> u64 {
    let bytes = engine
        .get_actor_ref(path)
        .unwrap()
        .ask(DylibAsk {
            type_key: "value".into(),
            payload: vec![],
        })
        .await
        .unwrap()
        .0;
    u64::from_le_bytes(bytes.try_into().unwrap())
}
fn deployed(reply: R, count: usize) {
    match reply {
        R::Deployed { instances, .. } => assert_eq!(instances.len(), count),
        other => panic!("{other:?}"),
    }
}
async fn wait_gone(executor: &NodeComponentExecutor, root: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                executor.status(3, root).await,
                R::Failed {
                    code: parrot_remote::admin_v2::v2_err::COMPONENT_NOT_FOUND,
                    ..
                }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn explicit_trust_is_exact_and_plain_executor_still_denies_native_code() {
    let fixture = NativeFixture::new();
    let engine = ThreadActorSystem::shared(Default::default());
    let plain = NodeComponentExecutor::new(engine.clone());
    let command = fixture.component("denied", 1);
    let reply = plain.deploy(1, &command).await;
    assert!(
        matches!(reply, R::Failed { ref detail, .. } if detail.contains("trust authorization")),
        "{reply:?}"
    );
    let trust = fixture.trust();
    let upper = fixture
        .digest
        .strip_prefix("sha256:")
        .unwrap()
        .to_ascii_uppercase();
    let normalized =
        unsafe { parrot_node::NativeTrust::authorize(&[upper, fixture.digest.clone()]) }.unwrap();
    assert_eq!(trust, normalized);
    let authorized = NodeComponentExecutor::with_native_trust(engine.clone(), &trust).unwrap();
    let mut denied = command.clone();
    if let parrot_remote::admin_v2::AdminArtifactRef::Dylib { digest, .. } = &mut denied.artifact {
        *digest = "0".repeat(64);
    }
    assert!(
        matches!(authorized.deploy(2, &denied).await, R::Failed { ref detail, .. } if detail.contains("trust authorization"))
    );
    assert!(fixture.events().is_empty());
    deployed(authorized.deploy(3, &command).await, 1);
    assert!(matches!(
        authorized.stop(4, "/user/denied").await,
        R::Stopped { .. }
    ));
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn native_pool_preserves_state_versions_config_and_exact_member_ownership() {
    let fixture = NativeFixture::new();
    let (engine, executor) = executor(&fixture);
    let command = fixture.component("native-2026", 2);
    deployed(executor.deploy(1, &command).await, 2);
    let actor = engine.get_actor_ref("/user/native-2026-0").unwrap();
    let config = actor
        .ask(DylibAsk {
            type_key: "config".into(),
            payload: vec![],
        })
        .await
        .unwrap()
        .0;
    let envelope: toml::Table = toml::from_str(std::str::from_utf8(&config).unwrap()).unwrap();
    assert_eq!(envelope["self_path"].as_str(), Some("/user/native-2026-0"));
    assert_eq!(envelope["config"].as_table(), Some(&fixture.config));
    actor
        .ask(DylibTell {
            type_key: "set".into(),
            payload: 42u64.to_le_bytes().to_vec(),
        })
        .await
        .unwrap();
    deployed(executor.clone().deploy(2, &command).await, 2);
    assert_eq!(value(&engine, "/user/native-2026-0").await, 42);
    assert_eq!(value(&engine, "/user/native-2026-1").await, 0);
    match executor.status(3, "/user/native-2026").await {
        R::Status { states, .. } => assert!(states
            .iter()
            .all(|s| s.version == "2.3.4" && s.state == "running")),
        other => panic!("{other:?}"),
    }
    let mut changed = command.clone();
    changed.version = "2.3.5".into();
    assert!(matches!(
        executor.deploy(4, &changed).await,
        R::Failed { .. }
    ));
    assert!(matches!(
        executor.stop(5, "/user/native-2026-1").await,
        R::Stopped { .. }
    ));
    assert_eq!(value(&engine, "/user/native-2026-0").await, 42);
    assert!(matches!(
        executor.deploy(6, &command).await,
        R::Failed { .. }
    ));
    assert!(matches!(
        executor.stop(7, "/user/native-2026").await,
        R::Stopped { .. }
    ));
    deployed(executor.deploy(8, &changed).await, 2);
    assert_eq!(value(&engine, "/user/native-2026-0").await, 0);
    assert!(matches!(
        executor.drain(9, "/user/native-2026", 5000).await,
        R::Drained {
            drained: 2,
            aborted: 0,
            ..
        }
    ));
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn native_partial_pool_failure_rolls_back_without_losing_instances() {
    let mut fixture = NativeFixture::new();
    fixture.config.insert(
        "fail_path".into(),
        toml::Value::String("/user/partial-2".into()),
    );
    let (engine, executor) = executor(&fixture);
    assert!(matches!(
        executor.deploy(1, &fixture.component("partial", 3)).await,
        R::Failed { .. }
    ));
    assert_eq!(
        fixture
            .events()
            .lines()
            .filter(|l| l.starts_with("destroy:"))
            .collect::<Vec<_>>(),
        ["destroy:/user/partial-1", "destroy:/user/partial-0"]
    );
    wait_gone(&executor, "/user/partial").await;
    for i in 0..3 {
        assert!(engine
            .get_actor_ref(&format!("/user/partial-{i}"))
            .is_none());
    }
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancelling_admin_deploy_keeps_the_accepted_native_deployment_owned() {
    let mut fixture = NativeFixture::new();
    let gate = fixture.gate("construct_gate");
    let (engine, executor) = executor(&fixture);
    let command = fixture.component("cancelled", 2);
    let owner = executor.clone();
    let job = tokio::spawn(async move { owner.deploy(1, &command).await });
    fixture.wait_for("construct-enter:/user/cancelled-0").await;
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    match executor.status(2, "/user/cancelled").await {
        R::Status { states, .. } => {
            assert_eq!(states.len(), 2);
            assert!(states.iter().all(|s| s.state == "deploying"));
        }
        other => panic!("{other:?}"),
    }
    std::fs::write(gate, b"release").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(executor.status(3,"/user/cancelled").await,R::Status{states,..} if states.len()==2 && states.iter().all(|s|s.state=="running")) { break; }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }).await.unwrap();
    assert_eq!(value(&engine, "/user/cancelled-1").await, 0);
    assert!(matches!(
        executor.stop(4, "/user/cancelled").await,
        R::Stopped { .. }
    ));
    assert_eq!(
        fixture
            .events()
            .lines()
            .filter(|l| l.starts_with("destroy:"))
            .count(),
        2
    );
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancelled_native_stop_retains_capacity_until_actual_cleanup_and_then_releases_it() {
    let mut fixture = NativeFixture::new();
    let gate = fixture.gate("drain_gate");
    let (engine, executor) = executor(&fixture);
    deployed(executor.deploy(1, &fixture.component("full", 32)).await, 32);
    let stopper = executor.clone();
    let stop = tokio::spawn(async move { stopper.stop(2, "/user/full-0").await });
    fixture.wait_for("drain-enter:/user/full-0").await;
    stop.abort();
    assert!(stop.await.unwrap_err().is_cancelled());
    let reply = executor
        .clone()
        .deploy(3, &fixture.component("overflow", 1))
        .await;
    assert!(
        matches!(reply,R::Failed{ref detail,..} if detail.contains("worker capacity")),
        "{reply:?}"
    );
    assert!(!fixture.events().contains("construct:/user/overflow"));
    std::fs::write(gate, b"release").unwrap();
    wait_gone(&executor, "/user/full-0").await;
    deployed(
        executor
            .clone()
            .deploy(4, &fixture.component("available", 1))
            .await,
        1,
    );
    assert!(matches!(executor.stop(5, "/user").await, R::Stopped { .. }));
    assert_eq!(
        fixture
            .events()
            .lines()
            .filter(|l| l.starts_with("destroy:"))
            .count(),
        33
    );
    engine.shutdown_internal().await.unwrap();
}
