//! Pure state-machine fault contracts. These tests do not kill OS processes,
//! partition a Raft cluster, or generate a message storm. Real process and actor
//! fault tests are in supervisor_contracts.rs and test_assemble_helper.rs; actual
//! Dylib/Wasm fault execution belongs to their runtime integration suites.

use parrot_app::manifest::{AppManifest, ArtifactRef, ComponentSpec, EngineKind, InstancePolicy};
use parrot_app::orchestrator::health::{diff_links, HealthWatch};
use parrot_app::orchestrator::rollout::{
    RolloutAction, RolloutEvent, RolloutPhase, RolloutTracker,
};
use parrot_app::orchestrator::supervisor::{
    AppSupervisor, MemStateStore, ObservedState, ReconcileAction,
};
use parrot_remote::admin_v2::ComponentStateReport;
use std::sync::Arc;

fn spec(name: &str) -> ComponentSpec {
    ComponentSpec {
        name: name.into(),
        engine: EngineKind::Erlang,
        artifact: ArtifactRef::Beam {
            app: name.into(),
            uri: None,
        },
        alt_artifact: None,
        instances: InstancePolicy::Singleton,
        placement: Default::default(),
        upgrade: Default::default(),
        deps: vec![],
        config: None,
        hooks: Default::default(),
    }
}

fn manifest(comps: Vec<ComponentSpec>) -> AppManifest {
    AppManifest {
        name: "chaos-app".into(),
        version: "1.0.0".into(),
        components: comps,
        wiring: vec![],
        mirrors: Vec::new(),
        config_overlay: None,
    }
}

fn sup(m: AppManifest) -> AppSupervisor {
    AppSupervisor::new(
        m,
        Arc::new(MemStateStore::new()),
        Arc::new(parrot_app::orchestrator::SystemClock),
    )
    .unwrap()
}

/// 场景 1：升级中 kill -9 目标 Executor → RolloutTracker 自动回滚 →
/// App 恢复 Running（旧版本继续服务）。
#[test]
fn rollout_node_loss_requests_rollback() {
    let mut t = RolloutTracker::new("frontier");
    // 升级进行到 Deploying（drain 完成、新版本部署中）
    t.advance(RolloutEvent::PlanReady).unwrap();
    t.advance(RolloutEvent::Drained { aborted: 0 }).unwrap();
    assert!(t.in_phase(RolloutPhase::Deploying));
    // kill -9：节点消失
    let a = t
        .advance(RolloutEvent::NodeLost("erl-gw-1".into()))
        .unwrap();
    assert!(matches!(a, RolloutAction::Rollback { .. }));
    // 回滚完成 → Done；旧版本恢复（App 回到 Running 由 supervisor 下轮确认）
    t.advance(RolloutEvent::Drained { aborted: 0 }).unwrap();
    assert!(t.in_phase(RolloutPhase::Done));
    // supervisor 视角：旧版本 observed running → converged
    let s = sup(manifest(vec![spec("frontier")]));
    let mut o = ObservedState::default();
    o.components.insert(
        "frontier".into(),
        vec![ComponentStateReport {
            path: "/user/frontier".into(),
            state: "running".into(),
            version: "1.0.0".into(),
        }],
    );
    let r = s.reconcile_once(&o);
    assert!(r.converged, "回滚后旧版本服务正常");
}

/// A stale supervisor sharing the same committed store must refresh before
/// planning deployment. This is a revision fence, not proof of Raft quorum.
#[test]
fn stale_supervisor_waits_after_another_owner_commits() {
    let initial = manifest(vec![spec("frontier")]);
    let store = Arc::new(MemStateStore::new());
    let mut writer = AppSupervisor::new(
        initial.clone(),
        store.clone(),
        Arc::new(parrot_app::orchestrator::SystemClock),
    )
    .unwrap();
    let stale = AppSupervisor::new(
        initial.clone(),
        store,
        Arc::new(parrot_app::orchestrator::SystemClock),
    )
    .unwrap();
    let mut update = initial;
    update.version = "2.0.0".into();
    writer.submit(update).unwrap();
    let report = stale.reconcile_once(&ObservedState::default());
    assert!(!report.converged);
    assert!(!report.actions.is_empty());
    assert!(report
        .actions
        .iter()
        .all(|action| matches!(action, ReconcileAction::Wait { .. })));
}

/// 场景 3：网关双杀（akka+ray 同时）→ 降级清单正确 → 恢复自动 reconcile。
#[test]
fn link_snapshot_loss_degrades_then_status_recovers() {
    let mut h = HealthWatch::new(vec!["akka-gw-1".into(), "ray-gw-1".into()]).unwrap();
    h.register_node_components("akka-gw-1", vec!["search".into()])
        .unwrap();
    h.register_node_components("ray-gw-1", vec!["parse".into()])
        .unwrap();
    let poll = h.begin_poll("akka-gw-1", "search").unwrap();
    h.apply_poll(poll, vec![rep("/user/search", "running")])
        .unwrap();
    let poll = h.begin_poll("ray-gw-1", "parse").unwrap();
    h.apply_poll(poll, vec![rep("/user/parse", "running")])
        .unwrap();
    // 双杀
    let d = h.apply_links(vec![]).unwrap();
    assert_eq!(d.left.len(), 2, "双网关同时离线");
    let o = h.observed();
    assert!(
        !o.any_running("search") && !o.any_running("parse"),
        "降级清单含全部受影响组件"
    );
    // 恢复（网关重启 + 轮询回执）
    h.apply_links(vec!["akka-gw-1".into(), "ray-gw-1".into()])
        .unwrap();
    let poll = h.begin_poll("akka-gw-1", "search").unwrap();
    h.apply_poll(poll, vec![rep("/user/search", "running")])
        .unwrap();
    let poll = h.begin_poll("ray-gw-1", "parse").unwrap();
    h.apply_poll(poll, vec![rep("/user/parse", "running")])
        .unwrap();
    let o2 = h.observed();
    assert!(
        o2.any_running("search") && o2.any_running("parse"),
        "恢复后自动 reconcile 数据面就绪"
    );
}

/// 场景 4：drain 半程消息风暴——超时兜底触发，DRAIN_ABORTED 计数=预期。
#[test]
fn rollout_accounts_executor_reported_aborts() {
    let mut t = RolloutTracker::new("index");
    t.advance(RolloutEvent::PlanReady).unwrap();
    // 风暴：drain 超时，in-flight 消息按监督策略中止——计数 42
    let a = t.advance(RolloutEvent::Drained { aborted: 42 }).unwrap();
    assert_eq!(
        a,
        RolloutAction::StartDeploy {
            comp: "index".into()
        }
    );
    assert_eq!(t.aborted_total, 42, "DRAIN_ABORTED 计数必须准确");
    // 前进不受阻（兜底语义）
    assert!(t.in_phase(RolloutPhase::Deploying));
}

fn rep(path: &str, state: &str) -> ComponentStateReport {
    ComponentStateReport {
        path: path.into(),
        state: state.into(),
        version: "1.0.0".into(),
    }
}

/// 补：diff_links 双杀形态直测（场景 3 的纯函数面）。
#[test]
fn chaos_diff_dual_kill() {
    let d = diff_links(&["akka-gw-1".into(), "ray-gw-1".into()], &[]);
    assert_eq!(d.joined.len(), 0);
    assert_eq!(
        d.left,
        vec!["akka-gw-1".to_string(), "ray-gw-1".to_string()]
    );
}
