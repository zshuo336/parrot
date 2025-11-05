use parrot_app::orchestrator::{
    AppSupervisor, FakeClock, HealthWatch, MemStateStore, ObservedState, ReconcileAction,
};
use parrot_remote::admin_v2::ComponentStateReport;
use std::sync::Arc;
fn rows() -> Vec<ComponentStateReport> {
    vec![ComponentStateReport {
        path: "/user/a".into(),
        state: "running".into(),
        version: "1.0.0".into(),
    }]
}
fn watch() -> HealthWatch {
    let mut h = HealthWatch::new(vec!["parrot-n1".into(), "parrot-n2".into()]).unwrap();
    h.register_node_components("parrot-n1", vec!["a".into()])
        .unwrap();
    h
}
fn accept(h: &mut HealthWatch, node: &str) {
    let p = h.begin_poll(node, "a").unwrap();
    h.apply_poll(p, rows()).unwrap();
}
#[test]
fn disconnected_node_cannot_be_resurrected_by_a_late_status() {
    let mut h = watch();
    accept(&mut h, "parrot-n1");
    assert!(h.observed().any_running("a"));
    let late = h.begin_poll("parrot-n1", "a").unwrap();
    h.apply_links(vec!["parrot-n2".into()]).unwrap();
    assert!(h.apply_poll(late, rows()).is_err());
    assert!(!h.observed().any_running("a"));
}
#[test]
fn previous_node_departure_cannot_downgrade_reassigned_component() {
    let mut h = watch();
    accept(&mut h, "parrot-n1");
    let late = h.begin_poll("parrot-n1", "a").unwrap();
    h.register_node_components("parrot-n2", vec!["a".into()])
        .unwrap();
    assert!(!h.observed().any_running("a"));
    assert!(h.apply_poll(late, rows()).is_err());
    accept(&mut h, "parrot-n2");
    h.apply_links(vec!["parrot-n2".into()]).unwrap();
    assert!(h.observed().any_running("a"));
    assert_eq!(h.observed().component_nodes["a"], "parrot-n2");
}
#[test]
fn removing_assignment_invalidates_its_previous_status() {
    let mut h = watch();
    accept(&mut h, "parrot-n1");
    let late = h.begin_poll("parrot-n1", "a").unwrap();
    h.register_node_components("parrot-n1", vec![]).unwrap();
    assert!(!h.observed().any_running("a"));
    assert!(h.apply_poll(late, rows()).is_err());
    h.register_node_components("parrot-n1", vec!["a".into()])
        .unwrap();
    accept(&mut h, "parrot-n1");
}
#[test]
fn reconnect_fences_old_polls_even_if_node_name_returns() {
    let mut h = watch();
    let old = h.begin_poll("parrot-n1", "a").unwrap();
    h.apply_links(vec![]).unwrap();
    h.apply_links(vec!["parrot-n1".into()]).unwrap();
    assert!(h.apply_poll(old, rows()).is_err());
    accept(&mut h, "parrot-n1");
    let old = h.begin_poll("parrot-n1", "a").unwrap();
    h.invalidate_node("parrot-n1");
    assert!(!h.observed().any_running("a"));
    assert!(h.apply_poll(old, rows()).is_err());
    accept(&mut h, "parrot-n1");
}
#[test]
fn newer_polls_fence_reordered_and_duplicate_replies() {
    let mut h = watch();
    let old = h.begin_poll("parrot-n1", "a").unwrap();
    let new = h.begin_poll("parrot-n1", "a").unwrap();
    assert!(h.apply_poll(old, rows()).is_err());
    h.apply_poll(new.clone(), rows()).unwrap();
    assert!(h.apply_poll(new, vec![]).is_err());
    assert!(h.observed().any_running("a"));
}
#[test]
fn tickets_cannot_cross_watches_or_nodes_and_no_source_less_network_injection() {
    let mut a = watch();
    let mut b = watch();
    let pa = a.begin_poll("parrot-n1", "a").unwrap();
    let pb = b.begin_poll("parrot-n1", "a").unwrap();
    assert!(b.apply_poll(pa.clone(), rows()).is_err());
    assert!(a.begin_poll("parrot-n2", "a").is_err());
    assert!(a.apply_status("a", rows()).is_err());
    a.apply_poll(pa, rows()).unwrap();
    b.apply_poll(pb, rows()).unwrap();
}
#[test]
fn invalid_assignment_preserves_current_receipt_and_pending_poll() {
    let mut h = watch();
    accept(&mut h, "parrot-n1");
    let p = h.begin_poll("parrot-n1", "a").unwrap();
    for names in [vec!["a".into(), "a".into()], vec!["../a".into()]] {
        assert!(h.register_node_components("parrot-n2", names).is_err());
    }
    assert_eq!(h.observed().component_nodes["a"], "parrot-n1");
    h.apply_poll(p, rows()).unwrap();
}
#[test]
fn foreign_duplicate_and_oversized_reports_cannot_replace_good_snapshot() {
    let limits = parrot_app::ApplicationLimits {
        max_instances: 1,
        max_metadata_bytes: 512,
        ..Default::default()
    };
    let mut h = HealthWatch::with_limits(vec!["parrot-n1".into()], limits).unwrap();
    h.register_node_components("parrot-n1", vec!["a".into()])
        .unwrap();
    accept(&mut h, "parrot-n1");
    let p = h.begin_poll("parrot-n1", "a").unwrap();
    let mut foreign = rows();
    foreign[0].path = "/user/alien".into();
    let mut large = rows();
    large[0].version = "x".repeat(1024);
    for value in [foreign, large, [rows(), rows()].concat()] {
        assert!(h.apply_poll(p.clone(), value).is_err());
        assert!(h.observed().any_running("a"));
    }
    h.apply_poll(p, vec![]).unwrap();
    assert!(!h.observed().any_running("a"));
}
#[test]
fn limits_cover_nodes_assignments_local_rows_and_retained_history() {
    let limits = parrot_app::ApplicationLimits {
        max_candidates: 1,
        max_components: 1,
        max_instances: 1,
        ..Default::default()
    };
    assert!(HealthWatch::with_limits(vec!["a".into(), "b".into()], limits).is_err());
    let mut h = HealthWatch::with_limits(vec!["n".into()], limits).unwrap();
    h.register_node_components("n", vec!["a".into()]).unwrap();
    assert!(h
        .register_node_components("other", vec!["b".into()])
        .is_err());
    assert!(h.apply_links(vec!["n".into(), "other".into()]).is_err());
    assert_eq!(h.live_nodes(), ["n"]);
    let mut local = HealthWatch::default();
    local.apply_status("a", rows()).unwrap();
    assert!(local.observed().any_running("a"));
    for i in 0..600 {
        h.apply_links(vec![format!("n-{i}")]).unwrap();
    }
    assert!(h.events().len() <= 256);
    assert!(h.dropped_events() > 0);
}
fn manifest() -> parrot_app::AppManifest {
    use parrot_app::manifest::{AppManifest, ArtifactRef, ComponentSpec, EngineKind};
    AppManifest {
        name: "health".into(),
        version: "1.0.0".into(),
        components: vec![ComponentSpec {
            name: "a".into(),
            engine: EngineKind::Parrot,
            artifact: ArtifactRef::Props {
                factory: "test.echo".into(),
            },
            alt_artifact: None,
            instances: Default::default(),
            placement: Default::default(),
            upgrade: Default::default(),
            deps: vec![],
            config: None,
            hooks: Default::default(),
        }],
        wiring: vec![],
        mirrors: vec![],
        config_overlay: None,
    }
}
fn supervisor() -> AppSupervisor {
    AppSupervisor::new(
        manifest(),
        Arc::new(MemStateStore::new()),
        Arc::new(FakeClock::new(0)),
    )
    .unwrap()
}
fn assert_absence_unknown(observed: &ObservedState) {
    let report = supervisor().reconcile_once(observed);
    assert!(!report.converged);
    assert!(!report.actions.is_empty());
    assert!(
        report
            .actions
            .iter()
            .all(|action| matches!(action, ReconcileAction::Wait { .. })),
        "missing health knowledge must not authorize deployment: {:?}",
        report.actions
    );
}
#[test]
fn absence_unpolled_component_is_unknown_not_missing() {
    let h = watch();
    assert_absence_unknown(&h.observed());
}
#[test]
fn absence_invalidated_generation_cannot_authorize_redeployment() {
    let mut h = watch();
    accept(&mut h, "parrot-n1");
    h.invalidate_component("a");
    assert_absence_unknown(&h.observed());
}
#[test]
fn absence_lost_source_cannot_authorize_duplicate_on_another_live_node() {
    let mut h = watch();
    accept(&mut h, "parrot-n1");
    h.apply_links(vec!["parrot-n2".into()]).unwrap();
    assert_absence_unknown(&h.observed());
}
#[test]
fn absence_empty_reports_must_belong_to_the_selected_live_node() {
    let mut o = watch().observed();
    o.components.insert("a".into(), vec![]);
    for source in [
        None,
        Some("parrot-offline"),
        Some("ray-n1"),
        Some("parrot-n2"),
    ] {
        o.component_nodes.clear();
        if let Some(source) = source {
            o.component_nodes.insert("a".into(), source.into());
        }
        assert_absence_unknown(&o);
    }
    o.components.clear();
    o.component_nodes.insert("a".into(), "parrot-n1".into());
    assert_absence_unknown(&o);
}
#[test]
fn absence_confirmed_by_selected_node_allows_initial_deployment() {
    let mut h = watch();
    let ticket = h.begin_poll("parrot-n1", "a").unwrap();
    h.apply_poll(ticket, vec![]).unwrap();
    let report = supervisor().reconcile_once(&h.observed());
    assert_eq!(
        report.actions,
        vec![ReconcileAction::Deploy {
            comp: "a".into(),
            node: "parrot-n1".into()
        }]
    );
    assert!(!report.converged);
}
#[test]
fn supervisor_requires_live_eligible_source_even_with_healthy_paths() {
    let s = supervisor();
    let mut o = ObservedState {
        links_known: true,
        live_nodes: vec!["parrot-n1".into(), "ray-n1".into()],
        ..Default::default()
    };
    o.components.insert("a".into(), rows());
    for source in [None, Some("ray-n1"), Some("parrot-offline")] {
        o.component_nodes.clear();
        if let Some(source) = source {
            o.component_nodes.insert("a".into(), source.into());
        }
        let r = s.reconcile_once(&o);
        assert!(!r.converged);
        assert!(r
            .actions
            .iter()
            .all(|a| matches!(a, ReconcileAction::Wait { .. })));
    }
    o.component_nodes.insert("a".into(), "parrot-n1".into());
    assert!(s.reconcile_once(&o).converged);
}
#[test]
fn healthy_cohort_may_stay_on_an_eligible_nonpreferred_node() {
    let s = supervisor();
    let mut h = watch();
    h.register_node_components("parrot-n2", vec!["a".into()])
        .unwrap();
    accept(&mut h, "parrot-n2");
    assert!(s.reconcile_once(&h.observed()).converged);
}

struct Empty;
#[async_trait::async_trait]
impl parrot_remote::LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}
struct StatusExecutor {
    gate: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    reports: Vec<ComponentStateReport>,
}
#[async_trait::async_trait]
impl parrot_remote::admin_v2::ComponentExecutor for StatusExecutor {
    async fn deploy(
        &self,
        _: u64,
        _: &parrot_remote::admin_v2::ComponentDeploy,
    ) -> parrot_remote::admin_v2::AdminReplyV2 {
        panic!("unexpected deploy")
    }
    async fn drain(&self, _: u64, _: &str, _: u64) -> parrot_remote::admin_v2::AdminReplyV2 {
        panic!("unexpected drain")
    }
    async fn stop(&self, _: u64, _: &str) -> parrot_remote::admin_v2::AdminReplyV2 {
        panic!("unexpected stop")
    }
    async fn status(&self, req_id: u64, prefix: &str) -> parrot_remote::admin_v2::AdminReplyV2 {
        assert_eq!(prefix, "/user/a");
        if let Some(gate) = &self.gate {
            gate.0.notify_one();
            gate.1.notified().await;
        }
        parrot_remote::admin_v2::AdminReplyV2::Status {
            req_id,
            states: self.reports.clone(),
        }
    }
}
async fn remote_pair(
    authorized: bool,
    gate: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
) -> (
    Arc<parrot_remote::RemoteActorSystem>,
    Arc<parrot_remote::RemoteActorSystem>,
) {
    use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig};
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let config = |suffix| {
        let c = RemoteConfig::tcp(
            format!("parrot-health-{}-{id}-{suffix}", std::process::id()),
            Some("127.0.0.1:0".parse().unwrap()),
        );
        if authorized {
            c.with_insecure_development()
        } else {
            c
        }
    };
    let node = RemoteActorSystem::new(config("node"), Arc::new(Empty)).unwrap();
    node.set_component_executor(Some(Arc::new(StatusExecutor {
        gate,
        reports: rows(),
    })));
    node.start().await.unwrap();
    let client = RemoteActorSystem::new(config("client"), Arc::new(Empty)).unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::tcp(
            &node.config.node_id,
            node.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    (node, client)
}
#[tokio::test]
async fn real_tcp_status_carries_the_requested_node_into_convergence() {
    let (node, client) = remote_pair(true, None).await;
    let n = &node.config.node_id;
    let mut h = HealthWatch::new(vec![n.clone()]).unwrap();
    h.register_node_components(n, vec!["a".into()]).unwrap();
    let poll = h.begin_poll(n, "a").unwrap();
    let result = poll.fetch(&client).await.unwrap();
    h.apply_poll(poll, result).unwrap();
    assert_eq!(h.observed().component_nodes["a"], *n);
    assert!(supervisor().reconcile_once(&h.observed()).converged);
    client.shutdown().await.unwrap();
    node.shutdown().await.unwrap();
}
#[tokio::test]
async fn absence_real_tcp_empty_status_authorizes_only_its_target() {
    let (node, client) = remote_pair(true, None).await;
    node.set_component_executor(Some(Arc::new(StatusExecutor {
        gate: None,
        reports: vec![],
    })));
    let n = node.config.node_id.clone();
    let mut h = HealthWatch::new(vec![n.clone()]).unwrap();
    h.register_node_components(&n, vec!["a".into()]).unwrap();
    assert_absence_unknown(&h.observed());
    let poll = h.begin_poll(&n, "a").unwrap();
    let reports = poll.fetch(&client).await.unwrap();
    assert!(reports.is_empty());
    h.apply_poll(poll, reports).unwrap();
    assert_eq!(
        supervisor().reconcile_once(&h.observed()).actions,
        vec![ReconcileAction::Deploy {
            comp: "a".into(),
            node: n.clone()
        }]
    );
    h.invalidate_node(&n);
    assert_absence_unknown(&h.observed());
    client.shutdown().await.unwrap();
    node.shutdown().await.unwrap();
}
#[tokio::test]
async fn delayed_real_tcp_reply_is_rejected_after_link_invalidation() {
    use std::time::Duration;
    let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    let (node, client) = remote_pair(true, Some(gate.clone())).await;
    let mut h = HealthWatch::new(vec![node.config.node_id.clone()]).unwrap();
    h.register_node_components(&node.config.node_id, vec!["a".into()])
        .unwrap();
    let poll = h.begin_poll(&node.config.node_id, "a").unwrap();
    let client2 = client.clone();
    let p = poll.clone();
    let query = tokio::spawn(async move { p.fetch(&client2).await });
    tokio::time::timeout(Duration::from_secs(2), gate.0.notified())
        .await
        .unwrap();
    h.apply_links(vec![]).unwrap();
    gate.1.notify_one();
    let reply = tokio::time::timeout(Duration::from_secs(2), query)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(h.apply_poll(poll, reply).is_err());
    assert!(!h.observed().any_running("a"));
    client.shutdown().await.unwrap();
    node.shutdown().await.unwrap();
}
#[tokio::test]
async fn unauthorized_tcp_status_cannot_produce_health_receipt() {
    let (node, client) = remote_pair(false, None).await;
    let mut h = HealthWatch::new(vec![node.config.node_id.clone()]).unwrap();
    h.register_node_components(&node.config.node_id, vec!["a".into()])
        .unwrap();
    let poll = h.begin_poll(&node.config.node_id, "a").unwrap();
    assert!(poll.fetch(&client).await.is_err());
    assert_absence_unknown(&h.observed());
    assert!(!h.observed().any_running("a"));
    client.shutdown().await.unwrap();
    node.shutdown().await.unwrap();
}
#[test]
fn wrong_source_dependency_does_not_release_dependent_deployment() {
    let mut m = manifest();
    let mut b = m.components[0].clone();
    b.name = "b".into();
    b.deps = vec!["a".into()];
    m.components.push(b);
    let s = AppSupervisor::new(
        m,
        Arc::new(MemStateStore::new()),
        Arc::new(FakeClock::new(0)),
    )
    .unwrap();
    let mut o = ObservedState {
        links_known: true,
        live_nodes: vec!["parrot-n1".into(), "ray-n1".into()],
        ..Default::default()
    };
    o.components.insert("a".into(), rows());
    o.component_nodes.insert("a".into(), "ray-n1".into());
    o.components.insert("b".into(), vec![]);
    o.component_nodes.insert("b".into(), "parrot-n1".into());
    let r = s.reconcile_once(&o);
    assert!(r
        .actions
        .iter()
        .all(|a| matches!(a, ReconcileAction::Wait { .. })));
    o.component_nodes.insert("a".into(), "parrot-n1".into());
    assert!(s
        .reconcile_once(&o)
        .actions
        .iter()
        .any(|a| matches!(a,ReconcileAction::Deploy{comp,..} if comp=="b")));
}
#[test]
fn source_eligibility_checks_metadata_even_when_another_node_is_eligible() {
    use parrot_app::orchestrator::ObservedNode;
    let mut m = manifest();
    m.components[0].placement.role = Some("hub".into());
    let s = AppSupervisor::new(
        m,
        Arc::new(MemStateStore::new()),
        Arc::new(FakeClock::new(0)),
    )
    .unwrap();
    let mut h = watch();
    accept(&mut h, "parrot-n1");
    let mut o = h.observed();
    for (node, role) in [("parrot-n1", "worker"), ("parrot-n2", "hub")] {
        o.node_metadata.insert(
            node.into(),
            ObservedNode {
                engine: parrot_app::EngineKind::Parrot,
                role: role.into(),
                realm: "default".into(),
                labels: vec![],
            },
        );
    }
    assert!(!s.reconcile_once(&o).converged);
    o.component_nodes.insert("a".into(), "parrot-n2".into());
    assert!(s.reconcile_once(&o).converged);
}
#[test]
fn oversized_history_event_is_dropped_without_losing_current_links() {
    let mut h = HealthWatch::new(vec![]).unwrap();
    let node = "x".repeat(70 * 1024);
    h.apply_links(vec![node.clone()]).unwrap();
    assert_eq!(h.live_nodes(), [node]);
    assert!(h.events().is_empty());
    assert_eq!(h.dropped_events(), 1);
}
#[test]
fn global_row_budget_rejects_another_component_without_changing_first() {
    let limits = parrot_app::ApplicationLimits {
        max_instances: 1,
        ..Default::default()
    };
    let mut h = HealthWatch::with_limits(vec![], limits).unwrap();
    h.apply_status("a", rows()).unwrap();
    let mut b = rows();
    b[0].path = "/user/b".into();
    assert!(h.apply_status("b", b).is_err());
    assert!(h.observed().any_running("a"));
    assert_eq!(h.observed().components.len(), 1);
}
#[test]
fn same_node_component_redeployment_requires_a_new_poll() {
    let mut h = watch();
    accept(&mut h, "parrot-n1");
    let old = h.begin_poll("parrot-n1", "a").unwrap();
    h.invalidate_component("a");
    assert!(!h.observed().any_running("a"));
    assert!(h.apply_poll(old, rows()).is_err());
    accept(&mut h, "parrot-n1");
    assert!(h.observed().any_running("a"));
}
