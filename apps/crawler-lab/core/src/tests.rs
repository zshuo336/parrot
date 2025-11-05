use super::*;

fn request(seq: u64, ids: &[u64]) -> Vec<u8> {
    encode_request(
        seq,
        &ids.iter()
            .map(|id| (*id, url_of(*id), 0))
            .collect::<Vec<_>>(),
    )
    .unwrap()
}
fn progress(hub: &mut Hub) -> (u64, u64) {
    let bytes = hub.handle("crawl/Progress", &[], MAX_BYTES).unwrap();
    (
        u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        u64::from_le_bytes(bytes[8..].try_into().unwrap()),
    )
}

#[test]
fn fixed_site_vector_and_tokenizer_semantics() {
    // Fixed vector calculated from the 64-bit xorshift specification, independent
    // of the encoder. This must also pass in the real 32-bit Wasm guest.
    assert_eq!(synth_page(0), b"<html><head><title>parrot page</title></head><body>mtls inverted router raft receptionist throughput frontier index index relay inverted topology gateway actor swim supervise gateway mtls throughput posting swim gateway twin wire crawler durable index federate parrot gateway wire relay gossip shard mailbox receptionist throughput parrot throughput mtls receptionist relay search gossip swim supervise digest query query shard cluster supervise crawler shard federate durable gossip search federate gossip twin shard search border supervise </body></html>");
    assert_eq!(outlinks(0, 3, 500), [165, 17, 103]);
    assert_eq!(
        tokenize(b"<HTML>A, THE; Parrot parrot ACTOR 12 x \xff</HTML>"),
        ["parrot", "parrot", "actor", "12"]
    );
    assert!(outlinks(0, 3, 0).is_empty());
}

#[test]
fn tick_has_real_pages_frequencies_and_links() {
    let mut hub = Hub::new(Config::default()).unwrap();
    let reply = hub
        .handle("crawl/Tick", &request(1, &[0, 0]), MAX_BYTES)
        .unwrap();
    let tick = Tick::decode(&reply).unwrap();
    assert_eq!((tick.sequence, tick.fetched, tick.pages.len()), (1, 1, 1));
    assert_eq!(tick.pages[0].0, 0);
    assert_eq!(
        tick.terms.iter().find(|x| x.0 == "parrot"),
        Some(&("parrot".into(), 0, 3))
    );
    assert_eq!(
        tick.terms.iter().find(|x| x.0 == "gateway"),
        Some(&("gateway".into(), 0, 4))
    );
    assert_eq!(tick.terms.iter().map(|x| x.2).sum::<u32>(), 66);
    assert_eq!(
        tick.outlinks,
        [
            (165, url_of(165), 1),
            (17, url_of(17), 1),
            (103, url_of(103), 1)
        ]
    );
    assert_eq!(tick.encode().unwrap(), reply);
}

#[test]
fn capacity_failure_and_invalid_last_entry_are_atomic() {
    let mut hub = Hub::new(Config::default()).unwrap();
    assert_eq!(
        hub.handle("crawl/Tick", &request(1, &[0]), 1),
        Err(Error::Overflow)
    );
    assert_eq!(progress(&mut hub), (0, 0));
    for bad in [
        (500, url_of(500), 0),
        (2, "wrong".into(), 0),
        (2, url_of(2), 3),
    ] {
        let req = encode_request(1, &[(0, url_of(0), 0), bad]).unwrap();
        assert!(hub.handle("crawl/Tick", &req, MAX_BYTES).is_err());
        assert_eq!(progress(&mut hub), (0, 0));
    }
    let tick = Tick::decode(
        &hub.handle("crawl/Tick", &request(1, &[0]), MAX_BYTES)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(tick.fetched, 1);
}

#[test]
fn replay_is_exact_and_instances_are_isolated() {
    let mut first = Hub::new(Config::default()).unwrap();
    let mut second = Hub::new(Config::default()).unwrap();
    let req = request(1, &[0]);
    let reply = first.handle("crawl/Tick", &req, MAX_BYTES).unwrap();
    assert_eq!(
        first.handle("crawl/Tick", &req, reply.len() - 1),
        Err(Error::Overflow)
    );
    assert_eq!(
        first.handle("crawl/Tick", &req, reply.len()).unwrap(),
        reply
    );
    assert_eq!(
        first.handle("crawl/Stat", &[], 8).unwrap(),
        2u64.to_le_bytes()
    );
    for req in [request(0, &[0]), request(3, &[1]), request(1, &[1])] {
        assert!(first.handle("crawl/Tick", &req, MAX_BYTES).is_err());
    }
    assert_eq!(progress(&mut second), (0, 0));
    assert_eq!(
        second
            .handle("crawl/Tick", &request(1, &[0]), MAX_BYTES)
            .unwrap(),
        reply
    );
    let next = first
        .handle("crawl/Tick", &request(2, &[0, 1]), MAX_BYTES)
        .unwrap();
    assert_eq!(
        Tick::decode(&next)
            .unwrap()
            .pages
            .iter()
            .map(|p| p.0)
            .collect::<Vec<_>>(),
        [1]
    );
    assert!(first.handle("crawl/Tick", &req, MAX_BYTES).is_err());
}

#[test]
fn malformed_requests_and_replies_are_rejected() {
    let req = request(1, &[0]);
    for end in 0..req.len() {
        let mut hub = Hub::new(Config::default()).unwrap();
        assert!(
            hub.handle("crawl/Tick", &req[..end], MAX_BYTES).is_err(),
            "cut {end}"
        );
        assert_eq!(progress(&mut hub), (0, 0));
    }
    let mut bad = req.clone();
    bad.push(0);
    let mut count = req.clone();
    count[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut utf8 = req.clone();
    utf8[24] = 255;
    let mut hub = Hub::new(Config::default()).unwrap();
    for req in [bad, count, utf8] {
        assert!(hub.handle("crawl/Tick", &req, MAX_BYTES).is_err());
    }
    let reply = hub.handle("crawl/Tick", &req, MAX_BYTES).unwrap();
    for end in 0..reply.len() {
        assert!(Tick::decode(&reply[..end]).is_err(), "cut {end}");
    }
    let mut extra = reply.clone();
    extra.push(0);
    assert!(Tick::decode(&extra).is_err());
    let mut bad_magic = reply;
    bad_magic[0] = 0;
    assert!(Tick::decode(&bad_magic).is_err());
}

#[test]
fn empty_and_maximum_workloads_respect_bounds() {
    let mut hub = Hub::new(Config {
        pages: 0,
        depth: 0,
        fanout: 0,
        batch: 1,
    })
    .unwrap();
    let tick = Tick::decode(
        &hub.handle("crawl/Tick", &request(1, &[]), MAX_BYTES)
            .unwrap(),
    )
    .unwrap();
    assert!(tick.pages.is_empty() && tick.terms.is_empty() && tick.outlinks.is_empty());
    assert!(hub
        .handle("crawl/Tick", &request(2, &[0]), MAX_BYTES)
        .is_err());
    let mut hub = Hub::new(Config {
        pages: 1_000_000,
        depth: u16::MAX,
        fanout: 256,
        batch: 256,
    })
    .unwrap();
    let tick = Tick::decode(
        &hub.handle(
            "crawl/Tick",
            &request(1, &(0..256).collect::<Vec<_>>()),
            MAX_BYTES,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(tick.pages.len(), 256);
    assert_eq!(tick.fetched, 256);
    assert!(!tick.outlinks.is_empty());
    let mut terminal = Hub::new(Config {
        depth: 0,
        ..Config::default()
    })
    .unwrap();
    assert!(Tick::decode(
        &terminal
            .handle("crawl/Tick", &request(1, &[0]), MAX_BYTES)
            .unwrap()
    )
    .unwrap()
    .outlinks
    .is_empty());
}

#[test]
fn drain_and_counter_operations_validate_before_commit() {
    let mut hub = Hub::new(Config::default()).unwrap();
    for (key, payload) in [
        ("unknown", &[][..]),
        ("crawl/Stat", &[1][..]),
        ("crawl/Bump", &[][..]),
    ] {
        assert!(hub.handle(key, payload, MAX_BYTES).is_err());
    }
    assert_eq!(hub.handle("crawl/Echo", b"abc", 2), Err(Error::Overflow));
    hub.handle("crawl/Bump", &[5], 0).unwrap();
    assert_eq!(
        hub.handle("crawl/Stat", &[], 8).unwrap(),
        6u64.to_le_bytes()
    );
    hub.drain();
    hub.drain();
    for key in ["crawl/Echo", "crawl/Tick", "crawl/Bump"] {
        assert!(hub.handle(key, &[], MAX_BYTES).is_err());
    }
    assert_eq!(progress(&mut hub), (0, 0));
}

#[test]
fn config_and_encoded_sizes_are_strict() {
    for raw in [
        r#"{}"#,
        r#"{"pages":0,"depth":0,"fanout":0,"batch":0}"#,
        r#"{"pages":0,"depth":0,"fanout":0,"batch":1,"extra":1}"#,
    ] {
        assert!(Config::from_json(raw.as_bytes()).is_err());
    }
    let entry = (0, "x".repeat(MAX_BYTES - 18), 0);
    assert_eq!(
        encode_frontier(std::slice::from_ref(&entry)).unwrap().len(),
        MAX_BYTES
    );
    assert_eq!(encode_request(1, &[entry]), Err(Error::Overflow));
    assert!(encode_request(1, &vec![(0, String::new(), 0); 257]).is_err());
    assert!(Tick {
        sequence: 1,
        fetched: 0,
        pages: vec![(0, vec![]); 257],
        terms: vec![],
        outlinks: vec![]
    }
    .encode()
    .is_err());
}
