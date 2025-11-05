use super::*;
use crate::mirror_control::{MirrorControl as C, MirrorControlReply as R, MirrorSessionId};
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}
#[tokio::test]
async fn old_peer_is_rejected_before_sending_or_reserving_a_callback() {
    let sys = RemoteActorSystem::new(
        RemoteConfig::mem(format!("mirror-old-{}", uuid::Uuid::new_v4())),
        Arc::new(Empty),
    )
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    sys.links.write().unwrap().push((
        "old-peer".into(),
        FrameSender::new(tx, "old-peer"),
        Arc::new(NodeStatus::default()),
    ));
    let error = sys
        .control_mirror(
            "old-peer",
            C::Open {
                owner: "app".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("required admin capabilities"),
        "{error}"
    );
    assert_eq!(sys.admin_pending_v2.len(), 0);
    assert!(rx.try_recv().is_err());
    sys.shutdown().await.unwrap();
}
#[test]
fn mirror_command_discriminants_are_additive_and_match_independent_bytes() {
    use crate::admin_v2::{AdminCommandV2 as C2, AdminReplyV2 as R2};
    let command = C2::MirrorControl {
        req_id: 42,
        command: C::Open { owner: "a".into() },
    };
    let wire = crate::admin_v2::encode_admin_cmd_v2(&command);
    assert_eq!(wire.as_ref(), &[3, 6, 42, 0, 1, b'a']);
    assert_eq!(
        crate::admin_v2::encode_admin_cmd_v2(&C2::MetricsReportV2 { req_id: 42 }).as_ref(),
        &[3, 5, 42]
    );
    let crate::admin::SysEvent::AdminCommandV2(decoded) =
        crate::admin::decode_sys_event(&wire).unwrap()
    else {
        panic!("wrong tag")
    };
    assert_eq!(decoded, command);
    let session = MirrorSessionId {
        incarnation: [7; 16],
        sequence: 9,
    };
    let reply = R2::MirrorControl {
        req_id: 42,
        result: R::Retired { session },
    };
    let wire = crate::admin_v2::encode_admin_reply_v2(&reply);
    let mut expected = vec![4, 7, 42, 1];
    expected.extend([7; 16]);
    expected.push(9);
    assert_eq!(wire.as_ref(), expected);
    let crate::admin::SysEvent::AdminReplyV2(decoded) =
        crate::admin::decode_sys_event(&wire).unwrap()
    else {
        panic!("wrong tag")
    };
    assert_eq!(decoded, reply);
    for length in 0..wire.len() {
        assert!(crate::admin::decode_sys_event(&wire[..length]).is_err());
    }
}

#[tokio::test]
async fn unnegotiated_inbound_command_cannot_allocate_a_session() {
    let mut config = RemoteConfig::mem(format!("mirror-unnegotiated-{}", uuid::Uuid::new_v4()));
    config.security = crate::security::RemoteSecurity::InsecureDevelopment;
    let sys = RemoteActorSystem::new(config, Arc::new(Empty)).unwrap();
    sys.start().await.unwrap();
    let command = crate::admin_v2::AdminCommandV2::MirrorControl {
        req_id: 42,
        command: C::Open {
            owner: "app".into(),
        },
    };
    let mut frame = crate::admin_v2::reply_frame_v2(
        42,
        "",
        &crate::admin_v2::AdminReplyV2::Stopped { req_id: 42 },
    );
    frame.payload = crate::admin_v2::encode_admin_cmd_v2(&command);
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let back = FrameSender::new(tx, "legacy-peer");
    sys.ingress.dispatch(frame, &back, "legacy-peer").await;
    let reply = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::admin::SysEvent::AdminReplyV2(crate::admin_v2::AdminReplyV2::Failed {
        detail, ..
    }) = crate::admin::decode_sys_event(&reply.payload).unwrap()
    else {
        panic!("must reject unnegotiated control")
    };
    assert!(detail.contains("not negotiated"), "{detail}");
    let created = sys
        .ingress
        .control_mirror(C::Open {
            owner: "app".into(),
        })
        .await
        .unwrap();
    assert_eq!(created.session().sequence, 1);
    sys.shutdown().await.unwrap();
}

#[test]
fn reply_correlation_alone_does_not_confirm_the_requested_revision() {
    use crate::mirror_control::{MirrorPolicySpec as P, MirrorSessionPhase as Phase};
    let session = MirrorSessionId {
        incarnation: [7; 16],
        sequence: 9,
    };
    let policies = vec![P::new("/a", "/b")];
    let command = C::Apply {
        session,
        revision: 2,
        policies: policies.clone(),
    };
    let correct = R::Session {
        session,
        owner: "app".into(),
        revision: 2,
        phase: Phase::Active,
        policies: policies.clone(),
    };
    assert!(command.accepts(&correct));
    assert!(!command.accepts(&R::Retired { session }));
    for (revision, phase) in [
        (1, Phase::Active),
        (2, Phase::Closing),
        (2, Phase::Superseded),
    ] {
        assert!(!command.accepts(&R::Session {
            session,
            owner: "app".into(),
            revision,
            phase,
            policies: policies.clone()
        }));
    }
    assert!(!C::Close { session }.accepts(&correct));
    assert!(!C::Open {
        owner: "other".into()
    }
    .accepts(&correct));
    assert!(!command.accepts(&R::Session {
        session,
        owner: "app".into(),
        revision: 2,
        phase: Phase::Active,
        policies: vec![P::new("/a", "/c")]
    }));
    let wrong = MirrorSessionId {
        sequence: 10,
        ..session
    };
    assert!(!C::Status { session: wrong }.accepts(&correct));
}

#[test]
fn mirror_session_fields_match_frozen_protocol_vectors() {
    use crate::admin_v2::{AdminCommandV2 as C2, AdminReplyV2 as R2};
    use crate::mirror_control::{MirrorPolicySpec as P, MirrorSessionPhase as Phase};
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../../docs/vectors/mirror_control.json")).unwrap();
    let session = MirrorSessionId {
        incarnation: [7; 16],
        sequence: 9,
    };
    let policy = P {
        source_prefix: "/a".into(),
        shadow_path: "/b".into(),
        capacity: 2,
        max_message_bytes: 64,
        max_pending_bytes: 128,
        delivery_timeout_ms: 100,
    };
    let mut vectors = Vec::new();
    for (name, command) in [
        (
            "open",
            C::Open {
                owner: "app".into(),
            },
        ),
        (
            "apply",
            C::Apply {
                session,
                revision: 2,
                policies: vec![policy.clone()],
            },
        ),
        ("status", C::Status { session }),
        ("close", C::Close { session }),
    ] {
        vectors.push((
            name,
            crate::admin_v2::encode_admin_cmd_v2(&C2::MirrorControl {
                req_id: 42,
                command,
            }),
        ));
    }
    for (name, phase) in [("active", Phase::Active), ("closing", Phase::Closing)] {
        vectors.push((
            name,
            crate::admin_v2::encode_admin_reply_v2(&R2::MirrorControl {
                req_id: 42,
                result: R::Session {
                    session,
                    owner: "app".into(),
                    revision: 2,
                    phase,
                    policies: vec![policy.clone()],
                },
            }),
        ));
    }
    vectors.push((
        "retired",
        crate::admin_v2::encode_admin_reply_v2(&R2::MirrorControl {
            req_id: 42,
            result: R::Retired { session },
        }),
    ));
    assert_eq!(fixture["vectors"].as_object().unwrap().len(), vectors.len());
    for (name, bytes) in vectors {
        let hex = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(hex, fixture["vectors"][name].as_str().unwrap(), "{name}");
        assert!(crate::admin::decode_sys_event(&bytes).is_ok());
    }
}
