//! Node-scoped, generation-fenced component observations.
//! Issue a poll ticket before I/O, then apply that ticket's reply. A node departure,
//! reassignment, explicit reconnect or newer poll invalidates older replies.
use super::supervisor::ObservedState;
use crate::ApplicationLimits;
use parrot_remote::{admin_v2::ComponentStateReport, RemoteActorSystem};
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkDiff {
    pub joined: Vec<String>,
    pub left: Vec<String>,
}
pub fn diff_links(prev: &[String], curr: &[String]) -> LinkDiff {
    let p: std::collections::BTreeSet<_> = prev.iter().cloned().collect();
    let c: std::collections::BTreeSet<_> = curr.iter().cloned().collect();
    LinkDiff {
        joined: c.difference(&p).cloned().collect(),
        left: p.difference(&c).cloned().collect(),
    }
}

/// Opaque ticket bound to one watch, assignment and latest poll. Clones cannot
/// apply twice. Caller-supplied observations must come from an authenticated host.
#[derive(Debug, Clone)]
pub struct StatusPoll {
    watch: Weak<()>,
    node: String,
    component: String,
    sequence: u64,
}
impl StatusPoll {
    pub fn node(&self) -> &str {
        &self.node
    }
    pub fn component(&self) -> &str {
        &self.component
    }
    /// Fetch from the node bound to this ticket. Keep the ticket across the await
    /// and feed the result into apply_poll; never infer the node from a path.
    pub async fn fetch(
        &self,
        remote: &Arc<RemoteActorSystem>,
    ) -> Result<Vec<ComponentStateReport>, String> {
        remote
            .component_status(&self.node, &format!("/user/{}", self.component))
            .await
            .map_err(|e| e.to_string())
    }
}

#[derive(Debug)]
pub struct HealthWatch {
    limits: ApplicationLimits,
    identity: Arc<()>,
    issued: u64,
    live_nodes: Vec<String>,
    links_known: bool,
    states: BTreeMap<String, Vec<ComponentStateReport>>,
    assignments: BTreeMap<String, String>,
    sources: BTreeMap<String, String>,
    pending: BTreeMap<String, u64>,
    events: Vec<LinkDiff>,
    event_bytes: usize,
    dropped_events: u64,
}
impl Default for HealthWatch {
    fn default() -> Self {
        Self {
            limits: Default::default(),
            identity: Arc::new(()),
            issued: 0,
            live_nodes: vec![],
            links_known: false,
            states: BTreeMap::new(),
            assignments: BTreeMap::new(),
            sources: BTreeMap::new(),
            pending: BTreeMap::new(),
            events: vec![],
            event_bytes: 0,
            dropped_events: 0,
        }
    }
}
impl HealthWatch {
    pub fn new(initial_nodes: Vec<String>) -> Result<Self, String> {
        Self::with_limits(initial_nodes, ApplicationLimits::default())
    }
    pub fn with_limits(mut nodes: Vec<String>, limits: ApplicationLimits) -> Result<Self, String> {
        let mut value = Self {
            limits,
            ..Self::default()
        };
        value.check_nodes(&nodes)?;
        nodes.sort();
        nodes.dedup();
        value.live_nodes = nodes;
        value.links_known = true;
        Ok(value)
    }
    fn check_nodes(&self, nodes: &[String]) -> Result<(), String> {
        crate::limits::check(
            "health nodes",
            nodes.len() as u64,
            self.limits.max_candidates,
        )
        .map_err(|e| e.to_string())?;
        if nodes
            .iter()
            .any(|n| n.is_empty() || n.chars().any(char::is_control))
        {
            return Err("invalid health node name".into());
        }
        let mut bytes = 0;
        for node in nodes {
            crate::limits::add(
                "health node bytes",
                &mut bytes,
                (node.len() + std::mem::size_of::<String>()) as u64,
                self.limits.max_candidate_bytes,
            )
            .map_err(|e| e.to_string())?;
        }
        self.limits
            .check_metadata(&nodes)
            .map_err(|e| e.to_string())
    }
    fn invalidate(&mut self, component: &str) {
        self.pending.remove(component);
        self.states.remove(component);
        self.sources.remove(component);
    }
    /// Fence observations when the host replaces a component generation on the
    /// same node. Assignment equality alone cannot identify a redeployment.
    pub fn invalidate_component(&mut self, component: &str) {
        self.invalidate(component);
    }
    /// Same-name connection replacement may be invisible in set snapshots. The
    /// host must call this when its authenticated connection generation changes.
    pub fn invalidate_node(&mut self, node: &str) {
        let names: Vec<_> = self
            .assignments
            .iter()
            .filter(|(_, n)| n.as_str() == node)
            .map(|(c, _)| c.clone())
            .collect();
        for name in names {
            self.invalidate(&name);
        }
    }
    pub fn apply_links(&mut self, mut curr: Vec<String>) -> Result<LinkDiff, String> {
        self.check_nodes(&curr)?;
        curr.sort();
        curr.dedup();
        let diff = diff_links(&self.live_nodes, &curr);
        for gone in &diff.left {
            self.invalidate_node(gone);
        }
        self.live_nodes = curr;
        self.links_known = true;
        if !diff.joined.is_empty() || !diff.left.is_empty() {
            let bytes = event_size(&diff);
            if bytes > 64 * 1024 {
                self.dropped_events = self.dropped_events.saturating_add(1);
            } else {
                while self.events.len() >= 256 || self.event_bytes + bytes > 64 * 1024 {
                    self.event_bytes -= event_size(&self.events.remove(0));
                    self.dropped_events = self.dropped_events.saturating_add(1);
                }
                self.event_bytes += bytes;
                self.events.push(diff.clone());
            }
        }
        Ok(diff)
    }
    /// Atomically replace this node's component assignments. Moving a component
    /// to another node invalidates the previous receipt and outstanding polls.
    pub fn register_node_components(
        &mut self,
        node: &str,
        comps: Vec<String>,
    ) -> Result<(), String> {
        if node.is_empty() || node.chars().any(char::is_control) {
            return Err("invalid health node name".into());
        }
        crate::limits::check(
            "health node bytes",
            (node.len() + std::mem::size_of::<String>()) as u64,
            self.limits.max_candidate_bytes,
        )
        .map_err(|e| e.to_string())?;
        self.limits
            .check_metadata(&node)
            .map_err(|e| e.to_string())?;
        crate::limits::check(
            "health assignments",
            comps.len() as u64,
            self.limits.max_components,
        )
        .map_err(|e| e.to_string())?;
        if comps
            .iter()
            .any(|c| !crate::manifest::valid_path_segment(c))
        {
            return Err("invalid health component name".into());
        }
        self.limits
            .check_metadata(&comps)
            .map_err(|e| e.to_string())?;
        let wanted: std::collections::BTreeSet<_> = comps.iter().cloned().collect();
        if wanted.len() != comps.len() {
            return Err("duplicate health component assignment".into());
        }
        let mut next = self.assignments.clone();
        next.retain(|c, n| n != node || wanted.contains(c));
        for c in comps {
            next.insert(c, node.to_string());
        }
        crate::limits::check(
            "health assignments",
            next.len() as u64,
            self.limits.max_components,
        )
        .map_err(|e| e.to_string())?;
        self.limits
            .check_metadata(&next)
            .map_err(|e| e.to_string())?;
        let changed: Vec<_> = self
            .assignments
            .keys()
            .chain(next.keys())
            .filter(|c| self.assignments.get(*c) != next.get(*c))
            .cloned()
            .collect();
        for name in changed {
            self.invalidate(&name);
        }
        self.assignments = next;
        Ok(())
    }
    pub fn begin_poll(&mut self, node: &str, component: &str) -> Result<StatusPoll, String> {
        if self.assignments.get(component).map(String::as_str) != Some(node)
            || !self.live_nodes.iter().any(|n| n == node)
        {
            return Err("component is not assigned to this live node".into());
        }
        let sequence = self
            .issued
            .checked_add(1)
            .ok_or("health poll sequence exhausted")?;
        self.issued = sequence;
        self.pending.insert(component.into(), sequence);
        Ok(StatusPoll {
            watch: Arc::downgrade(&self.identity),
            node: node.into(),
            component: component.into(),
            sequence,
        })
    }
    pub fn apply_poll(
        &mut self,
        poll: StatusPoll,
        reports: Vec<ComponentStateReport>,
    ) -> Result<(), String> {
        if !Weak::ptr_eq(&poll.watch, &Arc::downgrade(&self.identity))
            || self.pending.get(&poll.component) != Some(&poll.sequence)
            || self.assignments.get(&poll.component) != Some(&poll.node)
            || !self.live_nodes.contains(&poll.node)
        {
            return Err("stale or foreign health poll".into());
        }
        self.check_reports(&poll.component, &reports)?;
        self.states.insert(poll.component.clone(), reports);
        self.sources.insert(poll.component.clone(), poll.node);
        self.pending.remove(&poll.component);
        Ok(())
    }
    /// Trusted local injection only. Network assignments require an issued poll
    /// ticket so a delayed caller cannot accidentally resurrect a previous node.
    pub fn apply_status(
        &mut self,
        comp: &str,
        reports: Vec<ComponentStateReport>,
    ) -> Result<(), String> {
        if self.assignments.contains_key(comp) {
            return Err("network component requires a StatusPoll".into());
        }
        self.check_reports(comp, &reports)?;
        self.states.insert(comp.into(), reports);
        self.sources.remove(comp);
        Ok(())
    }
    fn check_reports(&self, comp: &str, reports: &[ComponentStateReport]) -> Result<(), String> {
        if !crate::manifest::valid_path_segment(comp) {
            return Err("invalid health component name".into());
        }
        let mut paths = std::collections::BTreeSet::new();
        crate::limits::check(
            "health instances",
            reports.len() as u64,
            self.limits.max_instances,
        )
        .map_err(|e| e.to_string())?;
        self.limits
            .check_metadata(&(comp, reports))
            .map_err(|e| e.to_string())?;
        if reports
            .iter()
            .any(|r| !super::supervisor::owned_root(comp, &r.path) || !paths.insert(&r.path))
        {
            return Err("foreign or duplicate health instance path".into());
        }
        let retained = self.states.iter().filter(|(c, _)| c.as_str() != comp);
        let count = retained.clone().count() + 1;
        crate::limits::check(
            "health components",
            count as u64,
            self.limits.max_components,
        )
        .map_err(|e| e.to_string())?;
        let rows = retained
            .clone()
            .try_fold(reports.len() as u64, |n, (_, rs)| {
                n.checked_add(rs.len() as u64)
            })
            .ok_or("health instance count overflow")?;
        crate::limits::check("health instances", rows, self.limits.max_instances)
            .map_err(|e| e.to_string())?;
        // Borrow existing rows; this bounded vector holds references, not copies.
        let views: Vec<_> = retained
            .map(|(c, r)| (c.as_str(), r.as_slice()))
            .chain(std::iter::once((comp, reports)))
            .collect();
        self.limits
            .check_metadata(&views)
            .map_err(|e| e.to_string())
    }
    pub fn live_nodes(&self) -> &[String] {
        &self.live_nodes
    }
    pub fn events(&self) -> &[LinkDiff] {
        &self.events
    }
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events
    }
    pub fn observed(&self) -> ObservedState {
        ObservedState {
            components: self.states.clone(),
            component_nodes: self.sources.clone(),
            live_nodes: self.live_nodes.clone(),
            links_known: self.links_known,
            node_metadata: Default::default(),
        }
    }
}
fn event_size(event: &LinkDiff) -> usize {
    std::mem::size_of::<LinkDiff>()
        + event
            .joined
            .iter()
            .chain(&event.left)
            .map(|n| n.len() + std::mem::size_of::<String>())
            .sum::<usize>()
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
        let mut h = HealthWatch::new(vec!["n1".into()]).unwrap();
        let d = h.apply_links(vec!["n1".into(), "n2".into()]).unwrap();
        assert_eq!(d.joined, vec!["n2".to_string()]);
        assert_eq!(h.live_nodes(), &["n1".to_string(), "n2".to_string()]);
        // 无变化帧不产生事件
        h.apply_links(vec!["n1".into(), "n2".into()]).unwrap();
        assert_eq!(h.events().len(), 1);
    }

    #[test]
    fn gateway_down_degrades_its_components() {
        // MG9-10 语义面：网关 kill → 该节点组件 observed 降级 →
        // supervisor 下轮 reconcile 触发重建
        let mut h = HealthWatch::new(vec!["erl-gw-1".into()]).unwrap();
        h.register_node_components("erl-gw-1", vec!["frontier".into()])
            .unwrap();
        let poll = h.begin_poll("erl-gw-1", "frontier").unwrap();
        h.apply_poll(poll, vec![rep("/user/frontier", "running")])
            .unwrap();
        let o = h.observed();
        assert!(o.any_running("frontier"));
        // kill：links 帧失去 erl-gw-1
        h.apply_links(vec![]).unwrap();
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
        let mut h = HealthWatch::new(vec!["erl-gw-1".into()]).unwrap();
        h.register_node_components("erl-gw-1", vec!["frontier".into()])
            .unwrap();
        let poll = h.begin_poll("erl-gw-1", "frontier").unwrap();
        h.apply_poll(poll, vec![rep("/user/frontier", "running")])
            .unwrap();
        h.apply_links(vec![]).unwrap(); // kill
        assert!(!h.observed().any_running("frontier"));
        // 重启：links 恢复 + 轮询回执恢复
        h.apply_links(vec!["erl-gw-1".into()]).unwrap();
        let poll = h.begin_poll("erl-gw-1", "frontier").unwrap();
        h.apply_poll(poll, vec![rep("/user/frontier", "running")])
            .unwrap();
        assert!(h.observed().any_running("frontier"));
    }

    #[test]
    fn observed_snapshot_independent() {
        // 导出快照后内部再变不影响旧快照（值语义）
        let mut h = HealthWatch::new(vec![]).unwrap();
        h.apply_status("a", vec![rep("/user/a", "running")])
            .unwrap();
        let o1 = h.observed();
        h.apply_status("a", vec![rep("/user/a", "stopped")])
            .unwrap();
        assert!(o1.any_running("a"));
        assert!(!h.observed().any_running("a"));
    }
}
