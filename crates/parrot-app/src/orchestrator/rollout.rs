//! E2（DEV_09 §3.5 / 09 §6.1）：RolloutTracker——升级状态机。
//!
//! 显式 enum + 转移表（测试穷举源——每条转移弧一个用例）：
//!
//! ```text
//! Pending ──PlanReady──▶ Planning ──(自动)──▶ Draining
//! Draining ──Drained{0 aborted}──▶ Deploying
//! Deploying ──DeployOk──▶ Verifying
//! Verifying ──VerifyOk──▶ Switching ──SwitchDone──▶ Running ──(自动)──▶ Done
//!
//! 异常弧：
//! Verifying ──VerifyFail──▶ RollingBack ──(自动)──▶ Done（回滚完成）
//! Draining/Deploying/Verifying ──NodeLost──▶ RollingBack（任何非终态）
//! 未知事件/非法转移 ──▶ RolloutError::InvalidTransition（状态保持）
//! ```

use std::collections::BTreeMap;

/// 升级阶段（显式状态机——转移表见模块头）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RolloutPhase {
    /// 已创建未开始（等待 PlanReady）。
    Pending,
    /// 计划中（组件依赖序分解——宿主执行计划生成）。
    Planning,
    /// 排空旧实例（等 in-flight 归零/超时）。
    Draining,
    /// 部署新版本实例。
    Deploying,
    /// 健康验证（新实例接收流量前）。
    Verifying,
    /// 路由原子切换（旧→新）。
    Switching,
    /// 新版本运行（切换后观察窗）。
    Running,
    /// 回滚（验证失败/节点丢失——恢复旧版本）。
    RollingBack,
    /// 终态（成功或回滚完成）。
    Done,
}

impl std::fmt::Display for RolloutPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RolloutPhase {
    /// 是否终态（Done 后不再转移）。
    pub fn is_terminal(&self) -> bool {
        matches!(self, RolloutPhase::Done)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RolloutPhase::Pending => "pending",
            RolloutPhase::Planning => "planning",
            RolloutPhase::Draining => "draining",
            RolloutPhase::Deploying => "deploying",
            RolloutPhase::Verifying => "verifying",
            RolloutPhase::Switching => "switching",
            RolloutPhase::Running => "running",
            RolloutPhase::RollingBack => "rolling-back",
            RolloutPhase::Done => "done",
        }
    }
}

/// 状态机事件（宿主动作回执/环境变化——事件驱动转移）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolloutEvent {
    /// 计划就绪（依赖序分解完成）。
    PlanReady,
    /// 排空完成（aborted = 超时中止的实例数——非零仍前进，
    /// 中止计数入报告交运维审计）。
    Drained { aborted: usize },
    /// 新实例部署成功。
    DeployOk,
    /// 验证通过。
    VerifyOk,
    /// 验证失败（原因串——触发回滚）。
    VerifyFail(String),
    /// 节点失联（非终态一律回滚）。
    NodeLost(String),
    /// 路由切换完成。
    SwitchDone,
}

/// 转移产出动作（宿主执行面）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolloutAction {
    /// 无动作（状态推进自身已足够）。
    None,
    /// 开始排空（path 前缀 + 超时——宿主发 DrainComponent）。
    StartDrain { comp: String, timeout_ms: u64 },
    /// 开始部署（宿主发 DeployComponent）。
    StartDeploy { comp: String },
    /// 验证（宿主发健康探测）。
    StartVerify { comp: String },
    /// 路由切换（宿主原子改路由表）。
    SwitchRoutes { comp: String },
    /// 回滚（恢复旧版本——宿主按快照逆向）。
    Rollback { comp: String, reason: String },
    /// 完成（终态通知）。
    Finished,
}

/// 状态机错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RolloutError {
    #[error("invalid transition: {event:?} in {phase}")]
    InvalidTransition { event: String, phase: RolloutPhase },
    #[error("rollout already terminal")]
    AlreadyTerminal,
    #[error("unknown component: {0}")]
    UnknownComponent(String),
}

/// 升级追踪器（单组件粒度——多组件由宿主按依赖序持有多个 tracker）。
pub struct RolloutTracker {
    /// 目标组件名。
    pub comp: String,
    pub phase: RolloutPhase,
    /// 已完成阶段轨迹（转移历史——测试/审计断言源）。
    pub steps_done: Vec<RolloutPhase>,
    /// drain 中止计数累计（DRAIN_ABORTED 指标数据源）。
    pub aborted_total: usize,
    /// 回滚原因（进入 RollingBack 时记录）。
    pub rollback_reason: Option<String>,
}

impl RolloutTracker {
    pub fn new(comp: impl Into<String>) -> Self {
        Self {
            comp: comp.into(),
            phase: RolloutPhase::Pending,
            steps_done: vec![],
            aborted_total: 0,
            rollback_reason: None,
        }
    }

    fn enter(&mut self, p: RolloutPhase) {
        self.steps_done.push(self.phase);
        self.phase = p;
    }

    /// 事件驱动转移（转移表唯一入口——非法事件报错且状态不变）。
    pub fn advance(&mut self, event: RolloutEvent) -> Result<RolloutAction, RolloutError> {
        use RolloutPhase as P;
        if self.phase.is_terminal() {
            return Err(RolloutError::AlreadyTerminal);
        }
        let comp = self.comp.clone();
        match (&self.phase, event) {
            // ── 主线弧 ──
            (P::Pending, RolloutEvent::PlanReady) => {
                self.enter(P::Planning);
                // 计划完成即进入排空（自动弧——计划本身无副作用）
                self.enter(P::Draining);
                Ok(RolloutAction::StartDrain { comp, timeout_ms: 5_000 })
            }
            (P::Draining, RolloutEvent::Drained { aborted }) => {
                self.aborted_total += aborted;
                self.enter(P::Deploying);
                Ok(RolloutAction::StartDeploy { comp })
            }
            (P::Deploying, RolloutEvent::DeployOk) => {
                self.enter(P::Verifying);
                Ok(RolloutAction::StartVerify { comp })
            }
            (P::Verifying, RolloutEvent::VerifyOk) => {
                self.enter(P::Switching);
                Ok(RolloutAction::SwitchRoutes { comp })
            }
            (P::Switching, RolloutEvent::SwitchDone) => {
                self.enter(P::Running);
                Ok(RolloutAction::None)
            }
            (P::Running, RolloutEvent::SwitchDone) => {
                // Running 观察窗结束信号（复用 SwitchDone 语义：切换稳态）
                self.enter(P::Done);
                Ok(RolloutAction::Finished)
            }
            // ── 回滚弧 ──
            (P::Verifying, RolloutEvent::VerifyFail(reason)) => {
                self.rollback_reason = Some(reason.clone());
                self.enter(P::RollingBack);
                Ok(RolloutAction::Rollback { comp, reason })
            }
            (p, RolloutEvent::NodeLost(node)) if !p.is_terminal() => {
                let reason = format!("node lost: {node}");
                self.rollback_reason = Some(reason.clone());
                self.enter(P::RollingBack);
                Ok(RolloutAction::Rollback { comp, reason })
            }
            (P::RollingBack, RolloutEvent::Drained { aborted }) => {
                // 回滚排空完成 → 回滚部署旧版 → 直达终态（简化弧：
                // 旧版本恢复由 Rollback 动作同步完成）
                self.aborted_total += aborted;
                self.enter(P::Done);
                Ok(RolloutAction::Finished)
            }
            // ── 非法弧（状态保持 + 报错）──
            (p, e) => Err(RolloutError::InvalidTransition {
                event: format!("{e:?}"),
                phase: *p,
            }),
        }
    }

    /// 便捷断言：处于某阶段。
    pub fn in_phase(&self, p: RolloutPhase) -> bool {
        self.phase == p
    }
}

/// 多组件升级编排（依赖序驱动——E2 宿主侧组合器）。
///
/// HotSwap：全部组件单 tracker 串行；
/// Rolling：分片逐个（tracker per 分片——同 HotSwap 复用）；
/// Recreate：全停全起（Drain 全部 → Deploy 全部——phase 共享）。
pub struct MultiRollout {
    pub trackers: BTreeMap<String, RolloutTracker>,
}

impl MultiRollout {
    pub fn new(comps: Vec<String>) -> Self {
        Self {
            trackers: comps.into_iter().map(|c| (c.clone(), RolloutTracker::new(c))).collect(),
        }
    }

    /// 广播事件（依赖序由调用方保证——此处逐 tracker 传递）。
    pub fn broadcast(&mut self, e: RolloutEvent) -> Vec<(String, Result<RolloutAction, RolloutError>)> {
        self.trackers
            .iter_mut()
            .map(|(name, t)| (name.clone(), t.advance(e.clone())))
            .collect()
    }

    /// 全部到达终态。
    pub fn all_done(&self) -> bool {
        self.trackers.values().all(|t| t.phase.is_terminal())
    }

    /// 任一回滚。
    pub fn any_rollback(&self) -> bool {
        self.trackers
            .values()
            .any(|t| t.rollback_reason.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 主线全弧：Pending→…→Done（Happy path）。
    #[test]
    fn happy_path_full_arc() {
        let mut t = RolloutTracker::new("frontier");
        assert_eq!(
            t.advance(RolloutEvent::PlanReady).unwrap(),
            RolloutAction::StartDrain { comp: "frontier".into(), timeout_ms: 5_000 }
        );
        assert!(t.in_phase(RolloutPhase::Draining));
        assert_eq!(
            t.advance(RolloutEvent::Drained { aborted: 0 }).unwrap(),
            RolloutAction::StartDeploy { comp: "frontier".into() }
        );
        assert_eq!(
            t.advance(RolloutEvent::DeployOk).unwrap(),
            RolloutAction::StartVerify { comp: "frontier".into() }
        );
        assert_eq!(
            t.advance(RolloutEvent::VerifyOk).unwrap(),
            RolloutAction::SwitchRoutes { comp: "frontier".into() }
        );
        assert_eq!(
            t.advance(RolloutEvent::SwitchDone).unwrap(),
            RolloutAction::None
        );
        assert!(t.in_phase(RolloutPhase::Running));
        assert_eq!(
            t.advance(RolloutEvent::SwitchDone).unwrap(),
            RolloutAction::Finished
        );
        assert!(t.in_phase(RolloutPhase::Done));
        assert_eq!(t.aborted_total, 0);
        // 轨迹完整（9 阶段：Pending/Planning/Draining/Deploying/
        // Verifying/Switching/Running 各一次入史 + Done 前 7 次转移）
        assert_eq!(t.steps_done.len(), 7);
    }

    #[test]
    fn verify_fail_triggers_rollback() {
        let mut t = RolloutTracker::new("c");
        t.advance(RolloutEvent::PlanReady).unwrap();
        t.advance(RolloutEvent::Drained { aborted: 0 }).unwrap();
        t.advance(RolloutEvent::DeployOk).unwrap();
        let a = t
            .advance(RolloutEvent::VerifyFail("health check failed".into()))
            .unwrap();
        assert_eq!(
            a,
            RolloutAction::Rollback { comp: "c".into(), reason: "health check failed".into() }
        );
        assert!(t.in_phase(RolloutPhase::RollingBack));
        assert_eq!(t.rollback_reason.as_deref(), Some("health check failed"));
    }

    #[test]
    fn rollback_completes_to_done() {
        let mut t = RolloutTracker::new("c");
        t.advance(RolloutEvent::PlanReady).unwrap();
        t.advance(RolloutEvent::Drained { aborted: 0 }).unwrap();
        t.advance(RolloutEvent::DeployOk).unwrap();
        t.advance(RolloutEvent::VerifyFail("bad".into())).unwrap();
        let a = t.advance(RolloutEvent::Drained { aborted: 1 }).unwrap();
        assert_eq!(a, RolloutAction::Finished);
        assert!(t.in_phase(RolloutPhase::Done));
        assert_eq!(t.aborted_total, 1);
    }

    #[test]
    fn node_lost_in_draining_rolls_back() {
        let mut t = RolloutTracker::new("c");
        t.advance(RolloutEvent::PlanReady).unwrap();
        let a = t.advance(RolloutEvent::NodeLost("parrot-gw-1".into())).unwrap();
        assert!(matches!(a, RolloutAction::Rollback { .. }));
        assert!(t.in_phase(RolloutPhase::RollingBack));
    }

    #[test]
    fn node_lost_in_deploying_rolls_back() {
        let mut t = RolloutTracker::new("c");
        t.advance(RolloutEvent::PlanReady).unwrap();
        t.advance(RolloutEvent::Drained { aborted: 0 }).unwrap();
        t.advance(RolloutEvent::NodeLost("n1".into())).unwrap();
        assert!(t.in_phase(RolloutPhase::RollingBack));
    }

    #[test]
    fn node_lost_in_verifying_rolls_back() {
        let mut t = RolloutTracker::new("c");
        t.advance(RolloutEvent::PlanReady).unwrap();
        t.advance(RolloutEvent::Drained { aborted: 0 }).unwrap();
        t.advance(RolloutEvent::DeployOk).unwrap();
        t.advance(RolloutEvent::NodeLost("n1".into())).unwrap();
        assert!(t.in_phase(RolloutPhase::RollingBack));
    }

    #[test]
    fn invalid_event_keeps_state() {
        let mut t = RolloutTracker::new("c");
        // Pending 时 DeployOk 非法
        let e = t.advance(RolloutEvent::DeployOk).unwrap_err();
        assert!(matches!(e, RolloutError::InvalidTransition { .. }));
        assert!(t.in_phase(RolloutPhase::Pending));
        // 状态未损：PlanReady 仍可用
        assert!(t.advance(RolloutEvent::PlanReady).is_ok());
    }

    #[test]
    fn terminal_rejects_everything() {
        let mut t = RolloutTracker::new("c");
        t.advance(RolloutEvent::PlanReady).unwrap();
        t.advance(RolloutEvent::Drained { aborted: 0 }).unwrap();
        t.advance(RolloutEvent::DeployOk).unwrap();
        t.advance(RolloutEvent::VerifyOk).unwrap();
        t.advance(RolloutEvent::SwitchDone).unwrap();
        t.advance(RolloutEvent::SwitchDone).unwrap();
        for e in [
            RolloutEvent::PlanReady,
            RolloutEvent::Drained { aborted: 0 },
            RolloutEvent::DeployOk,
            RolloutEvent::NodeLost("n".into()),
        ] {
            assert_eq!(t.advance(e).unwrap_err(), RolloutError::AlreadyTerminal);
        }
    }

    #[test]
    fn drained_with_abort_still_advances() {
        // drain 超时中止（DRAIN_ABORTED 计数）仍前进——超时兜底语义
        let mut t = RolloutTracker::new("c");
        t.advance(RolloutEvent::PlanReady).unwrap();
        let a = t.advance(RolloutEvent::Drained { aborted: 3 }).unwrap();
        assert_eq!(a, RolloutAction::StartDeploy { comp: "c".into() });
        assert_eq!(t.aborted_total, 3);
    }

    #[test]
    fn verify_fail_reason_carried_to_action() {
        let mut t = RolloutTracker::new("c");
        t.advance(RolloutEvent::PlanReady).unwrap();
        t.advance(RolloutEvent::Drained { aborted: 0 }).unwrap();
        t.advance(RolloutEvent::DeployOk).unwrap();
        match t.advance(RolloutEvent::VerifyFail("timeout 3s".into())).unwrap() {
            RolloutAction::Rollback { reason, .. } => assert_eq!(reason, "timeout 3s"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn phase_str_labels() {
        assert_eq!(RolloutPhase::Pending.as_str(), "pending");
        assert_eq!(RolloutPhase::RollingBack.as_str(), "rolling-back");
        assert!(RolloutPhase::Done.is_terminal());
        assert!(!RolloutPhase::Running.is_terminal());
    }

    #[test]
    fn switch_done_in_pending_invalid() {
        let mut t = RolloutTracker::new("c");
        assert!(t.advance(RolloutEvent::SwitchDone).is_err());
    }

    #[test]
    fn deploy_ok_in_draining_invalid() {
        let mut t = RolloutTracker::new("c");
        t.advance(RolloutEvent::PlanReady).unwrap();
        assert!(t.advance(RolloutEvent::DeployOk).is_err());
        assert!(t.in_phase(RolloutPhase::Draining));
    }

    // ── MultiRollout ──

    #[test]
    fn multi_rollout_all_done() {
        let mut m = MultiRollout::new(vec!["a".into(), "b".into()]);
        assert!(!m.all_done());
        m.broadcast(RolloutEvent::PlanReady);
        m.broadcast(RolloutEvent::Drained { aborted: 0 });
        m.broadcast(RolloutEvent::DeployOk);
        m.broadcast(RolloutEvent::VerifyOk);
        m.broadcast(RolloutEvent::SwitchDone);
        m.broadcast(RolloutEvent::SwitchDone);
        assert!(m.all_done());
        assert!(!m.any_rollback());
    }

    #[test]
    fn multi_rollout_partial_rollback_detected() {
        let mut m = MultiRollout::new(vec!["a".into(), "b".into()]);
        m.broadcast(RolloutEvent::PlanReady);
        m.broadcast(RolloutEvent::Drained { aborted: 0 });
        m.broadcast(RolloutEvent::DeployOk);
        // a 验证失败、b 通过
        m.trackers.get_mut("a").unwrap().advance(RolloutEvent::VerifyFail("x".into())).unwrap();
        m.trackers.get_mut("b").unwrap().advance(RolloutEvent::VerifyOk).unwrap();
        assert!(m.any_rollback());
        assert!(!m.all_done());
    }

    #[test]
    fn multi_broadcast_collects_errors_not_panics() {
        let mut m = MultiRollout::new(vec!["a".into()]);
        let results = m.broadcast(RolloutEvent::DeployOk); // Pending 非法
        assert!(results[0].1.is_err());
    }
}
