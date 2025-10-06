//! F 阶段（DEV_09 §3.6 / 09 §11.2.3）：六混沌场景之 Rust 可测面。
//!
//! | 场景 | 落点 | 断言 |
//! |---|---|---|
//! | 1. 升级中 kill -9 目标 Executor | RolloutTracker NodeLost 弧 | 自动 Rollback，恢复 Running |
//! | 2. Orchestrator 分区（多数派安全） | supervisor 分区形态 | 少数派不下发部署 |
//! | 3. 网关双杀 | HealthWatch 双断连 | 降级清单正确，恢复自动 reconcile |
//! | 4. drain 半程消息风暴 | rollout Drained{aborted} 弧 | 超时兜底 + DRAIN_ABORTED 计数 |
//! | 5. dylib 卸载后 dlopen 新版 | parrot-abi（D 阶段已测——此处引用断言） | 全局无污染 |
//! | 6. wasm fuel 风暴 | parrot-wasm（C 阶段已测——此处引用断言） | OverQuota 限频不影响他人 |

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
        artifact: ArtifactRef::Beam { app: name.into(), uri: None },
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
        config_overlay: None,
    }
}

fn sup(m: AppManifest) -> AppSupervisor {
    AppSupervisor::new(
        m,
        Arc::new(MemStateStore::new()),
        Arc::new(parrot_app::orchestrator::SystemClock),
    )
}

/// 场景 1：升级中 kill -9 目标 Executor → RolloutTracker 自动回滚 →
/// App 恢复 Running（旧版本继续服务）。
#[test]
fn chaos_upgrade_kill9_autorollback() {
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
            version: "1".into(),
        }],
    );
    let r = s.reconcile_once(&o);
    assert!(r.converged, "回滚后旧版本服务正常");
}

/// 场景 2：Orchestrator 分区——少数派侧 desired 停留旧版（Raft 提交
/// 不前进）；多数派提交新 desired。断言：少数派对新版本零感知零部署
/// （desired 持久化安全——分区不产生分裂部署）。
#[test]
fn chaos_partition_minority_no_deploys() {
    // 多数派：经 submit 提交新 desired（升级入口——登记在途）
    let mut majority = sup(manifest(vec![spec("frontier")]));
    let mut v2 = spec("frontier");
    v2.artifact = ArtifactRef::Beam { app: "frontier-v2".into(), uri: None };
    majority.submit(manifest(vec![v2])).unwrap();
    // 少数派：旧 desired（分区——Raft 提交未达其 store）
    let minority = sup(manifest(vec![spec("frontier")]));
    // 双方 observed：v1 运行中（分区前共同视图）
    let mut o = ObservedState::default();
    o.components.insert(
        "frontier".into(),
        vec![ComponentStateReport {
            path: "/user/frontier".into(),
            state: "running".into(),
            version: "1".into(),
        }],
    );
    // 少数派：v1 desired vs v1 observed → 零动作（converged）
    let r = minority.reconcile_once(&o);
    assert!(
        r.actions.is_empty(),
        "分区少数派不得下发任何部署: {:?}",
        r.actions
    );
    assert!(r.converged);
    // 多数派：v2 在途 → Wait（升级由 RolloutTracker 驱动——非盲 Deploy）
    let rm = majority.reconcile_once(&o);
    assert!(
        !rm.converged
            && rm
                .actions
                .iter()
                .all(|a| matches!(a, ReconcileAction::Wait { .. })),
        "多数派升级在途: {:?}",
        rm.actions
    );
}

/// 场景 3：网关双杀（akka+ray 同时）→ 降级清单正确 → 恢复自动 reconcile。
#[test]
fn chaos_dual_gateway_kill_degrades_then_recovers() {
    let mut h = HealthWatch::new(vec!["akka-gw-1".into(), "ray-gw-1".into()]);
    h.register_node_components("akka-gw-1", vec!["search".into()]);
    h.register_node_components("ray-gw-1", vec!["parse".into()]);
    h.apply_status("search", vec![rep("/user/search", "running")]);
    h.apply_status("parse", vec![rep("/user/parse", "running")]);
    // 双杀
    let d = h.apply_links(vec![]);
    assert_eq!(d.left.len(), 2, "双网关同时离线");
    let o = h.observed();
    assert!(
        !o.any_running("search") && !o.any_running("parse"),
        "降级清单含全部受影响组件"
    );
    // 恢复（网关重启 + 轮询回执）
    h.apply_links(vec!["akka-gw-1".into(), "ray-gw-1".into()]);
    h.apply_status("search", vec![rep("/user/search", "running")]);
    h.apply_status("parse", vec![rep("/user/parse", "running")]);
    let o2 = h.observed();
    assert!(
        o2.any_running("search") && o2.any_running("parse"),
        "恢复后自动 reconcile 数据面就绪"
    );
}

/// 场景 4：drain 半程消息风暴——超时兜底触发，DRAIN_ABORTED 计数=预期。
#[test]
fn chaos_drain_storm_timeout_abort_counted() {
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

/// 场景 5+6 引用断言：dylib/wasm 沙箱面在各自 crate 测试
/// （parrot-abi loader_tests::unload_then_reload_fresh_state 与
/// parrot-wasm runtime_tests 燃料组）——此处静态存在性锚定
/// （防误删测试锚点）。
#[test]
fn chaos_anchors_dylib_and_wasm_gates_exist() {
    // parrot-abi：四步卸载 + 重载无污染
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../parrot-abi/tests/loader_tests.rs");
    let t = std::fs::read_to_string(&p).unwrap();
    assert!(
        t.contains("fn unload_then_reload_fresh_state"),
        "dylib 重载门禁锚点缺失"
    );
    // parrot-wasm：fuel 风暴
    let p2 = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../parrot-wasm/tests/runtime_tests.rs");
    let t2 = std::fs::read_to_string(&p2).unwrap();
    assert!(t2.contains("fuel"), "wasm fuel 门禁锚点缺失");
}

fn rep(path: &str, state: &str) -> ComponentStateReport {
    ComponentStateReport {
        path: path.into(),
        state: state.into(),
        version: "1".into(),
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
