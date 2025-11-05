use super::*;
use std::future::Future;
use std::pin::Pin;
use tokio::time::{Instant, MissedTickBehavior};

/// The in-process POC intentionally uses bounded full-state gossip. Dead records
/// remain as tombstones and gossip targets, allowing a healed peer to refute even
/// when both sides considered each other dead. This is not a production WAN SLO.
#[derive(Debug, Clone)]
pub struct SwimConfig {
    pub probe_interval: Duration,
    pub probe_timeout: Duration,
    pub indirect_probes: usize,
    pub suspect_timeout: Duration,
    pub gossip_interval: Duration,
    pub gossip_fanout: usize,
}
impl Default for SwimConfig {
    fn default() -> Self {
        Self {
            probe_interval: Duration::from_millis(500),
            probe_timeout: Duration::from_millis(500),
            indirect_probes: 3,
            suspect_timeout: Duration::from_secs(3),
            gossip_interval: Duration::from_millis(200),
            gossip_fanout: 3,
        }
    }
}
impl SwimConfig {
    pub(super) fn validate(&self) -> Result<(), ClusterError> {
        if [
            self.probe_interval,
            self.probe_timeout,
            self.suspect_timeout,
            self.gossip_interval,
        ]
        .iter()
        .any(|d| d.is_zero() || Instant::now().checked_add(*d).is_none())
            || self.indirect_probes > MAX_INDIRECT
            || self.gossip_fanout == 0
            || self.gossip_fanout > MAX_MEMBERS
        {
            return Err(ClusterError::InvalidConfig);
        }
        Ok(())
    }
}

/// These registries belong to this observer's membership view. Suspect retains
/// routes; Dead withdraws them. Higher-incarnation recovery permits explicit
/// service/route re-registration, never resurrection of stale announcements.
#[derive(Clone, Default)]
pub struct ClusterBindings {
    pub receptionist: Option<Arc<Receptionist>>,
    pub router: Option<Arc<crate::PrefixRouter>>,
}
impl ClusterBindings {
    fn apply(&self, members: &[Member]) -> Result<(), ClusterError> {
        if let Some(router) = &self.router {
            router.apply_membership(members);
        }
        if let Some(receptionist) = &self.receptionist {
            receptionist.apply_membership(members)?;
        }
        Ok(())
    }
}

type Probe = Pin<Box<dyn Future<Output = (Member, Result<ProbeOutcome, ClusterError>)> + Send>>;
async fn finish(probe: &mut Option<Probe>) -> (Member, Result<ProbeOutcome, ClusterError>) {
    match probe {
        Some(p) => p.await,
        None => std::future::pending().await,
    }
}

// A sorted cursor guarantees every known member a turn, including membership
// changes. Selecting only the first N peers would starve later IDs forever.
fn rotate(members: &[Member], cursor: &mut String, count: usize) -> Vec<Member> {
    if members.is_empty() || count == 0 {
        return vec![];
    }
    let start = members
        .iter()
        .position(|m| m.node_id > *cursor)
        .unwrap_or(0);
    let selected: Vec<_> = members
        .iter()
        .cycle()
        .skip(start)
        .take(count.min(members.len()))
        .cloned()
        .collect();
    *cursor = selected.last().unwrap().node_id.clone();
    selected
}

pub(super) async fn run(
    node: Arc<SwimNode>,
    state: Arc<ProbeState>,
    config: SwimConfig,
    bindings: ClusterBindings,
) -> Result<(), ClusterError> {
    let now = Instant::now();
    let mut probe_ticks =
        tokio::time::interval_at(now + config.probe_interval, config.probe_interval);
    let mut gossip_ticks =
        tokio::time::interval_at(now + config.gossip_interval, config.gossip_interval);
    probe_ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    gossip_ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut probe = None;
    let mut probe_cursor = String::new();
    let mut helper_cursor = String::new();
    let mut gossip_cursor = String::new();
    let mut suspicions = BTreeMap::<String, (u64, Instant)>::new();
    loop {
        let members = node.members().await;
        bindings.apply(&members)?;
        suspicions.retain(|id, (inc, _)| {
            members.iter().any(|m| {
                &m.node_id == id && m.incarnation == *inc && m.status == MemberStatus::Suspect
            })
        });
        for m in &members {
            if m.node_id != node.node_id && m.status == MemberStatus::Suspect {
                // Duplicate gossip/failing probes never extend the deadline.
                suspicions
                    .entry(m.node_id.clone())
                    .or_insert((m.incarnation, Instant::now() + config.suspect_timeout));
            }
        }
        let expiry = suspicions.values().map(|(_, deadline)| *deadline).min();
        tokio::select! {
            _ = node.changed.notified() => {},
            _ = tokio::time::sleep_until(expiry.unwrap_or_else(Instant::now)), if expiry.is_some() => {
                for (id, (inc, deadline)) in &suspicions {
                    if *deadline <= Instant::now() { node.mark_dead(id, *inc).await; }
                }
            },
            (observed, outcome) = finish(&mut probe), if probe.is_some() => {
                probe = None;
                match outcome {
                    Ok(result) if !result.reachable => { node.mark_suspect(&observed.node_id, observed.incarnation).await; },
                    Ok(_) => {}, // An ACK cannot override an accusation at the same incarnation.
                    Err(ClusterError::ProbeCapacity) => {}, // Local overload is not evidence of peer failure.
                    Err(error) => return Err(error),
                }
            },
            _ = probe_ticks.tick() => {
                if probe.is_none() {
                    let live: Vec<_> = members.iter().filter(|m| m.node_id != node.node_id && m.status != MemberStatus::Dead).cloned().collect();
                    if let Some(target) = rotate(&live, &mut probe_cursor, 1).pop() {
                        let helpers: Vec<_> = live.into_iter().filter(|m| m.node_id != target.node_id && m.status == MemberStatus::Alive).collect();
                        let via: Vec<_> = rotate(&helpers, &mut helper_cursor, config.indirect_probes).into_iter().map(|m| m.node_id).collect();
                        let n = node.clone(); let s = state.clone(); let timeout = config.probe_timeout;
                        probe = Some(Box::pin(async move {
                            let outcome = n.probe_on(s, &target.node_id, &via, timeout).await;
                            (target, outcome)
                        }));
                    }
                }
            },
            _ = gossip_ticks.tick() => {
                let peers: Vec<_> = members.iter().filter(|m| m.node_id != node.node_id).cloned().collect();
                for peer in rotate(&peers, &mut gossip_cursor, config.gossip_fanout) {
                    state.sender.send(&peer.node_id, SwimMsg::Membership(members.clone()));
                }
            },
        }
    }
}
