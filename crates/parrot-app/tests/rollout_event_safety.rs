use parrot_app::orchestrator::rollout::MultiRollout;
use parrot_app::orchestrator::{
    RolloutAction as A, RolloutEvent as E, RolloutPhase as P, RolloutTracker,
};

fn snapshot(t: &RolloutTracker) -> (String, P, Vec<P>, usize, Option<String>) {
    (
        t.comp.clone(),
        t.phase,
        t.steps_done.clone(),
        t.aborted_total,
        t.rollback_reason.clone(),
    )
}
fn verifying(aborted: usize) -> RolloutTracker {
    let mut t = RolloutTracker::new("worker");
    t.advance(E::PlanReady).unwrap();
    t.advance(E::Drained { aborted }).unwrap();
    t.advance(E::DeployOk).unwrap();
    t
}

#[test]
fn duplicate_switch_receipt_does_not_complete_observation_window() {
    let mut t = verifying(0);
    t.advance(E::VerifyOk).unwrap();
    assert_eq!(t.advance(E::SwitchDone).unwrap(), A::None);
    let before = snapshot(&t);
    for _ in 0..32 {
        assert_eq!(t.advance(E::SwitchDone).unwrap(), A::None);
        assert_eq!(snapshot(&t), before);
    }
    assert_eq!(t.phase, P::Running);
}

#[test]
fn repeated_node_loss_preserves_first_rollback_and_history() {
    for verification_failure in [false, true] {
        let mut t = verifying(7);
        let event = if verification_failure {
            E::VerifyFail("unhealthy".into())
        } else {
            E::NodeLost("first".into())
        };
        assert!(matches!(t.advance(event).unwrap(), A::Rollback { .. }));
        let before = snapshot(&t);
        for i in 0..1000 {
            assert_eq!(
                t.advance(E::NodeLost(format!("again-{i}"))).unwrap(),
                A::None
            );
            assert_eq!(snapshot(&t), before);
        }
    }
}

#[test]
fn delayed_forward_drain_cannot_acknowledge_rollback() {
    let mut t = RolloutTracker::new("worker");
    t.advance(E::PlanReady).unwrap();
    t.advance(E::NodeLost("node".into())).unwrap();
    let before = snapshot(&t);
    assert!(t.advance(E::Drained { aborted: 1 }).is_err());
    assert_eq!(snapshot(&t), before);
}

#[test]
fn delayed_drain_does_not_overflow_after_prior_maximum_receipt() {
    let mut t = verifying(usize::MAX);
    t.advance(E::VerifyFail("unhealthy".into())).unwrap();
    let before = snapshot(&t);
    assert!(t.advance(E::Drained { aborted: 1 }).is_err());
    assert_eq!(snapshot(&t), before);
}

#[test]
fn forward_counter_overflow_is_an_error_without_partial_transition() {
    let mut t = RolloutTracker::new("worker");
    t.advance(E::PlanReady).unwrap();
    // The counter is public; a host-restored value must still be checked.
    t.aborted_total = usize::MAX;
    let before = snapshot(&t);
    let error = t
        .advance(E::Drained { aborted: 1 })
        .expect_err("overflow accepted");
    assert!(error.to_string().contains("overflow"));
    assert_eq!(snapshot(&t), before);
    t.advance(E::Drained { aborted: 0 }).unwrap();
    assert_eq!(t.aborted_total, usize::MAX);
    assert_eq!(t.phase, P::Deploying);
}

#[test]
fn repeated_broadcast_does_not_reissue_rollback_for_any_component() {
    let mut group = MultiRollout::new(vec!["z".into(), "a".into()]);
    for (_, result) in group.broadcast(E::PlanReady) {
        result.unwrap();
    }
    for (_, result) in group.broadcast(E::NodeLost("first".into())) {
        assert!(matches!(result.unwrap(), A::Rollback { .. }));
    }
    for _ in 0..32 {
        let results = group.broadcast(E::NodeLost("again".into()));
        assert_eq!(
            results.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
            ["a", "z"]
        );
        for (_, result) in results {
            assert_eq!(result.unwrap(), A::None);
        }
    }
    assert!(!group.all_done());
    assert!(group.any_rollback());
    for t in group.trackers.values() {
        assert_eq!(t.rollback_reason.as_deref(), Some("node lost: first"));
        assert_eq!(t.steps_done, [P::Pending, P::Planning, P::Draining]);
    }
}

#[test]
fn illegal_and_terminal_events_preserve_all_public_state() {
    let mut t = RolloutTracker::new("worker");
    for phase in [P::Pending, P::Done] {
        t.phase = phase;
        let before = snapshot(&t);
        for event in [
            E::DeployOk,
            E::VerifyOk,
            E::SwitchDone,
            E::Drained {
                aborted: usize::MAX,
            },
        ] {
            assert!(t.advance(event).is_err());
            assert_eq!(snapshot(&t), before);
        }
    }
}
