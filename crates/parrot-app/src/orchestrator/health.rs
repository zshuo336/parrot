//! E3（DEV_09 §3.5）：HealthWatch——observed 更新器。
//!
//! 数据源两路（09 §3.5）：
//! 1. 链接状态差分（links_snapshot 前后对比 → 节点增删事件）
//! 2. admin ComponentStatus 轮询回执（poller 喂入——协议侧由宿主驱动）
//!
//! 产出：[`ObservedState`] 增量更新（AppSupervisor.reconcile_once 输入）。

use super::supervisor::ObservedState;
use parrot_remote::admin_v2::ComponentStateReport;

/// 链接差分事件（links_snapshot 两帧对比）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkDiff {
    pub joined: Vec<String>,
    pub left: Vec<String>,
}

/// 链接差分计算（纯函数——帧间节点集对比）。
pub fn diff_links(prev: &[String], curr: &[String]) -> LinkDiff {
    let p: std::collections::BTreeSet<_> = prev.iter().cloned().collect();
    let c: std::collections::BTreeSet<_> = curr.iter().cloned().collect();
    LinkDiff {
        joined: c.difference(&p).cloned().collect(),
        left: p.difference(&c).cloned().collect(),
    }
}

/// 健康观察器（差分累积 → ObservedState 维护）。
#[derive(Debug, Default)]
pub struct HealthWatch {
    /// 当前存活节点集（links 差分维护）。
    live_nodes: Vec<String>,
    /// 组件状态（poller 喂入——全量覆盖单组件键）。
    states: std::collections::BTreeMap<String, Vec<ComponentStateReport>>,
    /// 网关断连时待降级组件（node → 该节点承载的组件前缀登记）。
    node_components: std::collections::BTreeMap<String, Vec<String>>,
    /// 差分事件历史（测试断言/审计）。
    pub events: Vec<LinkDiff>,
}

impl HealthWatch {
    pub fn new(initial_nodes: Vec<String>) -> Self {
        Self {
            live_nodes: initial_nodes,
            ..Default::default()
        }
    }

    /// 应用一帧 links 快照（差分 → 事件 + live_nodes 更新）。
    pub fn apply_links(&mut self, curr: Vec<String>) -> LinkDiff {
        let d = diff_links(&self.live_nodes, &curr);
        if !d.joined.is_empty() || !d.left.is_empty() {
            self.events.push(d.clone());
        }
        self.live_nodes = curr;
        // 离线节点的组件 → 状态降级（observed 侧标记 stopped——
        // supervisor diff 形态 5 的 Wait/重建输入）
        for gone in &d.left {
            if let Some(comps) = self.node_components.get(gone) {
                for c in comps {
                    if let Some(rs) = self.states.get_mut(c) {
                        for r in rs.iter_mut() {
                            r.state = "stopped".into();
                        }
                    }
                }
            }
        }
        d
    }

    /// 状态轮询回执喂入（单组件全量实例报告）。
    pub fn apply_status(&mut self, comp: &str, reports: Vec<ComponentStateReport>) {
        self.states.insert(comp.to_string(), reports);
    }

    /// 节点承载组件登记（断连降级映射）。
    pub fn register_node_components(&mut self, node: &str, comps: Vec<String>) {
        self.node_components.insert(node.to_string(), comps);
    }

    /// 当前存活节点快照。
    pub fn live_nodes(&self) -> &[String] {
        &self.live_nodes
    }

    /// 导出 observed（AppSupervisor 输入——快照语义，零内部状态转移）。
    pub fn observed(&self) -> ObservedState {
        ObservedState {
            components: self.states.clone(),
            live_nodes: self.live_nodes.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(path: &str, state: &str) -> ComponentStateReport {
        ComponentStateReport {
            path: path.into(),
            state: state.into(),
            version: "1".into(),
        }
    }

    #[test]
    fn diff_links_join_and_leave() {
        let d = diff_links(&["a".into()], &["a".into(), "b".into()]);
        assert_eq!(d.joined, vec!["b".to_string()]);
        assert!(d.left.is_empty());
        let d2 = diff_links(&["a".into(), "b".into()], &["a".into()]);
        assert!(d2.joined.is_empty());
        assert_eq!(d2.left, vec!["b".to_string()]);
    }

    #[test]
    fn diff_links_no_change_empty() {
        let d = diff_links(&["a".into(), "b".into()], &["b".into(), "a".into()]);
        assert!(d.joined.is_empty() && d.left.is_empty());
    }

    #[test]
    fn apply_links_updates_live_nodes() {
        let mut h = HealthWatch::new(vec!["n1".into()]);
        let d = h.apply_links(vec!["n1".into(), "n2".into()]);
        assert_eq!(d.joined, vec!["n2".to_string()]);
        assert_eq!(h.live_nodes(), &["n1".to_string(), "n2".to_string()]);
        // 无变化帧不产生事件
        h.apply_links(vec!["n1".into(), "n2".into()]);
        assert_eq!(h.events.len(), 1);
    }

    #[test]
    fn gateway_down_degrades_its_components() {
        // MG9-10 语义面：网关 kill → 该节点组件 observed 降级 →
        // supervisor 下轮 reconcile 触发重建
        let mut h = HealthWatch::new(vec!["erl-gw-1".into()]);
        h.register_node_components("erl-gw-1", vec!["frontier".into()]);
        h.apply_status("frontier", vec![rep("/user/frontier", "running")]);
        let o = h.observed();
        assert!(o.any_running("frontier"));
        // kill：links 帧失去 erl-gw-1
        h.apply_links(vec![]);
        let o2 = h.observed();
        assert!(
            !o2.any_running("frontier"),
            "gateway down must degrade observed state"
        );
        assert!(o2.live_nodes.is_empty());
    }

    #[test]
    fn gateway_restart_recovers_via_status_poll() {
        // MG9-10 恢复弧：网关重启 → links 回来 → 轮询恢复 running
        let mut h = HealthWatch::new(vec!["erl-gw-1".into()]);
        h.register_node_components("erl-gw-1", vec!["frontier".into()]);
        h.apply_status("frontier", vec![rep("/user/frontier", "running")]);
        h.apply_links(vec![]); // kill
        assert!(!h.observed().any_running("frontier"));
        // 重启：links 恢复 + 轮询回执恢复
        h.apply_links(vec!["erl-gw-1".into()]);
        h.apply_status("frontier", vec![rep("/user/frontier", "running")]);
        assert!(h.observed().any_running("frontier"));
    }

    #[test]
    fn observed_snapshot_independent() {
        // 导出快照后内部再变不影响旧快照（值语义）
        let mut h = HealthWatch::new(vec![]);
        h.apply_status("a", vec![rep("/user/a", "running")]);
        let o1 = h.observed();
        h.apply_status("a", vec![rep("/user/a", "stopped")]);
        assert!(o1.any_running("a"));
        assert!(!h.observed().any_running("a"));
    }
}
