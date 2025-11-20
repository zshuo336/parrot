use super::*;
use crate::{acl::AclRules, receptionist::ReceptionistFacade};
use parrot_api::receptionist::{ReceptionistGateway, RegistrationScope};
use std::sync::Arc;

fn key(s: &str) -> ReceptionistKey {
    ReceptionistKey::new(s).unwrap()
}
fn limits(count: usize, bytes: usize) -> RegistrationLimits {
    RegistrationLimits {
        max_registrations: count,
        max_reserved_bytes: bytes,
    }
}
fn sync(node: &str, paths: &[&str]) -> ReceptionistSync {
    ReceptionistSync {
        from: node.into(),
        entries: paths
            .iter()
            .map(|path| ReceptionistEntry {
                node: node.into(),
                key: key("edge/service"),
                remote_path: (*path).into(),
            })
            .collect(),
    }
}

// Recompute from actual table contents rather than replaying budget operations.
fn assert_usage(table: &Receptionist) {
    let mut total = Usage::default();
    let mut nodes: HashMap<String, Usage> = HashMap::new();
    let mut identities = HashSet::new();
    for entry in table.entries.values().flatten() {
        assert!(
            identities.insert(entry),
            "one charge per distinct registration"
        );
        let bytes = std::mem::size_of::<ReceptionistEntry>() as u128
            + 512
            + (3 * entry.key.as_str().len() + 2 * entry.remote_path.len() + entry.node.len())
                as u128;
        total.entries += 1;
        total.bytes += bytes;
        let node = nodes.entry(entry.node.clone()).or_default();
        node.entries += 1;
        node.bytes += bytes;
    }
    assert_eq!(
        table.registrations.total, total,
        "table budget must match actual retained entries"
    );
    assert_eq!(
        table.registrations.nodes, nodes,
        "node credits must match actual retained entries"
    );
}

#[test]
fn count_budget_is_shared_by_local_remote_keys_and_duplicate_replay() {
    let mut table = Receptionist::with_registration_limits(limits(2, usize::MAX));
    table
        .try_register("local", &key("edge/local"), "/local")
        .unwrap();
    table
        .try_merge_sync(&sync("peer", &["/remote", "/remote"]))
        .unwrap();
    table
        .try_register("local", &key("edge/local"), "/local")
        .unwrap();
    assert!(
        table
            .try_merge_sync(&sync("peer", &["/remote", "/extra"]))
            .is_err(),
        "distinct remote entries must share the local registration count"
    );
    assert!(table
        .try_register("local", &key("edge/other"), "/extra")
        .is_err());
    assert_eq!(table.snapshot("peer"), sync("peer", &["/remote"]).entries);
    assert_usage(&table);
}

#[test]
fn byte_boundary_uses_utf8_bytes_and_rejects_one_byte_short() {
    let k = key("edge/utf8");
    let path = "/service/\u{4e2d}";
    let expected = std::mem::size_of::<ReceptionistEntry>()
        + 512
        + 3 * k.as_str().len()
        + 2 * path.len()
        + "node".len();
    assert_eq!(registration_bytes("node", &k, path), expected as u128);
    let mut table = Receptionist::with_registration_limits(limits(2, expected - 1));
    assert!(
        table.try_register("node", &k, path).is_err(),
        "one byte short must reject registration"
    );
    assert_usage(&table);
    table.registrations.limits.max_reserved_bytes = expected;
    table.try_register("node", &k, path).unwrap();
    table.try_register("node", &k, path).unwrap();
    assert!(table.try_register("node", &k, "/other").is_err());
    assert_usage(&table);
}

#[test]
fn snapshot_replacement_credits_old_node_and_is_atomic_on_rejection() {
    let k = key("edge/service");
    let bytes = registration_bytes("peer", &k, "/old").try_into().unwrap();
    let mut table = Receptionist::with_registration_limits(limits(1, bytes));
    table.try_merge_sync(&sync("peer", &["/old"])).unwrap();
    let mut events = table.try_subscribe(&k, 8).unwrap();
    events.try_recv().unwrap();
    let delta = table.try_merge_sync(&sync("peer", &["/new", "/new"]));
    assert!(
        delta.is_ok(),
        "replacement must credit the old node before admission"
    );
    let delta = delta.unwrap();
    assert_eq!(delta.len(), 2);
    let before = table.snapshot("peer");
    assert!(table
        .try_merge_sync(&sync("peer", &["/bad", "/too-much"]))
        .is_err());
    assert_eq!(
        table.snapshot("peer"),
        before,
        "rejected snapshot must retain the complete old view"
    );
    assert_eq!(
        events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    assert_usage(&table);
    assert_eq!(table.try_merge_sync(&sync("peer", &[])).unwrap().len(), 1);
    table.try_register("local", &k, "/ok").unwrap();
    assert_usage(&table);
}

#[test]
fn all_removal_paths_refund_exact_node_and_total_usage() {
    let k = key("edge/service");
    let mut table = Receptionist::with_registration_limits(limits(4, usize::MAX));
    for node in ["local", "dead", "gone", "live"] {
        table.try_merge_sync(&sync(node, &["/same"])).unwrap();
        assert_usage(&table);
    }
    assert_usage(&table);
    assert_eq!(table.remove_node("dead").len(), 1);
    assert_usage(&table);
    assert_eq!(
        table
            .remove_unavailable("local", &HashSet::from(["live".into()]))
            .len(),
        1
    );
    assert_usage(&table);
    table.deregister(&k, "/same");
    assert_usage(&table);
    assert_eq!(table.registrations.total.entries, 0);
    assert!(table.remove_node("dead").is_empty());
    for n in 0..4 {
        table.try_register("new", &k, &format!("/{n}")).unwrap();
    }
    assert_usage(&table);
    let retired = table.retire_contents();
    assert_usage(&table);
    assert_eq!(table.registrations.limits, limits(4, usize::MAX));
    assert_eq!(table.registrations.total.entries, 0);
    drop(retired);
    drop(table.retire_contents());
    assert_usage(&table);
}

#[test]
fn unchecked_legacy_over_budget_state_can_shrink_and_recover() {
    let mut table = Receptionist::with_registration_limits(limits(0, 0));
    for path in ["/first-long", "/second-long"] {
        table.register("peer", &key("edge/service"), path);
    }
    assert_usage(&table);
    assert!(table
        .try_register("peer", &key("edge/service"), "/first-long")
        .is_ok());
    assert!(table
        .try_merge_sync(&sync("peer", &["/first-long", "/second-long"]))
        .is_ok());
    assert!(
        table.try_merge_sync(&sync("peer", &["/short"])).is_ok(),
        "shrinking legacy state must remain possible"
    );
    assert!(table
        .try_merge_sync(&sync("peer", &["/much-longer-than-before"]))
        .is_err());
    assert_usage(&table);
    table.try_merge_sync(&sync("peer", &[])).unwrap();
    assert_usage(&table);
    assert!(table
        .try_register("peer", &key("edge/service"), "/new")
        .is_err());
}

#[test]
fn invalid_snapshot_envelope_or_owner_never_replaces_existing_entries() {
    let mut table = Receptionist::with_registration_limits(limits(2, usize::MAX));
    table.try_merge_sync(&sync("peer", &["/original"])).unwrap();
    let before = table.snapshot("peer");
    let mut invalid = sync("peer", &["/valid", "/foreign"]);
    invalid.entries[1].node = "other".into();
    assert!(table.try_merge_sync(&invalid).is_err());
    invalid.entries[1].node = "peer".into();
    invalid.entries[1].remote_path = "x".repeat(crate::receptionist::MAX_EVENT_BYTES);
    assert!(table.try_merge_sync(&invalid).is_err());
    assert_eq!(table.snapshot("peer"), before);
    assert_usage(&table);
}

#[test]
fn maximum_configuration_and_projection_use_wide_arithmetic() {
    let mut table = Receptionist::with_registration_limits(limits(usize::MAX, usize::MAX));
    table
        .try_register("n", &key("edge/maximum"), "/one")
        .unwrap();
    assert_usage(&table);
    let mut budget = Budget::new(limits(usize::MAX, usize::MAX));
    budget.total = Usage {
        entries: usize::MAX as u128,
        bytes: usize::MAX as u128,
    };
    assert!(budget
        .check(Usage {
            entries: budget.total.entries + 1,
            bytes: budget.total.bytes
        })
        .is_err());
    assert!(budget
        .check(Usage {
            entries: budget.total.entries,
            bytes: budget.total.bytes + 1
        })
        .is_err());
}

#[test]
fn snapshot_byte_limit_is_atomic_and_credits_only_the_replaced_node() {
    let k = key("edge/service");
    let old = registration_bytes("peer", &k, "/old");
    let local = registration_bytes("local", &k, "/retained");
    let mut table = Receptionist::with_registration_limits(limits(10, (old + local) as usize));
    table.try_register("local", &k, "/retained").unwrap();
    table
        .try_merge_sync(&sync("peer", &["/old", "/old"]))
        .unwrap();
    assert!(table.try_merge_sync(&sync("peer", &["/new"])).is_ok());
    assert!(
        table.try_merge_sync(&sync("peer", &["/longer"])).is_err(),
        "snapshot byte limit must reject growth without stealing another node credit"
    );
    assert_eq!(table.snapshot("peer"), sync("peer", &["/new"]).entries);
    assert_eq!(table.snapshot("local").len(), 1);
    assert_usage(&table);
    table.deregister(&k, "/retained");
    table.try_merge_sync(&sync("peer", &["/longer"])).unwrap();
    assert_usage(&table);
}

#[tokio::test]
async fn scoped_transfer_at_capacity_preserves_new_owner_and_refunds_on_retirement() {
    let gateway = ReceptionistFacade::with_registration_limits("n", limits(1, usize::MAX))
        .with_acl(AclRules::permissive());
    let k = key("edge/scoped");
    let old = Arc::new(RegistrationScope::default());
    let new = Arc::new(RegistrationScope::default());
    gateway
        .register_scoped(k.clone(), "/owned".into(), old.clone())
        .await
        .unwrap();
    gateway
        .register_scoped(k.clone(), "/owned".into(), new.clone())
        .await
        .unwrap();
    assert!(gateway.register(k.clone(), "/extra".into()).await.is_err());
    old.retire();
    old.join().await.unwrap();
    assert_eq!(gateway.inner.lock().unwrap().lookup(&k).len(), 1);
    assert_usage(&gateway.inner.lock().unwrap());
    new.retire();
    new.join().await.unwrap();
    assert_usage(&gateway.inner.lock().unwrap());
    assert_eq!(gateway.inner.lock().unwrap().registrations.total.entries, 0);
    gateway.register(k, "/extra".into()).await.unwrap();
}

#[tokio::test]
async fn rejected_scoped_publications_do_not_consume_cleanup_bindings_or_events() {
    let gateway = ReceptionistFacade::with_registration_limits("n", limits(1, usize::MAX))
        .with_acl(AclRules::permissive());
    let scope = Arc::new(RegistrationScope::default());
    let k = key("edge/scoped");
    gateway
        .register(k.clone(), "/occupied".into())
        .await
        .unwrap();
    let mut stream = gateway.subscribe(k.clone()).await.unwrap();
    stream.try_recv().unwrap();
    for n in 0..=RegistrationScope::MAX_BINDINGS {
        assert!(gateway
            .register_scoped(k.clone(), format!("/denied/{n}"), scope.clone())
            .await
            .is_err());
    }
    assert_eq!(
        stream.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    gateway
        .deregister(k.clone(), "/occupied".into())
        .await
        .unwrap();
    assert!(
        gateway
            .register_scoped(k, "/accepted".into(), scope.clone())
            .await
            .is_ok(),
        "registration budget rejection must not consume generation bindings"
    );
    scope.retire();
    scope.join().await.unwrap();
    assert_usage(&gateway.inner.lock().unwrap());
    assert_eq!(gateway.inner.lock().unwrap().registrations.total.entries, 0);
}
