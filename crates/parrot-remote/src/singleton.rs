//! Local singleton lease bookkeeping for a host-provided authority protocol.
//!
//! This module performs no network voting, persistence or resource fencing.
//! SWIM reachability and a caller-supplied boolean alone cannot prove exclusive
//! ownership: two independent candidates can both receive `true`. Production
//! hosts need an authenticated authority protocol, durable epochs and fencing
//! at the resource before using this state to control singleton work.
//!
//! Hosts drive `tick` with fresh confirmations and inspect state after every
//! observation. A lost local lease emits one `SteppedDown` on the next tick,
//! including losses discovered by `observe`. Retirement prevents this instance
//! from reacquiring the same epoch. Reconstructing this object resets its floor;
//! it is not a substitute for persistent fencing across processes or restarts.

use std::time::{Duration, Instant};

/// Host lease duration and suggested tick cadence. These are not a measured
/// cluster takeover bound. Zero or unrepresentable leases do not grant authority.
#[derive(Debug, Clone, Copy)]
pub struct LeaseParams {
    pub lease: Duration,
    pub renew_interval: Duration,
}

impl Default for LeaseParams {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(10),
            renew_interval: Duration::from_secs(3),
        }
    }
}

/// Host-provided identity and epoch priority. Ordering compares sequence first,
/// then node name. Node-local restart counters do not establish a global epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub node: String,
    pub seq: u64,
}

impl Candidate {
    fn valid(&self) -> bool {
        self.seq != 0 && !self.node.is_empty() && !self.node.chars().any(char::is_control)
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.seq, &self.node).cmp(&(other.seq, &other.node))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// 租约状态机（本节点视角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SingletonState {
    /// 无主（初始/租约失效后）。
    Vacant,
    /// Last processed local ownership; also check the deadline at current time.
    Leader,
    /// 他节点持有（已知 Leader）。
    Follower,
}

/// Deterministic local state. `state()` is the last processed state; callers
/// must drive ticks and check `current_holder(now)` before relying on a lease.
pub struct SingletonLease {
    params: LeaseParams,
    me: Candidate,
    /// 当前已知持有者 + 租约到期时刻。
    holder: Option<(Candidate, Instant)>,
    /// Highest admitted epoch, including local acquisition. Retirement does
    /// not make a delayed observation or local confirmation a new epoch.
    floor_seq: u64,
    state: SingletonState,
    pending_loss: bool,
    last_now: Option<Instant>,
}

impl SingletonLease {
    pub fn new(me: Candidate, params: LeaseParams) -> Self {
        Self {
            params,
            me,
            holder: None,
            floor_seq: 0,
            state: SingletonState::Vacant,
            pending_loss: false,
            last_now: None,
        }
    }

    /// 当前持有者（租约未过期时）。
    pub fn current_holder(&self, now: Instant) -> Option<&Candidate> {
        match &self.holder {
            Some((c, expiry)) if *expiry > now => Some(c),
            Some((c, _)) => {
                // 过期——Vacant 视角（不改内部，tick 驱动转移）
                let _ = c;
                None
            }
            None => None,
        }
    }

    pub fn state(&self) -> SingletonState {
        self.state
    }

    fn deadline(&self, now: Instant) -> Option<Instant> {
        now.checked_add(self.params.lease).filter(|end| *end > now)
    }

    fn expire(&mut self, now: Instant) {
        if self.holder.as_ref().is_some_and(|(_, end)| now >= *end) {
            let (holder, _) = self.holder.take().expect("expired holder");
            self.floor_seq = self.floor_seq.max(holder.seq);
            self.pending_loss |= self.state == SingletonState::Leader;
            self.state = SingletonState::Vacant;
        }
    }

    fn advance(&mut self, now: Instant) -> bool {
        if self.last_now.is_some_and(|last| now < last) {
            return false;
        }
        self.last_now = Some(now);
        self.expire(now);
        true
    }

    /// Advance with a fresh host authority confirmation. Reachability alone
    /// is insufficient. Expiry is reported before any further acquisition;
    /// losing authority permanently retires this instance's local epoch.
    pub fn tick(&mut self, now: Instant, quorum_ok: bool) -> Option<SingletonTransition> {
        if !self.advance(now) {
            return None;
        }
        if std::mem::take(&mut self.pending_loss) {
            return Some(SingletonTransition::SteppedDown(self.me.clone()));
        }

        match self.state {
            SingletonState::Leader => {
                if quorum_ok {
                    if let Some(deadline) = self.deadline(now) {
                        if let Some((_, expiry)) = self.holder.as_mut() {
                            *expiry = (*expiry).max(deadline);
                            return None;
                        }
                    }
                }
                self.holder = None;
                self.state = SingletonState::Vacant;
                Some(SingletonTransition::SteppedDown(self.me.clone()))
            }
            SingletonState::Vacant | SingletonState::Follower => {
                let can_take =
                    self.holder.is_none() && self.me.valid() && self.me.seq > self.floor_seq;
                if quorum_ok && can_take {
                    if let Some(deadline) = self.deadline(now) {
                        self.floor_seq = self.me.seq;
                        self.holder = Some((self.me.clone(), deadline));
                        self.state = SingletonState::Leader;
                        return Some(SingletonTransition::Acquired(self.me.clone()));
                    }
                }
                None
            }
        }
    }

    /// Apply a fresh authenticated lease confirmation supplied by the host.
    /// Raw gossip/replayed identities are not confirmations. A matching live
    /// remote holder may renew, but an expired epoch cannot be resurrected.
    /// Self-observation never grants authority. Inspect state after this call;
    /// a local loss is also retained for the next tick's notification.
    pub fn observe(&mut self, now: Instant, holder: &Candidate) {
        if !self.advance(now) {
            return;
        }
        if !holder.valid() || *holder == self.me {
            return;
        }
        let Some(deadline) = self.deadline(now) else {
            return;
        };
        if let Some((current, expiry)) = self.holder.as_mut() {
            if current == holder {
                *expiry = (*expiry).max(deadline);
                return;
            }
        }
        if holder.seq <= self.floor_seq {
            return;
        }
        self.pending_loss |= self.state == SingletonState::Leader;
        self.floor_seq = holder.seq;
        self.holder = Some((holder.clone(), deadline));
        self.state = SingletonState::Follower;
    }

    /// Configured observation lifetime. Renewals, host scheduling and authority
    /// protocol availability can delay takeover beyond this duration.
    pub fn takeover_wait(&self) -> Duration {
        self.params.lease
    }
}

/// 状态迁移事件。
#[derive(Debug, Clone, PartialEq)]
pub enum SingletonTransition {
    /// Local acquisition after the supplied host confirmation.
    Acquired(Candidate),
    /// Local authority lost through expiry, preemption or failed confirmation.
    SteppedDown(Candidate),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(n: &str, seq: u64) -> Candidate {
        Candidate {
            node: n.into(),
            seq,
        }
    }

    // Inject host confirmations; this does not obtain real network votes.
    #[test]
    fn acquire_and_renew() {
        let mut l = SingletonLease::new(cand("a", 1), LeaseParams::default());
        let t0 = Instant::now();
        assert_eq!(
            l.tick(t0, true),
            Some(SingletonTransition::Acquired(cand("a", 1)))
        );
        assert_eq!(l.state(), SingletonState::Leader);
        // renew 3s 后仍 Leader（租约延至 t+13s）
        l.tick(t0 + Duration::from_secs(3), true);
        assert_eq!(l.state(), SingletonState::Leader);
        assert!(l.current_holder(t0 + Duration::from_secs(12)).is_some());
    }

    // A denied host confirmation withdraws local ownership.
    #[test]
    fn step_down_on_quorum_loss() {
        let mut l = SingletonLease::new(cand("a", 1), LeaseParams::default());
        let t0 = Instant::now();
        l.tick(t0, true);
        assert_eq!(
            l.tick(t0 + Duration::from_secs(3), false),
            Some(SingletonTransition::SteppedDown(cand("a", 1)))
        );
        assert_eq!(l.state(), SingletonState::Vacant);
    }

    // Check the exact injected expiry boundary without a process/network failure.
    #[test]
    fn takeover_at_injected_expiry() {
        let mut l = SingletonLease::new(cand("b", 5), LeaseParams::default());
        let t0 = Instant::now();
        // 观察：a(seq=3) 是持有者（t0 起租 10s）
        l.observe(t0, &cand("a", 3));
        assert_eq!(l.state(), SingletonState::Follower);
        // No further model observations of a are supplied.
        assert_eq!(l.tick(t0 + Duration::from_secs(9), true), None);
        assert_eq!(l.state(), SingletonState::Follower);
        let t_take = t0 + l.takeover_wait();
        assert_eq!(
            l.tick(t_take - Duration::from_nanos(1), true),
            None,
            "the current holder must not be replaced just before expiry"
        );
        assert_eq!(
            l.tick(t_take, true),
            Some(SingletonTransition::Acquired(cand("b", 5)))
        );
    }

    // 低序号候选不夺权（观察到的持有者更高）
    #[test]
    fn lower_candidate_waits() {
        let mut l = SingletonLease::new(cand("a", 1), LeaseParams::default());
        let t0 = Instant::now();
        l.observe(t0, &cand("b", 9));
        // 租约过期后 a 仍不可夺（序号低于 floor=9）
        assert_eq!(l.tick(t0 + Duration::from_secs(11), true), None);
        // 过期即 Vacant（等待更高序号持有者/重启后新序号）
        assert_eq!(l.state(), SingletonState::Vacant);
        // A separate object with a higher supplied epoch can acquire. This
        // does not test persistence of the first object's epoch floor.
        let mut l2 = SingletonLease::new(cand("a", 10), LeaseParams::default());
        assert_eq!(
            l2.tick(t0 + Duration::from_secs(11), true),
            Some(SingletonTransition::Acquired(cand("a", 10)))
        );
    }

    // Repeated denied confirmations cannot grant local authority.
    #[test]
    fn unconfirmed_candidate_never_acquires() {
        let mut l = SingletonLease::new(cand("a", 1), LeaseParams::default());
        let t0 = Instant::now();
        for t in 0..20 {
            assert_eq!(l.tick(t0 + Duration::from_secs(t), false), None);
        }
        assert_eq!(
            l.state(),
            SingletonState::Vacant,
            "an unconfirmed local candidate must not acquire"
        );
    }
}
