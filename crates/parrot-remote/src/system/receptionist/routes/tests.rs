use super::*;
use crate::{acl::AclRules, LocalLookup, RemoteActorSystem, RemoteConfig};
use parrot_api::types::BoxedActorRef;
use parrot_config::receptionist::RegistrationLimits;
use std::sync::Arc;
use tokio::sync::mpsc;

mod last_owner_contracts;

fn sender(node: &str) -> (FrameSender, mpsc::Receiver<crate::frame::Frame>) {
    let (tx, rx) = mpsc::channel(1);
    (FrameSender::new(tx, node), rx)
}
fn key(value: &str) -> ReceptionistKey {
    ReceptionistKey::new(value).unwrap()
}
fn view(path: &[&str], service: &str, suffixes: &[&str]) -> Advertisement {
    Advertisement {
        path: path.iter().map(|node| (*node).into()).collect(),
        entries: suffixes
            .iter()
            .map(|suffix| ReceptionistEntry {
                node: path[0].into(),
                key: key(service),
                remote_path: format!("parrot://{}/user/{suffix}", path[0]),
            })
            .collect(),
    }
}
fn prune(
    routes: &mut Routes,
    senders: &[&FrameSender],
    table: &mut Receptionist,
) -> Vec<ReceptionistEvent> {
    routes.prune(
        &senders.iter().map(|sender| sender.dispatch_key()).collect(),
        &["a", "b", "c", "d", "x", "local"]
            .into_iter()
            .map(String::from)
            .collect(),
        table,
    )
}
fn paths(table: &Receptionist, owner: &str) -> Vec<String> {
    let mut paths: Vec<_> = table
        .snapshot(owner)
        .into_iter()
        .map(|e| e.remote_path)
        .collect();
    paths.sort();
    paths
}

#[test]
fn selects_per_key_with_deterministic_shortest_path_and_complete_omission() {
    let (b, _b_rx) = sender("b");
    let (c, _c_rx) = sender("c");
    let mut routes = Routes::default();
    let mut table = Receptionist::default();
    routes
        .replace(
            &c,
            vec![
                view(&["d", "c"], "edge/one", &["c"]),
                view(&["d", "c"], "edge/two", &["two"]),
            ],
            &mut table,
        )
        .unwrap();
    routes
        .replace(&b, vec![view(&["d", "b"], "edge/one", &["b"])], &mut table)
        .unwrap();
    assert_eq!(
        paths(&table, "d"),
        ["parrot://d/user/b", "parrot://d/user/two"]
    );
    routes
        .replace(&c, vec![view(&["d", "c"], "edge/one", &["c"])], &mut table)
        .unwrap();
    assert_eq!(paths(&table, "d"), ["parrot://d/user/b"]);
    routes.replace(&b, Vec::new(), &mut table).unwrap();
    assert_eq!(paths(&table, "d"), ["parrot://d/user/c"]);
    routes.replace(&c, Vec::new(), &mut table).unwrap();
    assert!(table.snapshot("d").is_empty());
    assert!(routes.active.is_empty());
}

#[test]
fn physical_replacement_cannot_retain_old_keys_or_accept_obsolete_source() {
    let (old, _old_rx) = sender("b");
    let (new, _new_rx) = sender("b");
    assert_ne!(old.dispatch_key(), new.dispatch_key());
    let mut routes = Routes::default();
    let mut table = Receptionist::default();
    routes
        .replace(
            &old,
            vec![view(&["d", "b"], "edge/old", &["old"])],
            &mut table,
        )
        .unwrap();
    routes
        .replace(
            &new,
            vec![view(&["d", "b"], "edge/new", &["new"])],
            &mut table,
        )
        .unwrap();
    assert_eq!(routes.sources.len(), 1);
    assert!(routes.sources.contains_key(&new.dispatch_key()));
    assert_eq!(paths(&table, "d"), ["parrot://d/user/new"]);
    prune(&mut routes, &[&old], &mut table);
    assert!(routes.sources.is_empty());
    assert!(table.snapshot("d").is_empty());
}

#[test]
fn cohort_budget_rejection_preserves_all_owners_and_cached_source() {
    let (b, _rx) = sender("b");
    let mut routes = Routes::default();
    let mut table = Receptionist::with_registration_limits(RegistrationLimits {
        max_registrations: 3,
        max_reserved_bytes: usize::MAX,
    });
    table
        .try_register("local", &key("edge/local"), "/local")
        .unwrap();
    routes
        .replace(
            &b,
            vec![
                view(&["d", "b"], "edge/one", &["old"]),
                view(&["x", "b"], "edge/one", &["old"]),
            ],
            &mut table,
        )
        .unwrap();
    let before_d = table.snapshot("d");
    let before_x = table.snapshot("x");
    assert!(routes
        .replace(
            &b,
            vec![
                view(&["d", "b"], "edge/one", &["new"]),
                view(&["x", "b"], "edge/one", &["new", "excess"])
            ],
            &mut table
        )
        .is_err());
    assert_eq!(table.snapshot("d"), before_d);
    assert_eq!(table.snapshot("x"), before_x);
    assert_eq!(table.snapshot("local").len(), 1);
    assert!(prune(&mut routes, &[&b], &mut table).is_empty());
    assert_eq!(
        table.snapshot("d"),
        before_d,
        "rejected source must not reappear on the next selection"
    );
}

#[test]
fn fallback_over_capacity_withdraws_dead_route_then_retries_without_losing_unrelated() {
    let (b, _b_rx) = sender("b");
    let (c, _c_rx) = sender("c");
    let mut routes = Routes::default();
    let mut table = Receptionist::with_registration_limits(RegistrationLimits {
        max_registrations: 3,
        max_reserved_bytes: usize::MAX,
    });
    table
        .try_register("local", &key("edge/local"), "/local")
        .unwrap();
    routes
        .replace(
            &b,
            vec![
                view(&["d", "b"], "edge/one", &["short"]),
                view(&["x", "b"], "edge/other", &["other"]),
            ],
            &mut table,
        )
        .unwrap();
    routes
        .replace(
            &c,
            vec![
                view(&["d", "c"], "edge/one", &["long-1", "long-2"]),
                view(&["x", "c"], "edge/other", &["other"]),
            ],
            &mut table,
        )
        .unwrap();
    let events = prune(&mut routes, &[&c], &mut table);
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], ReceptionistEvent::Unregistered { .. }));
    assert!(table.snapshot("d").is_empty());
    assert_eq!(table.snapshot("x").len(), 1);
    assert_eq!(table.snapshot("local").len(), 1);
    table.deregister(&key("edge/local"), "/local");
    prune(&mut routes, &[&c], &mut table);
    assert_eq!(
        paths(&table, "d"),
        ["parrot://d/user/long-1", "parrot://d/user/long-2"]
    );
    assert_eq!(table.snapshot("x").len(), 1);
}

#[test]
fn dead_intermediate_retires_only_paths_containing_it() {
    let (b, _rx) = sender("b");
    let mut routes = Routes::default();
    let mut table = Receptionist::default();
    routes
        .replace(
            &b,
            vec![
                view(&["d", "c", "b"], "edge/one", &["old"]),
                view(&["x", "b"], "edge/other", &["kept"]),
            ],
            &mut table,
        )
        .unwrap();
    routes.prune(
        &[b.dispatch_key()].into(),
        &["d", "b", "x"].into_iter().map(String::from).collect(),
        &mut table,
    );
    assert!(table.snapshot("d").is_empty());
    assert_eq!(paths(&table, "x"), ["parrot://x/user/kept"]);
}

#[test]
fn source_cache_does_not_retain_writer_channel() {
    let (back, mut rx) = sender("b");
    let mut routes = Routes::default();
    routes
        .replace(
            &back,
            vec![view(&["d", "b"], "edge/one", &["one"])],
            &mut Receptionist::default(),
        )
        .unwrap();
    drop(back);
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    assert_eq!(
        routes.sources.len(),
        1,
        "identity remains cached after IO owner retirement"
    );
}

#[test]
fn split_horizon_and_hop_limit_prevent_feedback() {
    let (back, _rx) = sender("b");
    let mut routes = Routes::default();
    let mut table = Receptionist::default();
    let mut long = view(&["d", "b"], "edge/long", &["one"]);
    long.path = (0..MAX_HOPS).map(|i| format!("hop-{i}")).collect();
    long.path[0] = "d".into();
    routes
        .replace(
            &back,
            vec![view(&["d", "b"], "edge/one", &["one"]), long],
            &mut table,
        )
        .unwrap();
    let local = || ReceptionistSync {
        from: "local".into(),
        entries: view(&["local"], "edge/local", &["own"]).entries,
    };
    let split = routes.snapshot(local(), "b");
    assert_eq!(split.views.len(), 1);
    assert_eq!(split.views[0].owner(), "local");
    let other = routes.snapshot(local(), "x");
    assert_eq!(other.views.len(), 2);
    assert!(other
        .views
        .iter()
        .any(|view| view.path == ["d", "b", "local"]));
}

#[test]
fn source_count_and_byte_limits_reject_atomically_and_cleanup_releases_capacity() {
    let mut routes = Routes::default();
    let mut table = Receptionist::default();
    let senders: Vec<_> = (0..MAX_SOURCES + 1)
        .map(|i| sender(&format!("source-{i}")))
        .collect();
    for (back, _) in &senders[..MAX_SOURCES] {
        routes.replace(back, Vec::new(), &mut table).unwrap();
    }
    assert!(routes
        .replace(&senders[MAX_SOURCES].0, Vec::new(), &mut table)
        .is_err());
    assert_eq!(routes.sources.len(), MAX_SOURCES);
    routes.prune(&HashSet::new(), &HashSet::new(), &mut table);
    assert!(routes.sources.is_empty());
    let entry = view(&["d", "b"], "edge/one", &["one"]);
    let mut large = entry.clone();
    large.entries = vec![
        entry.entries[0].clone();
        (MAX_SOURCE_BYTES / registration_bytes("d", &key("edge/one"), "parrot://d/user/one"))
            as usize
            + 1
    ];
    assert!(routes
        .replace(&senders[0].0, vec![large], &mut table)
        .is_err());
    assert!(routes.sources.is_empty());
    routes
        .replace(&senders[0].0, vec![entry], &mut table)
        .unwrap();
}

#[test]
fn exact_origin_limit_is_accepted_and_one_extra_is_atomic() {
    let (back, _rx) = sender("b");
    let mut routes = Routes::default();
    let mut table = Receptionist::default();
    let mut views: Vec<_> = (0..MAX_ORIGINS)
        .map(|i| view(&[&format!("owner-{i}"), "b"], "edge/one", &["one"]))
        .collect();
    routes.replace(&back, views.clone(), &mut table).unwrap();
    assert_eq!(routes.active.len(), MAX_ORIGINS);
    let local = ReceptionistSync {
        from: "local".into(),
        entries: view(&["local"], "edge/local", &["own"]).entries,
    };
    let outgoing = routes.snapshot(local, "x");
    assert_eq!(
        outgoing
            .views
            .iter()
            .map(Advertisement::owner)
            .collect::<HashSet<_>>()
            .len(),
        MAX_ORIGINS
    );
    assert_eq!(outgoing.views[0].owner(), "local");
    views.push(view(&["extra", "b"], "edge/one", &["one"]));
    assert!(routes.replace(&back, views, &mut table).is_err());
    assert_eq!(routes.active.len(), MAX_ORIGINS);
    assert!(table.snapshot("extra").is_empty());
}

#[test]
fn deterministic_ring_converges_to_empty_after_origin_withdrawal() {
    let ids = ["a", "b", "c"];
    let mut tables = [
        Receptionist::default(),
        Receptionist::default(),
        Receptionist::default(),
    ];
    let mut routes = [Routes::default(), Routes::default(), Routes::default()];
    let links: Vec<_> = ids.iter().map(|id| sender(id)).collect();
    let own = view(&["a"], "edge/one", &["one"]).entries[0].clone();
    tables[0]
        .try_register("a", &own.key, &own.remote_path)
        .unwrap();
    for round in 0..12 {
        if round == 4 {
            tables[0].deregister(&own.key, &own.remote_path);
        }
        let snapshots: Vec<_> = (0..3)
            .map(|i| {
                routes[i].snapshot(
                    ReceptionistSync {
                        from: ids[i].into(),
                        entries: tables[i].snapshot(ids[i]),
                    },
                    ids[(i + 1) % 3],
                )
            })
            .collect();
        for i in 0..3 {
            let destination = (i + 1) % 3;
            routes[destination]
                .replace(
                    &links[i].0,
                    snapshots[i].views.clone(),
                    &mut tables[destination],
                )
                .unwrap();
        }
        if round == 3 {
            assert!(tables.iter().all(|table| table.snapshot("a").len() == 1));
        }
        if round >= 7 {
            assert!(tables.iter().all(|table| table.snapshot("a").is_empty()));
            assert!(routes.iter().all(|routes| routes.active.is_empty()));
        }
    }
}

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
fn system(owner: AclRules, relay: AclRules) -> Arc<RemoteActorSystem> {
    RemoteActorSystem::new(
        RemoteConfig::tcp("local", None)
            .with_receptionist_acl(owner)
            .with_receptionist_relay_acl(relay),
        Arc::new(Empty),
    )
    .unwrap()
}
fn grant(node: &str, prefix: &str) -> AclRules {
    AclRules {
        default_deny: true,
        roles: [(node.into(), vec![prefix.into()])].into(),
    }
}

#[test]
fn foreign_projection_requires_both_owner_and_immediate_relay_grants() {
    let (back, _rx) = sender("b");
    for (owner, relay, expected) in [
        (AclRules::default(), grant("b", "edge"), 0),
        (grant("d", "edge"), AclRules::default(), 0),
        (grant("d", "edge"), grant("b", "edge/one"), 1),
        (grant("d", "edge-admin"), grant("b", "edge"), 0),
    ] {
        let system = system(owner, relay);
        let result = super::super::project_views(
            &system,
            vec![view(&["d", "b"], "edge/one", &["one"])],
            &back,
            true,
        )
        .unwrap();
        assert_eq!(result.len(), expected);
    }
}

#[test]
fn denied_key_cannot_suppress_another_relays_authorized_key() {
    let (b, _b_rx) = sender("b");
    let (c, _c_rx) = sender("c");
    let system = system(
        AclRules::permissive(),
        AclRules {
            default_deny: true,
            roles: [
                ("b".into(), vec!["edge/one".into()]),
                ("c".into(), vec!["edge/two".into()]),
            ]
            .into(),
        },
    );
    let mut routes = Routes::default();
    let mut table = Receptionist::default();
    for back in [&c, &b] {
        let views = vec![
            view(&["d", back.node_id()], "edge/one", &["one"]),
            view(&["d", back.node_id()], "edge/two", &["two"]),
        ];
        let projected = super::super::project_views(&system, views, back, true).unwrap();
        assert_eq!(projected.len(), 1);
        routes.replace(back, projected, &mut table).unwrap();
    }
    assert_eq!(
        paths(&table, "d"),
        ["parrot://d/user/one", "parrot://d/user/two"]
    );
}

#[test]
fn projection_rejects_cycles_wrong_peer_hop_overflow_and_forged_owner() {
    let (back, _rx) = sender("b");
    let system = system(AclRules::permissive(), AclRules::permissive());
    let mut invalid = vec![
        view(&["d", "b", "d", "b"], "edge/one", &["one"]),
        view(&["d", "local", "b"], "edge/one", &["one"]),
        view(&["d", "c"], "edge/one", &["one"]),
    ];
    let mut long = view(&["d", "b"], "edge/one", &["one"]);
    long.path = (0..MAX_HOPS)
        .map(|i| format!("hop-{i}"))
        .chain(std::iter::once("b".into()))
        .collect();
    invalid.push(long);
    let mut forged = view(&["d", "b"], "edge/one", &["one"]);
    forged.entries[0].node = "x".into();
    invalid.push(forged);
    let mut forged_path = view(&["d", "b"], "edge/one", &["one"]);
    forged_path.entries[0].remote_path = "parrot://x/user/one".into();
    invalid.push(forged_path);
    for view in invalid {
        assert!(super::super::project_views(&system, vec![view], &back, true).is_err());
    }
}

#[test]
fn projection_deduplicates_entries_but_rejects_duplicate_owner_key_views() {
    let (back, _rx) = sender("b");
    let allowed = system(AclRules::permissive(), AclRules::permissive());
    let duplicate = view(&["d", "b"], "edge/one", &["one", "one"]);
    let projected =
        super::super::project_views(&allowed, vec![duplicate.clone()], &back, true).unwrap();
    assert_eq!(projected[0].entries.len(), 1);
    assert!(
        super::super::project_views(&allowed, vec![duplicate.clone(), duplicate], &back, true)
            .is_err()
    );
    let denied = system(AclRules::default(), AclRules::permissive());
    assert!(super::super::project_views(
        &denied,
        vec![view(&["b"], "edge/one", &["one"])],
        &back,
        false
    )
    .is_err());
}

async fn memory_node(id: &str) -> Arc<RemoteActorSystem> {
    let mut config = RemoteConfig::mem(id)
        .with_insecure_development()
        .with_receptionist_acl(AclRules::permissive())
        .with_receptionist_relay_acl(AclRules::permissive());
    config.swim.gossip_interval = std::time::Duration::from_secs(3600);
    config.swim.probe_interval = std::time::Duration::from_secs(3600);
    let system = RemoteActorSystem::new(config, Arc::new(Empty)).unwrap();
    system.start().await.unwrap();
    system
}

async fn memory_pair(a: &Arc<RemoteActorSystem>, b: &Arc<RemoteActorSystem>) {
    a.connect_mem_pair(b).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !a
            .membership
            .lock()
            .await
            .members
            .contains_key(&b.config.node_id)
            || !b
                .membership
                .lock()
                .await
                .members
                .contains_key(&a.config.node_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("link membership publication must complete before injecting snapshots");
}

#[tokio::test]
async fn incoming_publication_projects_route_without_waiting_for_periodic_tick() {
    use parrot_api::receptionist::ReceptionistGateway;
    let a = memory_node("routes-publication-a").await;
    let b = memory_node("routes-publication-b").await;
    memory_pair(&a, &b).await;
    let back = a.direct_sender_of("routes-publication-b").unwrap();
    let mut member = b.membership.lock().await.members["routes-publication-b"].clone();
    member.node_id = "d".into();
    member.addr.node_id = "d".into();
    a.membership.lock().await.members.insert("d".into(), member);
    let mut stream = a
        .receptionist_gateway()
        .subscribe(key("edge/one"))
        .await
        .unwrap();
    // Remove all projections after the stream is ready. No task is yielded
    // between this reset and the uncontended receive path under test.
    a.nodes.reconcile_membership(Vec::new());
    assert!(a.nodes.discovered("d").is_none());
    super::super::receive_views(
        Arc::downgrade(&a),
        vec![view(&["d", "routes-publication-b"], "edge/one", &["one"])],
        &back,
        true,
    )
    .await;
    assert!(
        a.nodes.discovered("d").is_some(),
        "publication must precede notification visibility"
    );
    assert_eq!(
        stream.try_recv().unwrap(),
        ReceptionistEvent::Registered {
            key: key("edge/one"),
            remote_path: "parrot://d/user/one".into()
        }
    );
    a.remote_ref("parrot://d/user/one").unwrap();
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn queued_snapshot_from_superseded_physical_connection_cannot_replace_current_view() {
    use parrot_api::receptionist::ReceptionistGateway;
    let a = memory_node("routes-replacement-a").await;
    let b = memory_node("routes-replacement-b").await;
    memory_pair(&a, &b).await;
    let old = a.direct_sender_of("routes-replacement-b").unwrap();
    memory_pair(&a, &b).await;
    let current = a.direct_sender_of("routes-replacement-b").unwrap();
    assert!(!current.same_connection(&old));
    let mut stream = a
        .receptionist_gateway()
        .subscribe(key("edge/one"))
        .await
        .unwrap();
    super::super::receive_views(
        Arc::downgrade(&a),
        vec![view(&["routes-replacement-b"], "edge/one", &["fresh"])],
        &current,
        true,
    )
    .await;
    assert_eq!(
        stream.try_recv().unwrap(),
        ReceptionistEvent::Registered {
            key: key("edge/one"),
            remote_path: "parrot://routes-replacement-b/user/fresh".into()
        }
    );
    super::super::receive_views(
        Arc::downgrade(&a),
        vec![view(&["routes-replacement-b"], "edge/one", &["obsolete"])],
        &old,
        true,
    )
    .await;
    assert_eq!(stream.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert_eq!(
        paths(&a.receptionist.lock().unwrap(), "routes-replacement-b"),
        ["parrot://routes-replacement-b/user/fresh"]
    );
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

fn segment(id: u64, total: usize, offset: usize, bytes: &[u8]) -> Vec<u8> {
    [
        id.to_le_bytes().as_slice(),
        (total as u32).to_le_bytes().as_slice(),
        (offset as u32).to_le_bytes().as_slice(),
        bytes,
    ]
    .concat()
}

#[tokio::test]
async fn routed_segments_publish_and_withdraw_only_complete_cohorts() {
    use parrot_api::receptionist::ReceptionistGateway;
    let a = memory_node("routes-segments-a").await;
    let b = memory_node("routes-segments-b").await;
    memory_pair(&a, &b).await;
    let back = a.direct_sender_of("routes-segments-b").unwrap();
    let mut stream = a
        .receptionist_gateway()
        .subscribe(key("edge/one"))
        .await
        .unwrap();
    let suffixes: Vec<_> = (0..64).map(|i| format!("bulk-{i}")).collect();
    let advertisement = view(
        &["routes-segments-b"],
        "edge/one",
        &suffixes.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let payload = encode(&Sync {
        from: "routes-segments-b".into(),
        views: vec![advertisement],
    });
    let body = &payload[1..];
    let split = body.len() / 2;
    super::super::receive_segment(
        Arc::downgrade(&a),
        &segment(1, body.len(), 0, &body[..split]),
        &back,
        true,
    )
    .await;
    assert_eq!(stream.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert!(a
        .receptionist
        .lock()
        .unwrap()
        .snapshot("routes-segments-b")
        .is_empty());
    super::super::receive_segment(
        Arc::downgrade(&a),
        &segment(1, body.len(), split, &body[split..]),
        &back,
        true,
    )
    .await;
    assert!(matches!(
        stream.try_recv(),
        Ok(ReceptionistEvent::Registered { .. })
    ));
    assert_eq!(
        a.receptionist
            .lock()
            .unwrap()
            .snapshot("routes-segments-b")
            .len(),
        64
    );
    for _ in 1..64 {
        assert!(matches!(
            stream.try_recv(),
            Ok(ReceptionistEvent::Registered { .. })
        ));
    }
    let payload = encode(&Sync {
        from: "routes-segments-b".into(),
        views: Vec::new(),
    });
    let body = &payload[1..];
    super::super::receive_segment(
        Arc::downgrade(&a),
        &segment(2, body.len(), 0, &body[..1]),
        &back,
        true,
    )
    .await;
    assert_eq!(
        a.receptionist
            .lock()
            .unwrap()
            .snapshot("routes-segments-b")
            .len(),
        64
    );
    assert_eq!(stream.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    super::super::receive_segment(
        Arc::downgrade(&a),
        &segment(2, body.len(), 1, &body[1..]),
        &back,
        true,
    )
    .await;
    assert!(a
        .receptionist
        .lock()
        .unwrap()
        .snapshot("routes-segments-b")
        .is_empty());
    for _ in 0..64 {
        assert!(matches!(
            stream.try_recv(),
            Ok(ReceptionistEvent::Unregistered { .. })
        ));
    }
    assert_eq!(stream.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[test]
fn assembly_rejects_protocol_switch_inside_one_snapshot() {
    use super::super::segments::{Assemblies, RetiredAssemblies};
    let (back, _rx) = sender("b");
    for initial in [true, false] {
        let mut assemblies = Assemblies::default();
        let mut retired = RetiredAssemblies::default();
        assert!(assemblies
            .push_deferred(
                &back,
                &segment(1, 2, 0, &[7]),
                std::time::Instant::now(),
                initial,
                &mut retired
            )
            .unwrap()
            .is_none());
        assert!(assemblies
            .push_deferred(
                &back,
                &segment(1, 2, 1, &[8]),
                std::time::Instant::now(),
                !initial,
                &mut retired
            )
            .is_err());
    }
}

fn route_members() -> crate::swim::Membership {
    let mut members = crate::swim::Membership::new(0);
    members.members.insert(
        "d".into(),
        crate::swim::Member {
            node_id: "d".into(),
            addr: crate::swim::NodeAddrWire {
                node_id: "d".into(),
                scheme: "tcp".into(),
                host: "127.0.0.1".into(),
                port: 9000,
            },
            status: crate::swim::MemberStatus::Alive,
            incarnation: 1,
            status_until_ms: 0,
            metadata: bytes::Bytes::new(),
        },
    );
    members
}

#[test]
fn membership_projection_cannot_repopulate_routes_after_system_close() {
    let system = system(AclRules::permissive(), AclRules::permissive());
    let members = route_members();
    let seed = crate::NodeAddr::tcp("seed", "127.0.0.1:9001".parse().unwrap());
    system.nodes.add_seed(seed.clone());
    {
        let _gate = system.link_gate.lock().unwrap();
        system.reconcile_membership_routes(&members);
    }
    assert!(system.nodes.discovered("d").is_some());
    system.begin_close();
    assert!(system.nodes.discovered("d").is_none());
    assert_eq!(
        system.nodes.get("seed").unwrap().0,
        seed,
        "closing retires discoveries without deleting configured seeds"
    );
    // Model a projector resuming with its previously accepted member view.
    {
        let _gate = system.link_gate.lock().unwrap();
        system.reconcile_membership_routes(&members);
    }
    assert!(
        system.nodes.discovered("d").is_none(),
        "a late projection must obey the permanent close fence"
    );
}

#[test]
fn closed_system_rejects_late_membership_projection() {
    let system = system(AclRules::permissive(), AclRules::permissive());
    system.begin_close();
    assert!(system.nodes.discovered("d").is_none());
    let members = route_members();
    {
        let _gate = system.link_gate.lock().unwrap();
        system.reconcile_membership_routes(&members);
    }
    assert!(
        system.nodes.discovered("d").is_none(),
        "late projection cannot create a new route in a closed system"
    );
}
