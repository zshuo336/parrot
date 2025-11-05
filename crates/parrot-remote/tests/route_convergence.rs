use parrot_remote::roles::route_reflector::RouteReflector;
use parrot_remote::topology::{route_digest, RouteEntry, RouteGossip, RouteTable};

fn route(prefix: &str, hop: &str, cost: u32, version: u64) -> RouteEntry {
    RouteEntry {
        prefix: prefix.into(),
        next_hop: hop.into(),
        cost,
        version,
    }
}

fn gossip(entries: Vec<RouteEntry>) -> RouteGossip {
    RouteGossip {
        digest: route_digest(&entries),
        entries,
    }
}

#[test]
fn digest_distinguishes_cost() {
    let a = route("parrot://node/user/", "hub", 1, 8);
    for cost in [0, 2, u32::MAX] {
        let b = RouteEntry { cost, ..a.clone() };
        assert_ne!(
            route_digest(std::slice::from_ref(&a)),
            route_digest(&[b]),
            "cost omitted"
        );
    }
}

#[test]
fn digest_preserves_field_boundaries() {
    let a = route("parrot://node/user/a", "bc", 1, 8);
    let b = route("parrot://node/user/ab", "c", 1, 8);
    assert_ne!(
        route_digest(&[a]),
        route_digest(&[b]),
        "field boundaries omitted"
    );
}

#[test]
fn digest_is_order_independent_and_covers_each_field() {
    let base = route("parrot://node/user/", "hub", 1, 8);
    let other = route("parrot://other/", "edge", 2, 9);
    let digest = route_digest(&[base.clone(), other.clone()]);
    assert_eq!(digest, route_digest(&[other.clone(), base.clone()]));
    for changed in [
        RouteEntry {
            prefix: "parrot://new/".into(),
            ..base.clone()
        },
        RouteEntry {
            next_hop: "new-hop".into(),
            ..base.clone()
        },
        RouteEntry {
            version: u64::MAX,
            ..base.clone()
        },
        RouteEntry {
            cost: u32::MAX,
            ..base.clone()
        },
    ] {
        assert_ne!(digest, route_digest(&[changed, other.clone()]));
    }
    assert_eq!(route_digest(&[]), 0);
}

fn permutations(mut routes: Vec<RouteEntry>, mut visit: impl FnMut(&[RouteEntry])) {
    fn walk(routes: &mut [RouteEntry], at: usize, visit: &mut impl FnMut(&[RouteEntry])) {
        if at == routes.len() {
            visit(routes);
            return;
        }
        for index in at..routes.len() {
            routes.swap(at, index);
            walk(routes, at + 1, visit);
            routes.swap(at, index);
        }
    }
    walk(&mut routes, 0, &mut visit);
}

#[test]
fn table_converges_for_all_arrival_orders_and_replays() {
    let winner = route("parrot://node/", "a", 3, 8);
    let mut visited = 0;
    permutations(
        vec![
            winner.clone(),
            route("parrot://node/", "z", 3, 8),
            route("parrot://node/", "b", 3, 8),
            route("parrot://node/", "0", 4, 8),
            route("parrot://node/", "0", 0, 7),
            route("parrot://node/", "a", u32::MAX, 0),
        ],
        |arrivals| {
            let mut table = RouteTable::new();
            for entry in arrivals {
                table.merge(&gossip(vec![entry.clone()]));
            }
            assert_eq!(
                table.resolve("parrot://node/user/service"),
                Some(&winner),
                "arrival dependent"
            );
            assert_eq!(
                table.merge(&gossip(arrivals.to_vec())),
                0,
                "replay changed winner"
            );
            assert_eq!(table.digest(), route_digest(std::slice::from_ref(&winner)));
            visited += 1;
        },
    );
    assert_eq!(visited, 720);
}

#[test]
fn reflector_accepts_same_origin_improvements_and_suppresses_replays() {
    let mut reflector = RouteReflector::new("rr", vec!["source".into(), "peer".into()]);
    let routes = [
        route("parrot://node/", "z", 8, 5),
        route("parrot://node/", "z", 3, 5),
        route("parrot://node/", "a", 3, 5),
        route("parrot://node/", "z", 9, 6),
    ];
    for entry in &routes {
        let update = gossip(vec![entry.clone()]);
        assert_eq!(
            reflector.reflect_from("source", &update, "same-origin"),
            vec![("peer".into(), vec![entry.clone()])],
            "same-origin improvement lost"
        );
        assert_eq!(reflector.table.get(&entry.prefix), Some(entry));
        assert!(reflector
            .reflect_from("peer", &update, "same-origin")
            .is_empty());
        assert!(reflector
            .reflect_from("peer", &update, "different-origin")
            .is_empty());
    }
    assert_eq!(
        reflector.digest(),
        route_digest(std::slice::from_ref(&routes[3]))
    );
}

#[test]
fn reflector_and_table_choose_identical_winners_for_all_arrival_orders() {
    let winner = route("parrot://node/", "a", 3, 8);
    let mut visited = 0;
    permutations(
        vec![
            winner.clone(),
            route("parrot://node/", "z", 3, 8),
            route("parrot://node/", "b", 3, 8),
            route("parrot://node/", "0", 4, 8),
            route("parrot://node/", "0", 0, 7),
            route("parrot://node/", "0", u32::MAX, 0),
        ],
        |arrivals| {
            let mut reflector = RouteReflector::new("rr", vec!["source".into(), "peer".into()]);
            for (index, entry) in arrivals.iter().enumerate() {
                reflector.reflect_from(
                    "source",
                    &gossip(vec![entry.clone()]),
                    &format!("origin-{index}"),
                );
            }
            assert_eq!(
                reflector.table.get(&winner.prefix),
                Some(&winner),
                "reflector arrival dependent"
            );
            assert!(reflector
                .reflect_from("source", &gossip(arrivals.to_vec()), "replay")
                .is_empty());
            visited += 1;
        },
    );
    assert_eq!(visited, 720);
}
