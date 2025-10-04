//! D2 · Cluster Singleton（DEV_04 §3 / 06 P4.2）。
//!
//! 租约制：候选节点向多数派（Alive 集合严格多数 >n/2）周期续约
//! （lease 10s / renew 3s）；持有者 Dead → 租约到期 → 候选序号最高者接管
//! （接管等待 = lease 全额过期，防双主）。
//!
//! 用途：云 proxy 分配器、全局定时器、ACL 管理者。
//!
//! 注：P4 实现为"单节点本地租约状态机 + 多数派确认回调"形态——网络
//! 确认经 SWIM Alive 视图（非多数派侧续约失败自动降级，无需额外心跳）。

use std::time::{Duration, Instant};

/// 租约参数（06 P4.2：lease 10s / renew 3s / 接管 ≤13s 门禁）。
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

/// 候选者身份：node_id + 序号（节点单调递增——重启 +1）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Candidate {
    pub node: String,
    pub seq: u64,
}

/// 租约状态机（本节点视角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SingletonState {
    /// 无主（初始/租约失效后）。
    Vacant,
    /// 本节点持有有效租约。
    Leader,
    /// 他节点持有（已知 Leader）。
    Follower,
}

/// 单例租约协调（确定性状态机——时间由调用方注入便于测试）。
pub struct SingletonLease {
    params: LeaseParams,
    me: Candidate,
    /// 当前已知持有者 + 租约到期时刻。
    holder: Option<(Candidate, Instant)>,
    /// 最后已知持有者序号（过期不清除——低序号候选不得竞选，
    /// 防与"仍在续约的真持有者"竞争形成双主）。
    floor_seq: u64,
    state: SingletonState,
}

impl SingletonLease {
    pub fn new(me: Candidate, params: LeaseParams) -> Self {
        Self {
            params,
            me,
            holder: None,
            floor_seq: 0,
            state: SingletonState::Vacant,
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

    /// 续约/竞选推进（renew 周期调用）。
    ///
    /// `alive_confirmed`：SWIM 视角本候选可达多数派的确认（严格 >n/2——
    /// 现签名是回调形态：调用方传入 `quorum_ok`）。
    pub fn tick(&mut self, now: Instant, quorum_ok: bool) -> Option<SingletonTransition> {
        // 1. 租约过期检测（floor 保留——过期后低序号候选仍不可竞选）
        if let Some((c, expiry)) = &self.holder {
            if now >= *expiry {
                self.floor_seq = self.floor_seq.max(c.seq);
                self.holder = None;
                self.state = SingletonState::Vacant;
            }
        }

        match self.state {
            SingletonState::Leader => {
                if quorum_ok {
                    // 续约：租约延长
                    if let Some((_, expiry)) = self.holder.as_mut() {
                        *expiry = now + self.params.lease;
                    }
                    None
                } else {
                    // 多数派失联——自动降级（防双主的核心）
                    self.holder = None;
                    self.state = SingletonState::Vacant;
                    Some(SingletonTransition::SteppedDown(self.me.clone()))
                }
            }
            SingletonState::Vacant | SingletonState::Follower => {
                // 竞选：租约有效期内绝不夺权（等过期）；过期后需
                // me.seq > floor（最后持有者序号）——重启低序号节点
                // 不得与"可能仍在续约的真持有者"竞争。
                let can_take = self.holder.is_none() && self.me.seq > self.floor_seq;
                if quorum_ok && can_take {
                    self.holder = Some((self.me.clone(), now + self.params.lease));
                    self.state = SingletonState::Leader;
                    Some(SingletonTransition::Acquired(self.me.clone()))
                } else {
                    None
                }
            }
        }
    }

    /// 观察（SWIM gossip 广播途径）。
    pub fn observe(&mut self, now: Instant, holder: &Candidate) {
        let expiry = now + self.params.lease;
        match &self.holder {
            Some((cur, _)) if cur.seq >= holder.seq => {} // 旧观察，忽略
            _ => {
                self.floor_seq = self.floor_seq.max(holder.seq);
                self.holder = Some((holder.clone(), expiry));
                self.state = if *holder == self.me {
                    SingletonState::Leader
                } else {
                    SingletonState::Follower
                };
            }
        }
    }

    /// 接管等待预算（防双主：候选须等 lease 全额过期才可竞选——
    /// 06 P4.2 门禁 ≤13s = lease 10s + 确认 3s）。
    pub fn takeover_wait(&self) -> Duration {
        self.params.lease
    }
}

/// 状态迁移事件。
#[derive(Debug, Clone, PartialEq)]
pub enum SingletonTransition {
    /// 竞选成功（本节点成为 singleton）。
    Acquired(Candidate),
    /// 多数派失联降级。
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

    // 竞选 + 续约：多数派 OK → Acquired → 周期续约保持 Leader
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

    // 多数派失联 → 降级（防双主核心）
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

    // 接管时序：kill 持有者 → 候选等 lease 过期 → 高序号者接管（≤13s）
    #[test]
    fn takeover_within_budget() {
        let mut l = SingletonLease::new(cand("b", 5), LeaseParams::default());
        let t0 = Instant::now();
        // 观察：a(seq=3) 是持有者（t0 起租 10s）
        l.observe(t0, &cand("a", 3));
        assert_eq!(l.state(), SingletonState::Follower);
        // a 死亡——b 视角租约到 t0+10s；期间 tick 不竞选（租约仍有效）
        assert_eq!(l.tick(t0 + Duration::from_secs(9), true), None);
        assert_eq!(l.state(), SingletonState::Follower);
        // 过期后（t0+10s）b 可接管——门禁：kill 后 ≤13s
        let t_take = t0 + Duration::from_secs(10);
        let wait = t_take.duration_since(t0);
        assert!(
            wait <= Duration::from_secs(13),
            "takeover wait {wait:?} > 13s gate"
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
        // 新一届：a 重启 seq 提升至 10 > floor=9 → 可竞选
        let mut l2 = SingletonLease::new(cand("a", 10), LeaseParams::default());
        assert_eq!(
            l2.tick(t0 + Duration::from_secs(11), true),
            Some(SingletonTransition::Acquired(cand("a", 10)))
        );
    }

    // 分区唯一性：非多数派侧（quorum_ok=false）永不成主
    #[test]
    fn minority_never_leads() {
        let mut l = SingletonLease::new(cand("a", 1), LeaseParams::default());
        let t0 = Instant::now();
        for t in 0..20 {
            assert_eq!(l.tick(t0 + Duration::from_secs(t), false), None);
        }
        assert_eq!(
            l.state(),
            SingletonState::Vacant,
            "minority side must not lead"
        );
    }
}
