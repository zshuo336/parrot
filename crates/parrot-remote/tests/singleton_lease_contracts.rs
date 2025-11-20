use parrot_remote::singleton::{
    Candidate, LeaseParams, SingletonLease, SingletonState, SingletonTransition,
};
use std::time::{Duration, Instant};

fn candidate(node: &str, seq: u64) -> Candidate {
    Candidate {
        node: node.into(),
        seq,
    }
}

fn lease(node: &str, seq: u64) -> SingletonLease {
    SingletonLease::new(candidate(node, seq), LeaseParams::default())
}

#[test]
fn expired_leader_emits_one_loss_before_any_new_authority() {
    for confirmed in [false, true] {
        let mut value = lease("leader", 7);
        let start = Instant::now();
        assert_eq!(
            value.tick(start, true),
            Some(SingletonTransition::Acquired(candidate("leader", 7)))
        );
        let expiry = start + Duration::from_secs(10);
        assert_eq!(
            value.tick(expiry, confirmed),
            Some(SingletonTransition::SteppedDown(candidate("leader", 7))),
            "expiry must notify the authority owner even with fresh confirmation"
        );
        assert_eq!(value.state(), SingletonState::Vacant);
        assert_eq!(value.current_holder(expiry), None);
        assert_eq!(value.tick(expiry, true), None, "loss is emitted once");
    }
}

#[test]
fn quorum_loss_retires_the_local_incarnation() {
    let mut value = lease("leader", 7);
    let start = Instant::now();
    value.tick(start, true);
    assert_eq!(
        value.tick(start + Duration::from_secs(1), false),
        Some(SingletonTransition::SteppedDown(candidate("leader", 7)))
    );
    for offset in [1, 3, 10, 30] {
        let now = start + Duration::from_secs(offset);
        assert_eq!(value.tick(now, true), None);
        assert_eq!(value.current_holder(now), None);
        assert_eq!(value.state(), SingletonState::Vacant);
    }
}

#[test]
fn observing_ones_own_identity_is_not_a_quorum_confirmation() {
    let mut value = lease("leader", 7);
    let now = Instant::now();
    value.observe(now, &candidate("leader", 7));
    assert_eq!(value.state(), SingletonState::Vacant);
    assert_eq!(value.current_holder(now), None);
    assert_eq!(value.tick(now, false), None);
    assert_eq!(
        value.tick(now, true),
        Some(SingletonTransition::Acquired(candidate("leader", 7)))
    );
}

#[test]
fn a_delayed_self_observation_cannot_restore_retired_authority() {
    let mut value = lease("leader", 7);
    let start = Instant::now();
    value.tick(start, true);
    let lost = start + Duration::from_secs(1);
    value.tick(lost, false);
    value.observe(lost, &candidate("leader", 7));
    assert_eq!(value.state(), SingletonState::Vacant);
    assert_eq!(value.current_holder(lost), None);
    assert_eq!(value.tick(lost, true), None);
}

#[test]
fn expired_remote_identity_cannot_be_restored_by_a_late_observation() {
    for tick_first in [false, true] {
        let mut value = lease("local", 2);
        let start = Instant::now();
        value.observe(start, &candidate("remote", 9));
        let expired = start + Duration::from_secs(10);
        if tick_first {
            value.tick(expired, false);
        }
        value.observe(expired, &candidate("remote", 9));
        assert_eq!(value.current_holder(expired), None);
        assert_eq!(value.state(), SingletonState::Vacant);
    }
}

#[test]
fn an_older_remote_incarnation_cannot_replace_the_retired_floor() {
    let mut value = lease("local", 2);
    let start = Instant::now();
    value.observe(start, &candidate("remote", 9));
    let expired = start + Duration::from_secs(10);
    value.tick(expired, false);
    value.observe(expired, &candidate("stale", 8));
    assert_eq!(value.state(), SingletonState::Vacant);
    assert_eq!(value.current_holder(expired), None);
}

#[test]
fn fresh_observation_renews_the_same_live_remote_holder() {
    let mut value = lease("local", 5);
    let start = Instant::now();
    let holder = candidate("remote", 3);
    value.observe(start, &holder);
    value.observe(start + Duration::from_secs(3), &holder);
    assert_eq!(value.tick(start + Duration::from_secs(10), true), None);
    assert_eq!(value.state(), SingletonState::Follower);
    assert_eq!(
        value.current_holder(start + Duration::from_secs(12)),
        Some(&holder)
    );
    assert_eq!(
        value.tick(start + Duration::from_secs(13), true),
        Some(SingletonTransition::Acquired(candidate("local", 5)))
    );
}

#[test]
fn older_renewal_observations_do_not_shorten_a_live_lease() {
    let mut value = lease("local", 5);
    let start = Instant::now();
    let holder = candidate("remote", 3);
    value.observe(start + Duration::from_secs(5), &holder);
    value.observe(start + Duration::from_secs(2), &holder);
    assert_eq!(
        value.current_holder(start + Duration::from_secs(14)),
        Some(&holder)
    );
}

#[test]
fn older_local_renewals_do_not_shorten_a_live_lease() {
    let mut value = lease("local", 5);
    let start = Instant::now();
    value.tick(start, true);
    value.tick(start + Duration::from_secs(5), true);
    value.tick(start + Duration::from_secs(2), true);
    assert_eq!(
        value.current_holder(start + Duration::from_secs(14)),
        Some(&candidate("local", 5))
    );
}

#[test]
fn invalid_local_identity_never_acquires_authority() {
    let now = Instant::now();
    for identity in [
        candidate("", 1),
        candidate("bad\nnode", 1),
        candidate("node", 0),
    ] {
        let mut value = SingletonLease::new(identity, LeaseParams::default());
        assert_eq!(value.tick(now, true), None);
        assert_eq!(value.state(), SingletonState::Vacant);
        assert_eq!(value.current_holder(now), None);
    }
}

#[test]
fn invalid_observed_identity_cannot_replace_a_live_holder() {
    let now = Instant::now();
    let holder = candidate("remote", 3);
    for identity in [
        candidate("", 9),
        candidate("bad\nnode", 9),
        candidate("node", 0),
    ] {
        let mut value = lease("local", 5);
        value.observe(now, &holder);
        value.observe(now + Duration::from_secs(1), &identity);
        assert_eq!(value.current_holder(now), Some(&holder));
        assert_eq!(value.state(), SingletonState::Follower);
    }
}

fn unrepresentable_params() -> LeaseParams {
    LeaseParams {
        lease: Duration::MAX,
        ..LeaseParams::default()
    }
}

#[test]
fn unrepresentable_local_deadline_is_rejected_without_panicking() {
    let mut value = SingletonLease::new(candidate("local", 5), unrepresentable_params());
    let now = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| value.tick(now, true)));
    assert!(result.is_ok(), "lease arithmetic must not unwind");
    assert_eq!(result.unwrap(), None);
    assert_eq!(value.state(), SingletonState::Vacant);
    assert_eq!(value.current_holder(now), None);
}

#[test]
fn unrepresentable_observed_deadline_is_rejected_without_panicking() {
    let mut value = SingletonLease::new(candidate("local", 5), unrepresentable_params());
    let now = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        value.observe(now, &candidate("remote", 3));
    }));
    assert!(result.is_ok(), "lease arithmetic must not unwind");
    assert_eq!(value.state(), SingletonState::Vacant);
    assert_eq!(value.current_holder(now), None);
}

#[test]
fn a_zero_length_lease_never_publishes_a_holder() {
    let params = LeaseParams {
        lease: Duration::ZERO,
        ..LeaseParams::default()
    };
    let mut value = SingletonLease::new(candidate("local", 5), params);
    let now = Instant::now();
    assert_eq!(value.tick(now, true), None);
    value.observe(now, &candidate("remote", 3));
    assert_eq!(value.state(), SingletonState::Vacant);
    assert_eq!(value.current_holder(now), None);
}

#[test]
fn a_newer_remote_incarnation_can_still_be_observed_after_retirement() {
    let mut value = lease("local", 2);
    let now = Instant::now();
    value.observe(now, &candidate("remote", 3));
    let expired = now + Duration::from_secs(10);
    value.tick(expired, false);
    let next = candidate("remote", 4);
    value.observe(expired, &next);
    assert_eq!(value.current_holder(expired), Some(&next));
    assert_eq!(value.state(), SingletonState::Follower);
    assert_eq!(value.tick(expired, true), None);
}

#[test]
fn candidate_priority_compares_sequence_before_node_name() {
    assert!(candidate("a", 9) > candidate("z", 8));
    assert!(candidate("z", 9) > candidate("a", 9));
    assert_eq!(
        candidate("node", 9).partial_cmp(&candidate("node", 9)),
        Some(std::cmp::Ordering::Equal)
    );
}

#[test]
fn observation_driven_loss_is_delivered_once_by_the_next_tick() {
    for preempted in [false, true] {
        let mut value = lease("local", 5);
        let start = Instant::now();
        value.tick(start, true);
        let now = if preempted {
            let now = start + Duration::from_secs(1);
            value.observe(now, &candidate("remote", 6));
            assert_eq!(value.state(), SingletonState::Follower);
            now
        } else {
            let now = start + Duration::from_secs(10);
            value.observe(now, &candidate("local", 5));
            assert_eq!(value.state(), SingletonState::Vacant);
            now
        };
        assert_eq!(
            value.tick(now, true),
            Some(SingletonTransition::SteppedDown(candidate("local", 5)))
        );
        assert_eq!(value.tick(now, true), None);
        if preempted {
            assert_eq!(value.current_holder(now), Some(&candidate("remote", 6)));
        } else {
            assert_eq!(value.current_holder(now), None);
        }
    }
}

#[test]
fn stale_local_confirmation_cannot_acquire_after_a_newer_expiry() {
    let mut value = lease("local", 5);
    let start = Instant::now();
    value.observe(start, &candidate("remote", 3));
    let expired = start + Duration::from_secs(10);
    value.tick(expired, false);
    assert_eq!(value.tick(start + Duration::from_secs(1), true), None);
    assert_eq!(value.state(), SingletonState::Vacant);
    assert_eq!(value.current_holder(expired), None);
    assert_eq!(
        value.tick(expired, true),
        Some(SingletonTransition::Acquired(candidate("local", 5)))
    );
}

#[test]
fn stale_remote_confirmation_cannot_replace_a_newer_expiry() {
    let mut value = lease("local", 2);
    let start = Instant::now();
    value.observe(start, &candidate("remote", 3));
    let expired = start + Duration::from_secs(10);
    value.tick(expired, false);
    let next = candidate("remote", 4);
    value.observe(start + Duration::from_secs(1), &next);
    assert_eq!(value.state(), SingletonState::Vacant);
    assert_eq!(value.current_holder(expired), None);
    value.observe(expired, &next);
    assert_eq!(value.current_holder(expired), Some(&next));
}

#[test]
fn lower_or_tied_remote_observations_preserve_the_live_holder() {
    let mut value = lease("local", 2);
    let now = Instant::now();
    let holder = candidate("remote", 9);
    value.observe(now, &holder);
    for other in [candidate("older", 8), candidate("tied", 9)] {
        value.observe(now, &other);
        assert_eq!(value.current_holder(now), Some(&holder));
        assert_eq!(value.state(), SingletonState::Follower);
    }
}

#[test]
fn local_admission_excludes_lower_observations_before_retirement() {
    let mut value = lease("local", 9);
    let now = Instant::now();
    value.tick(now, true);
    value.observe(now, &candidate("older", 8));
    assert_eq!(value.current_holder(now), Some(&candidate("local", 9)));
    assert_eq!(value.state(), SingletonState::Leader);
    assert_eq!(value.tick(now, true), None);
}
