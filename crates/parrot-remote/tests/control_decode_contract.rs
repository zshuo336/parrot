//! Control messages must reject malformed input before state changes or large allocations.
use parrot_remote::frame::Frame;
use parrot_remote::receptionist::{decode_sync, encode_sync, ReceptionistSync};
use parrot_remote::swim::{decode_gossip, encode_gossip, MembershipGossip};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

// Thread-local counters keep unrelated parallel test allocations out of the result.
thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}
struct Meter;
fn record(size: usize) {
    if ENABLED.try_with(Cell::get).unwrap_or(false) {
        let _ = LARGEST.try_with(|n| n.set(n.get().max(size)));
    }
}
// SAFETY: Delegates unchanged pointer/layout contracts to System. Accounting
// does not allocate, dereference allocation pointers, or recurse into System.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(pointer, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;

fn bounded_rejection(run: impl FnOnce() -> bool) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ENABLED.set(false);
        }
    }
    LARGEST.set(0);
    ENABLED.set(true);
    let reset = Reset;
    let rejected = run();
    drop(reset);
    let largest = LARGEST.get();
    assert!(rejected, "invalid input was accepted");
    assert!(
        largest < 16 * 1024,
        "tiny invalid input requested {largest} bytes"
    );
}

fn length(value: u32) -> Vec<u8> {
    let mut out = vec![252]; // bincode 2 standard: u32 marker and LE payload.
    out.extend_from_slice(&value.to_le_bytes());
    out
}

#[test]
fn route_hint_literal_vectors_preserve_fields_and_wire_boundaries() {
    let wire = [1, 0, b'n', 3, 0, b'a', b':', b'1'];
    assert_eq!(
        Frame::parse_route_hint(&wire).unwrap(),
        ("n".into(), "a:1".into())
    );
    assert_eq!(
        Frame::try_route_hint("n", "a:1").unwrap().payload.as_ref(),
        wire
    );
    assert_eq!(
        Frame::parse_route_hint(&[0, 0, 0, 0]).unwrap(),
        (String::new(), String::new())
    );

    let node = "n".repeat(u16::MAX as usize);
    let addr = "a".repeat(u16::MAX as usize);
    let frame = Frame::try_route_hint(&node, &addr).unwrap();
    assert_eq!(
        Frame::parse_route_hint(&frame.payload).unwrap(),
        (node, addr)
    );
    let frame = Frame::try_route_hint("节点", "[::1]:9000").unwrap();
    assert_eq!(
        Frame::parse_route_hint(&frame.payload).unwrap(),
        ("节点".into(), "[::1]:9000".into())
    );
}

#[test]
fn route_hint_rejects_truncation_invalid_utf8_and_every_unconsumed_suffix() {
    let wire = [1, 0, b'n', 3, 0, b'a', b':', b'1'];
    for end in 0..wire.len() {
        assert!(
            Frame::parse_route_hint(&wire[..end]).is_err(),
            "prefix {end}"
        );
    }
    for bad in [
        &[1, 0, 255, 0, 0][..],
        &[0, 0, 1, 0, 255],
        &[255, 255, 0, 0],
        &[0, 0, 255, 255],
    ] {
        assert!(
            Frame::parse_route_hint(bad).is_err(),
            "invalid payload {bad:?}"
        );
    }
    for suffix in [&[0][..], &[0xa5], &[0, 0, 0, 0], &wire] {
        let payload = [wire.as_slice(), suffix].concat();
        bounded_rejection(|| Frame::parse_route_hint(&payload).is_err());
    }
}

#[tokio::test]
async fn real_tcp_rejects_tainted_route_hints_before_cache_mutation() {
    use parrot_api::types::BoxedActorRef;
    use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
    use std::{sync::Arc, time::Duration};
    struct Empty;
    #[async_trait::async_trait]
    impl LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
            None
        }
    }
    // Suppress speculative dialing: this contract observes ingress/cache effects.
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("hint-server", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development()
            .with_topology(parrot_remote::topology::TopologyConfig {
                mode: parrot_remote::topology::TopologyMode::Hub,
                relay_fallback: false,
                ..Default::default()
            }),
        Arc::new(Empty),
    )
    .unwrap();
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("hint-client", None).with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let outcome: Result<bool, String> = async {
        client
            .connect(&NodeAddr::tcp("hint-server", server.local_addr().unwrap()))
            .await
            .map_err(|e| e.to_string())?;
        let sender = client.links_snapshot().await[0].1.clone();
        sender
            .send(Frame::try_route_hint("existing", "127.0.0.1:1").unwrap())
            .await
            .map_err(|e| e.to_string())?;
        for node in ["existing", "tainted"] {
            let mut frame = Frame::try_route_hint(node, "127.0.0.1:2").unwrap();
            frame.payload = [frame.payload.as_ref(), &[0xa5]].concat().into();
            sender.send(frame).await.map_err(|e| e.to_string())?;
        }
        sender
            .send(Frame::try_route_hint("accepted-marker", "[::1]:3").unwrap())
            .await
            .map_err(|e| e.to_string())?;
        // ROUTE_HINT frames share the ordered fast lane. The marker proves that
        // both malformed frames were processed before inspecting their effects.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                {
                    let learned = server.ingress.learned.lock().unwrap();
                    if learned.contains_key("accepted-marker") {
                        return learned.len() == 2
                            && learned.get("existing").map(String::as_str) == Some("127.0.0.1:1")
                            && learned.get("accepted-marker").map(String::as_str)
                                == Some("[::1]:3")
                            && !sender.is_closed();
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|e| e.to_string())
    }
    .await;
    let client_shutdown = client.shutdown().await;
    let server_shutdown = server.shutdown().await;
    client_shutdown.unwrap();
    server_shutdown.unwrap();
    assert!(
        outcome.expect("valid following route hint must still be processed"),
        "a route hint with trailing bytes created or overwrote a cached route"
    );
}

#[test]
fn literal_wire_vectors_preserve_existing_valid_payloads() {
    let gossip = MembershipGossip {
        events: vec![],
        seen_from: "n1".into(),
        digest: 42,
        full_sync: None,
    };
    let sync = ReceptionistSync {
        from: "n1".into(),
        entries: vec![],
    };
    // These literals are independent of the production encoder/decoder pair.
    let gossip_wire = [0x10, 0, 2, b'n', b'1', 42, 0];
    let sync_wire = [0x20, 2, b'n', b'1', 0];
    assert_eq!(encode_gossip(&gossip).as_ref(), gossip_wire);
    assert_eq!(encode_sync(&sync).as_ref(), sync_wire);
    let g = decode_gossip(&gossip_wire[1..]).unwrap();
    assert!(g.events.is_empty());
    assert_eq!(g.seen_from, "n1");
    assert_eq!(g.digest, 42);
    assert!(g.full_sync.is_none());
    let s = decode_sync(&sync_wire[1..]).unwrap();
    assert_eq!(s.from, "n1");
    assert!(s.entries.is_empty());
}

#[test]
fn gossip_rejects_trailing_data() {
    assert!(decode_gossip(&[0, 2, b'n', b'1', 42, 0, 0xa5]).is_err());
}

#[test]
fn receptionist_rejects_trailing_data() {
    assert!(decode_sync(&[2, b'n', b'1', 0, 0xa5]).is_err());
}

#[test]
fn all_truncated_prefixes_fail() {
    let gossip = [0, 2, b'n', b'1', 42, 0];
    let sync = [2, b'n', b'1', 0];
    for n in 0..gossip.len() {
        assert!(decode_gossip(&gossip[..n]).is_err(), "gossip prefix {n}");
    }
    for n in 0..sync.len() {
        assert!(decode_sync(&sync[..n]).is_err(), "sync prefix {n}");
    }
}

#[test]
fn gossip_missing_string_bytes_do_not_allocate_declared_length() {
    let mut wire = vec![0]; // Empty events, followed by seen_from.
    wire.extend(length(8 * 1024 * 1024));
    bounded_rejection(|| decode_gossip(&wire).is_err());
}

#[test]
fn receptionist_missing_string_bytes_do_not_allocate_declared_length() {
    let wire = length(8 * 1024 * 1024);
    bounded_rejection(|| decode_sync(&wire).is_err());
}

#[test]
fn gossip_event_count_is_bounded_before_preallocation() {
    let wire = length((1 << 20) + 1);
    bounded_rejection(|| decode_gossip(&wire).is_err());
}

#[test]
fn gossip_full_sync_count_is_bounded_before_preallocation() {
    let mut wire = vec![0, 0, 0, 1]; // Events, seen_from, digest, Some(full_sync).
    wire.extend(length((1 << 20) + 1));
    bounded_rejection(|| decode_gossip(&wire).is_err());
}

#[test]
fn receptionist_entry_count_is_bounded_before_preallocation() {
    let mut wire = vec![0]; // Empty from.
    wire.extend(length((1 << 20) + 1));
    bounded_rejection(|| decode_sync(&wire).is_err());
}

fn member(name: &str) -> parrot_remote::swim::Member {
    use parrot_remote::swim::{Member, MemberStatus, NodeAddrWire};
    Member {
        node_id: name.into(),
        addr: NodeAddrWire {
            node_id: name.into(),
            scheme: "tcp".into(),
            host: "127.0.0.1".into(),
            port: 12345,
        },
        status: MemberStatus::Alive,
        incarnation: 253,
        status_until_ms: 0,
        metadata: bytes::Bytes::from_static(&[0, 128, 255]),
    }
}

#[test]
fn nested_nonempty_messages_preserve_every_field() {
    use parrot_remote::receptionist::{ApiKey, ReceptionistEntry};
    use parrot_remote::swim::{MemberEvent, MemberStatus};
    let mut remote = member("节点");
    remote.status = MemberStatus::Suspect;
    remote.status_until_ms = u64::MAX;
    let gossip = MembershipGossip {
        events: vec![
            MemberEvent::Upsert(remote.clone()),
            MemberEvent::Remove("gone".into()),
        ],
        seen_from: "发送方".into(),
        digest: u64::MAX,
        full_sync: Some(vec![remote]),
    };
    let encoded = encode_gossip(&gossip);
    let decoded = decode_gossip(&encoded[1..]).unwrap();
    assert_eq!(decoded.events, gossip.events);
    assert_eq!(decoded.seen_from, gossip.seen_from);
    assert_eq!(decoded.digest, gossip.digest);
    assert_eq!(decoded.full_sync, gossip.full_sync);
    let sync = ReceptionistSync {
        from: "发送方".into(),
        entries: vec![ReceptionistEntry {
            key: ApiKey::new("edge/服务").unwrap(),
            remote_path: "parrot://node/user/service".into(),
            node: "node".into(),
        }],
    };
    let encoded = encode_sync(&sync);
    let decoded = decode_sync(&encoded[1..]).unwrap();
    assert_eq!(decoded.from, sync.from);
    assert_eq!(decoded.entries, sync.entries);
}

#[test]
fn oversized_valid_payloads_fail_before_allocating_decoded_fields() {
    // Construct outside the measurement window; public decode calls must enforce
    // their own byte ceiling even when not reached through the transport frame cap.
    let large = "x".repeat(16 * 1024 * 1024);
    let sync = encode_sync(&ReceptionistSync {
        from: large.clone(),
        entries: vec![],
    });
    let gossip = encode_gossip(&MembershipGossip {
        events: vec![],
        seen_from: large,
        digest: 0,
        full_sync: None,
    });
    bounded_rejection(|| decode_sync(&sync[1..]).is_err());
    bounded_rejection(|| decode_gossip(&gossip[1..]).is_err());
}

#[test]
fn invalid_tags_and_utf8_are_rejected_without_poisoning_later_calls() {
    for bad in [&[1, 99][..], &[0, 0, 0, 2], &[0, 1, 255, 0, 0]] {
        assert!(decode_gossip(bad).is_err());
        assert!(decode_gossip(&[0, 0, 0, 0]).is_ok());
    }
    assert!(decode_sync(&[1, 255, 0]).is_err());
    assert!(decode_sync(&[0, 0]).is_ok());
}

#[tokio::test]
async fn real_tcp_rejects_tainted_gossip_before_membership_mutation() {
    use parrot_api::types::BoxedActorRef;
    use parrot_remote::swim::MemberEvent;
    use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
    use std::{sync::Arc, time::Duration};
    struct Empty;
    #[async_trait::async_trait]
    impl LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
            None
        }
    }
    fn event(node: &str, seen_from: &str) -> parrot_remote::frame::Frame {
        let mut frame = parrot_remote::frame::Frame::try_route_hint("", "").unwrap();
        frame.header.frame_type = parrot_remote::frame::frame_type::SYSTEM_EVENT;
        frame.payload = encode_gossip(&MembershipGossip {
            events: vec![MemberEvent::Upsert(member(node))],
            seen_from: seen_from.into(),
            digest: 0,
            full_sync: None,
        });
        frame
    }
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("decode-server", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("decode-client", None).with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let outcome: Result<bool, String> = async {
        client
            .connect(&NodeAddr::tcp(
                "decode-server",
                server.local_addr().unwrap(),
            ))
            .await
            .map_err(|e| e.to_string())?;
        let sender = client.links_snapshot().await[0].1.clone();
        let mut tainted = event("tainted-member", "decode-client");
        let mut bytes = tainted.payload.to_vec();
        bytes.push(0xa5);
        tainted.payload = bytes.into();
        sender.send(tainted).await.map_err(|e| e.to_string())?;
        sender
            .send(event("forged-origin", "other-node"))
            .await
            .map_err(|e| e.to_string())?;
        let mut malformed: Vec<_> = [
            "mismatched-identity",
            "unknown-scheme",
            "missing-port",
            "missing-host",
            "wildcard-ipv4",
            "wildcard-ipv6",
            "bracketed-wire-ipv6",
            "bad/node",
        ]
        .map(member)
        .into();
        malformed[0].addr.node_id = "different-node".into();
        malformed[1].addr.scheme = "file".into();
        malformed[2].addr.port = 0;
        malformed[3].addr.host.clear();
        malformed[4].addr.host = "0.0.0.0".into();
        malformed[5].addr.host = "::".into();
        malformed[6].addr.host = "[::1]".into();
        for full_sync in [false, true] {
            let mut frame = event("unused", "decode-client");
            frame.payload = encode_gossip(&MembershipGossip {
                events: if full_sync {
                    Vec::new()
                } else {
                    malformed.iter().cloned().map(MemberEvent::Upsert).collect()
                },
                seen_from: "decode-client".into(),
                digest: 0,
                full_sync: full_sync.then(|| malformed.clone()),
            });
            sender.send(frame).await.map_err(|e| e.to_string())?;
        }
        sender
            .send(event("accepted-marker", "decode-client"))
            .await
            .map_err(|e| e.to_string())?;
        // All frames use the same ordered control lane. The valid marker proves
        // the malformed frames were handled, without a guessed sleep.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = server.membership.lock().await;
                if state.members.contains_key("accepted-marker") {
                    return !state.members.contains_key("tainted-member")
                        && !state.members.contains_key("forged-origin")
                        && malformed
                            .iter()
                            .all(|member| !state.members.contains_key(&member.node_id));
                }
                drop(state);
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|e| e.to_string())
    }
    .await;
    let client_shutdown = client.shutdown().await;
    let server_shutdown = server.shutdown().await;
    client_shutdown.unwrap();
    server_shutdown.unwrap();
    assert!(
        outcome.expect("valid following gossip must still be processed"),
        "malformed gossip or a forged origin mutated the live membership table"
    );
}
