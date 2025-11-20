use parrot_app::orchestrator::{
    rollout::MultiRollout, RolloutAction as A, RolloutError, RolloutEvent as E, RolloutPhase as P,
    RolloutTracker,
};

fn snapshot(t: &RolloutTracker) -> (P, Vec<P>, usize, Option<String>) {
    (
        t.phase,
        t.steps_done.clone(),
        t.aborted_total,
        t.rollback_reason.clone(),
    )
}
fn verifying(aborted: usize) -> RolloutTracker {
    let mut t = RolloutTracker::new("worker");
    assert!(matches!(
        t.advance(E::PlanReady).unwrap(),
        A::StartDrain { .. }
    ));
    assert!(matches!(
        t.advance(E::Drained { aborted }).unwrap(),
        A::StartDeploy { .. }
    ));
    assert!(matches!(
        t.advance(E::DeployOk).unwrap(),
        A::StartVerify { .. }
    ));
    t
}

#[test]
fn all_phase_event_pairs_follow_independent_contract() {
    // Planning is an automatic intermediate phase. Public fields still permit
    // hosts to restore it, so include it explicitly in this 9 x 9 matrix.
    let phases = [
        P::Pending,
        P::Planning,
        P::Draining,
        P::Deploying,
        P::Verifying,
        P::Switching,
        P::Running,
        P::RollingBack,
        P::Done,
    ];
    let events = [
        E::PlanReady,
        E::Drained { aborted: 3 },
        E::DeployOk,
        E::VerifyOk,
        E::VerifyFail("bad".into()),
        E::NodeLost("lost".into()),
        E::SwitchDone,
        E::ObservationComplete,
        E::RollbackComplete { aborted: 3 },
    ];
    let mut checked = 0;
    for phase in phases {
        for (event_index, event) in events.iter().enumerate() {
            let mut t = RolloutTracker::new("worker");
            t.phase = phase;
            t.aborted_total = 7;
            t.rollback_reason = (phase == P::RollingBack).then(|| "original".into());
            let before = snapshot(&t);
            // Event indices are fixed by the literal specification vector above.
            let expected = match (phase, event_index) {
                (P::Pending, 0) => Some((
                    P::Draining,
                    A::StartDrain {
                        comp: "worker".into(),
                        timeout_ms: 5000,
                    },
                )),
                (P::Draining, 1) => Some((
                    P::Deploying,
                    A::StartDeploy {
                        comp: "worker".into(),
                    },
                )),
                (P::Deploying, 2) => Some((
                    P::Verifying,
                    A::StartVerify {
                        comp: "worker".into(),
                    },
                )),
                (P::Verifying, 3) => Some((
                    P::Switching,
                    A::SwitchRoutes {
                        comp: "worker".into(),
                    },
                )),
                (P::Verifying, 4) => Some((
                    P::RollingBack,
                    A::Rollback {
                        comp: "worker".into(),
                        reason: "bad".into(),
                    },
                )),
                (P::Switching, 6) => Some((P::Running, A::None)),
                (P::Running, 6) | (P::RollingBack, 5) => Some((phase, A::None)),
                (P::Running, 7) | (P::RollingBack, 8) => Some((P::Done, A::Finished)),
                (
                    P::Pending
                    | P::Planning
                    | P::Draining
                    | P::Deploying
                    | P::Verifying
                    | P::Switching
                    | P::Running,
                    5,
                ) => Some((
                    P::RollingBack,
                    A::Rollback {
                        comp: "worker".into(),
                        reason: "node lost: lost".into(),
                    },
                )),
                _ => None,
            };
            let result = t.advance(event.clone());
            match expected {
                Some((next, action)) => {
                    assert_eq!(result.unwrap(), action, "{phase:?} / {event:?}");
                    assert_eq!(t.phase, next);
                    let history = if phase == next {
                        vec![]
                    } else if phase == P::Pending && event_index == 0 {
                        vec![P::Pending, P::Planning]
                    } else {
                        vec![phase]
                    };
                    assert_eq!(t.steps_done, history);
                    assert_eq!(
                        t.aborted_total,
                        if event_index == 1 || event_index == 8 {
                            10
                        } else {
                            7
                        }
                    );
                    let reason = if phase == P::RollingBack {
                        Some("original")
                    } else if event_index == 4 {
                        Some("bad")
                    } else if event_index == 5 {
                        Some("node lost: lost")
                    } else {
                        None
                    };
                    assert_eq!(t.rollback_reason.as_deref(), reason);
                }
                None => {
                    let error = result.expect_err("illegal pair accepted");
                    if phase == P::Done {
                        assert_eq!(error, RolloutError::AlreadyTerminal);
                    } else {
                        assert!(
                            matches!(error, RolloutError::InvalidTransition { phase: actual, .. } if actual == phase)
                        );
                    }
                    assert_eq!(snapshot(&t), before);
                }
            }
            assert_eq!(t.comp, "worker");
            checked += 1;
        }
    }
    assert_eq!(checked, 81);
    println!("checked {checked} explicit phase/event pairs");
}

#[test]
fn observation_requires_explicit_receipt_and_preserves_transition_history() {
    let mut t = verifying(2);
    assert!(t.advance(E::ObservationComplete).is_err());
    t.advance(E::VerifyOk).unwrap();
    assert!(t.advance(E::ObservationComplete).is_err());
    t.advance(E::SwitchDone).unwrap();
    let before = snapshot(&t);
    for _ in 0..16 {
        assert_eq!(t.advance(E::SwitchDone).unwrap(), A::None);
    }
    assert_eq!(snapshot(&t), before);
    assert_eq!(t.advance(E::ObservationComplete).unwrap(), A::Finished);
    assert_eq!(
        t.steps_done,
        [
            P::Pending,
            P::Planning,
            P::Draining,
            P::Deploying,
            P::Verifying,
            P::Switching,
            P::Running
        ]
    );
    assert_eq!(t.aborted_total, 2);
    assert_eq!(
        t.advance(E::ObservationComplete),
        Err(RolloutError::AlreadyTerminal)
    );
}

#[test]
fn rollback_requires_restore_receipt_and_preserves_first_cause() {
    let mut t = verifying(5);
    assert!(t.advance(E::RollbackComplete { aborted: 3 }).is_err());
    t.advance(E::VerifyFail("first failure".into())).unwrap();
    let before = snapshot(&t);
    assert!(t.advance(E::Drained { aborted: 999 }).is_err());
    assert_eq!(t.advance(E::NodeLost("later".into())).unwrap(), A::None);
    assert_eq!(snapshot(&t), before);
    assert_eq!(
        t.advance(E::RollbackComplete { aborted: 3 }).unwrap(),
        A::Finished
    );
    assert_eq!(t.aborted_total, 8);
    assert_eq!(t.rollback_reason.as_deref(), Some("first failure"));
    assert_eq!(
        t.steps_done,
        [
            P::Pending,
            P::Planning,
            P::Draining,
            P::Deploying,
            P::Verifying,
            P::RollingBack
        ]
    );
    let done = snapshot(&t);
    assert_eq!(
        t.advance(E::RollbackComplete { aborted: 3 }),
        Err(RolloutError::AlreadyTerminal)
    );
    assert_eq!(snapshot(&t), done);
}

#[test]
fn rollback_overflow_preserves_state_and_can_retry_zero() {
    let mut t = verifying(usize::MAX);
    t.advance(E::VerifyFail("bad".into())).unwrap();
    let before = snapshot(&t);
    assert_eq!(
        t.advance(E::RollbackComplete { aborted: 1 }),
        Err(RolloutError::AbortedCountOverflow {
            current: usize::MAX,
            additional: 1
        })
    );
    assert_eq!(snapshot(&t), before);
    assert_eq!(
        t.advance(E::RollbackComplete { aborted: 0 }).unwrap(),
        A::Finished
    );
    assert_eq!(t.aborted_total, usize::MAX);
}

#[test]
fn forward_counter_accepts_exact_max_then_rejects_overflow() {
    for (current, additional, total) in [
        (0, usize::MAX, usize::MAX),
        (usize::MAX - 1, 1, usize::MAX),
        (usize::MAX, 0, usize::MAX),
    ] {
        let mut t = RolloutTracker::new("worker");
        t.advance(E::PlanReady).unwrap();
        t.aborted_total = current;
        t.advance(E::Drained {
            aborted: additional,
        })
        .unwrap();
        assert_eq!(t.aborted_total, total);
        assert_eq!(t.phase, P::Deploying);
    }
    let mut t = RolloutTracker::new("worker");
    t.advance(E::PlanReady).unwrap();
    t.aborted_total = 1;
    let before = snapshot(&t);
    assert_eq!(
        t.advance(E::Drained {
            aborted: usize::MAX
        }),
        Err(RolloutError::AbortedCountOverflow {
            current: 1,
            additional: usize::MAX
        })
    );
    assert_eq!(snapshot(&t), before);
}

#[test]
fn mixed_broadcast_continues_after_counter_error() {
    let mut group = MultiRollout::new(vec!["c".into(), "b".into(), "a".into()]);
    for t in group.trackers.values_mut() {
        t.advance(E::PlanReady).unwrap();
    }
    group.trackers.get_mut("a").unwrap().aborted_total = usize::MAX;
    let before = snapshot(&group.trackers["a"]);
    let result = group.broadcast(E::Drained { aborted: 1 });
    assert_eq!(
        result.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    assert_eq!(
        result[0].1,
        Err(RolloutError::AbortedCountOverflow {
            current: usize::MAX,
            additional: 1
        })
    );
    for index in [1, 2] {
        assert_eq!(
            result[index].1,
            Ok(A::StartDeploy {
                comp: result[index].0.clone()
            })
        );
    }
    assert_eq!(snapshot(&group.trackers["a"]), before);
    for name in ["b", "c"] {
        assert_eq!(group.trackers[name].phase, P::Deploying);
    }
    assert!(!group.all_done());
    assert!(!group.any_rollback());
}
