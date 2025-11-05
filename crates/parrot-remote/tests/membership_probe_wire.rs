use parrot_remote::{
    admin::{decode_sys_event, SysEvent},
    swim::{ProbeEvent, ProbeOutcome, ProbeReply, ProbeRequest},
};

#[test]
fn recognizes_independently_encoded_membership_probe_messages() {
    // Four dedicated SYSTEM_EVENT tags. Standard bincode varints: request id 1,
    // target length 1 / "b", then budget 50 ms or Reachable outcome 0.
    for (tag, event) in [
        (
            0x11,
            ProbeEvent::Ping(ProbeRequest {
                request_id: 1,
                target: "b".into(),
                budget_ms: 50,
            }),
        ),
        (
            0x12,
            ProbeEvent::PingReq(ProbeRequest {
                request_id: 1,
                target: "b".into(),
                budget_ms: 50,
            }),
        ),
        (
            0x13,
            ProbeEvent::Pong(ProbeReply {
                request_id: 1,
                target: "b".into(),
                outcome: ProbeOutcome::Reachable,
            }),
        ),
        (
            0x14,
            ProbeEvent::PingReqAck(ProbeReply {
                request_id: 1,
                target: "b".into(),
                outcome: ProbeOutcome::Reachable,
            }),
        ),
    ] {
        let payload = vec![tag, 1, 1, b'b', if tag < 0x13 { 50 } else { 0 }];
        let SysEvent::MembershipProbe(decoded) = decode_sys_event(&payload).unwrap() else {
            panic!("wrong event")
        };
        assert_eq!(decoded, event);
        assert_eq!(event.encode().unwrap().as_ref(), payload);
    }
}

#[test]
fn malformed_wire_cannot_become_a_probe() {
    for tag in 0x11..=0x14 {
        let valid = vec![tag, 1, 1, b'b', if tag < 0x13 { 50 } else { 0 }];
        for len in 1..valid.len() {
            assert!(decode_sys_event(&valid[..len]).is_err());
        }
        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(decode_sys_event(&trailing).is_err());
        for invalid in [
            vec![tag, 0, 1, b'b', 1], // zero request identity
            vec![tag, 1, 0, 1],       // empty target
            vec![tag, 1, 1, 0xff, 1], // non-UTF8
            vec![tag, 1, 1, b'/', 1], // invalid node identity
            vec![tag, 1, 253, 255, 255, 255, 255, 255, 255, 255, 127], // huge claimed string
        ] {
            assert!(decode_sys_event(&invalid).is_err(), "accepted {invalid:?}");
        }
    }
    for tag in [0x11, 0x12] {
        assert!(decode_sys_event(&[tag, 1, 1, b'b', 0]).is_err());
        // 10001 encoded as a bincode u16 varint.
        assert!(decode_sys_event(&[tag, 1, 1, b'b', 251, 0x11, 0x27]).is_err());
    }
    for tag in [0x13, 0x14] {
        assert!(decode_sys_event(&[tag, 1, 1, b'b', 3]).is_err());
    }
}

#[test]
fn encoding_and_decoding_enforce_identity_and_budget_boundaries() {
    for budget_ms in [1, 10_000] {
        let event = ProbeEvent::PingReq(ProbeRequest {
            request_id: u64::MAX,
            target: "n".repeat(65_535),
            budget_ms,
        });
        let SysEvent::MembershipProbe(decoded) =
            decode_sys_event(&event.encode().unwrap()).unwrap()
        else {
            panic!("wrong event")
        };
        assert_eq!(decoded, event);
    }
    for (request_id, target, budget_ms) in [
        (0, "b".into(), 1),
        (1, "b".into(), 0),
        (1, "b".into(), 10_001),
        (1, "n".repeat(65_536), 1),
        (1, "x/y".into(), 1),
    ] {
        assert!(ProbeEvent::Ping(ProbeRequest {
            request_id,
            target,
            budget_ms
        })
        .encode()
        .is_err());
    }
    let oversized = vec![0x11; 65_535 + 66];
    assert!(decode_sys_event(&oversized).is_err());
}
