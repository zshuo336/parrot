use parrot_remote::topology::{RouteEntry, SupernetAggregator, SUPERNET_THRESHOLD};

fn route(prefix: &str, hop: &str, cost: u32, version: u64) -> RouteEntry {
    RouteEntry {
        prefix: prefix.into(),
        next_hop: hop.into(),
        cost,
        version,
    }
}

fn padded(mut routes: Vec<RouteEntry>) -> Vec<RouteEntry> {
    routes.push(route("parrot://padding/filler/", "padding", 1, 1));
    for index in 0..SUPERNET_THRESHOLD {
        routes.push(route(
            &format!("parrot://padding/filler/{index}/"),
            "padding",
            1,
            1,
        ));
    }
    routes
}

fn forwarding(entry: Option<&RouteEntry>) -> Option<(&str, u32, u64)> {
    entry.map(|entry| (entry.next_hop.as_str(), entry.cost, entry.version))
}

#[test]
fn uncovered_child_keeps_existing_parent_fallback() {
    let entries = padded(vec![
        route("parrot://node/", "fallback", 9, 1),
        route("parrot://node/user/a/", "service", 2, 4),
        route("parrot://node/user/b/", "service", 2, 4),
    ]);
    let mut compact = SupernetAggregator::new();
    compact.compact(&entries);
    assert_eq!(
        forwarding(compact.resolve("parrot://node/user/unregistered/x")),
        Some(("fallback", 9, 1)),
        "uncovered prefix gained a route"
    );
    assert_eq!(
        forwarding(compact.resolve("parrot://node/user/a/x")),
        Some(("service", 2, 4))
    );
}

#[test]
fn siblings_keep_their_own_cost_and_version() {
    let entries = padded(vec![
        route("parrot://node/user/a/", "service", 1, 2),
        route("parrot://node/user/b/", "service", 9, 8),
    ]);
    let mut compact = SupernetAggregator::new();
    compact.compact(&entries);
    for (path, cost, version) in [("a", 1, 2), ("b", 9, 8)] {
        assert_eq!(
            forwarding(compact.resolve(&format!("parrot://node/user/{path}/x"))),
            Some(("service", cost, version)),
            "routing metadata changed"
        );
    }
}

#[test]
fn node_root_does_not_advertise_other_nodes() {
    let mut compact = SupernetAggregator::new();
    compact.compact(&padded(vec![route("parrot://node/", "service", 2, 4)]));
    assert!(
        compact.resolve("parrot://unregistered/user/x").is_none(),
        "unrelated node gained a route"
    );
    assert!(compact.resolve("parrot://node-other/user/x").is_none());
    assert_eq!(
        forwarding(compact.resolve("parrot://node/user/x")),
        Some(("service", 2, 4))
    );
}

#[test]
fn nested_exceptions_preserve_longest_prefix_precedence() {
    let mut compact = SupernetAggregator::new();
    compact.compact(&padded(vec![
        route("parrot://node/", "root", 2, 4),
        route("parrot://node/user/", "exception", 3, 5),
        route("parrot://node/user/special/", "root", 2, 4),
    ]));
    for (path, expected) in [
        ("parrot://node/other/x", ("root", 2, 4)),
        ("parrot://node/user/other/x", ("exception", 3, 5)),
        ("parrot://node/user/special/x", ("root", 2, 4)),
    ] {
        assert_eq!(
            forwarding(compact.resolve(path)),
            Some(expected),
            "nested exception changed"
        );
    }
}

#[test]
fn explicit_parent_allows_real_redundancy_compaction() {
    let mut entries = vec![route("parrot://node/", "service", 2, 4)];
    for index in 0..128 {
        entries.push(route(
            &format!("parrot://node/user-{index}/"),
            "service",
            2,
            4,
        ));
    }
    let entries = padded(entries);
    let mut compact = SupernetAggregator::new();
    let report = compact.compact(&entries);
    assert_eq!(report.before, entries.len());
    assert_eq!(report.after, 2, "redundant child was not removed");
    assert_eq!(compact.len(), report.after);
    assert_eq!(
        forwarding(compact.resolve("parrot://node/any/x")),
        Some(("service", 2, 4))
    );
    assert!(compact.resolve("parrot://another-node/x").is_none());
    assert_eq!(compact.compact(&entries), report);
}

#[test]
fn crossing_threshold_and_reset_do_not_change_reachability() {
    let mut entries = (0..SUPERNET_THRESHOLD)
        .map(|index| route(&format!("parrot://node/user/{index}/"), "service", 2, 4))
        .collect::<Vec<_>>();
    let mut compact = SupernetAggregator::new();
    assert_eq!(compact.compact(&entries).after, SUPERNET_THRESHOLD);
    assert!(compact.resolve("parrot://node/user/missing/x").is_none());
    entries.push(route("literal", "other", 1, 1));
    let report = compact.compact(&entries);
    assert!(
        compact.resolve("parrot://node/user/missing/x").is_none(),
        "threshold changed reachability"
    );
    assert_eq!(report.after, entries.len());
    assert_eq!(
        forwarding(compact.resolve("parrot://node/user/42/x")),
        Some(("service", 2, 4))
    );
    let empty = compact.compact(&[]);
    assert_eq!(
        (empty.before, empty.after, empty.ratio_permille),
        (0, 0, 1000)
    );
    assert!(compact.is_empty());
    assert!(compact.resolve("parrot://node/user/42/x").is_none());
}

#[test]
fn same_prefix_candidates_are_compacted_as_one_decision() {
    for reverse in [false, true] {
        let mut candidates = vec![
            route("parrot://node/user/", "service", 1, 1),
            route("parrot://node/user/", "service", 2, 2),
        ];
        if reverse {
            candidates.reverse();
        }
        candidates.push(route("parrot://node/", "service", 1, 1));
        let mut compact = SupernetAggregator::new();
        compact.compact(&padded(candidates));
        for query in ["parrot://node/other/x", "parrot://node/user/x"] {
            assert_eq!(
                forwarding(compact.resolve(query)),
                Some(("service", 1, 1)),
                "same-prefix candidate changed: reverse={reverse}, query={query}"
            );
        }
    }
}

#[test]
fn compacted_routes_match_independent_lookup_for_reordered_inputs() {
    let mut entries = vec![route("parrot://model/", "default", 9, 1)];
    let mut queries = vec!["parrot://missing/x".to_string()];
    for a in ['a', 'b', 'c'] {
        entries.push(route(&format!("parrot://model/{a}/"), "branch", 4, 2));
        for b in ['a', 'b', 'c', 'd'] {
            for c in ['a', 'b', 'c', 'd'] {
                queries.push(format!("parrot://model/{a}/{b}/{c}/x"));
                if b != 'd' && c != 'd' {
                    entries.push(route(
                        &format!("parrot://model/{a}/{b}/{c}/"),
                        if c == 'a' { "branch" } else { "leaf" },
                        if c == 'a' { 4 } else { 1 },
                        if c == 'a' { 2 } else { 8 },
                    ));
                }
            }
        }
    }
    let oracle = entries.clone();
    let mut entries = padded(entries);
    for order in 0..3 {
        if order == 1 {
            entries.reverse();
        }
        if order == 2 {
            entries.rotate_left(7);
        }
        let mut compact = SupernetAggregator::new();
        compact.compact(&entries);
        for query in &queries {
            let expected = oracle
                .iter()
                .filter(|entry| query.starts_with(&entry.prefix))
                .min_by_key(|entry| {
                    (
                        std::cmp::Reverse(entry.prefix.len()),
                        entry.cost,
                        &entry.next_hop,
                    )
                });
            assert_eq!(
                forwarding(compact.resolve(query)),
                forwarding(expected),
                "reference route model disagrees: order={order}, query={query}"
            );
        }
    }
}
