//! Administrative replies obey the same physical connection boundary as ASK.
use futures::{SinkExt, StreamExt};
use parrot_remote::{
    admin::{AdminReply, SysEvent},
    admin_v2::{AdminArtifactRef, AdminInstancePolicy, AdminReplyV2, ComponentDeploy},
    frame::{frame_type, Frame},
    transport::FrameCodec,
    LocalLookup, RemoteActorSystem, RemoteConfig,
};
use std::{sync::Arc, time::Duration};
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}
type Peer = tokio_util::codec::Framed<tokio::net::TcpStream, FrameCodec>;
async fn system(name: &str) -> Arc<RemoteActorSystem> {
    let sys = RemoteActorSystem::new(
        RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap())).with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    sys.start().await.unwrap();
    sys
}
async fn peer(sys: &Arc<RemoteActorSystem>, count: usize) -> Peer {
    let mut peer = Peer::new(
        tokio::net::TcpStream::connect(sys.local_addr().unwrap())
            .await
            .unwrap(),
        FrameCodec,
    );
    let mut bytes = bytes::BytesMut::new();
    parrot_remote::HandshakeBody {
        node_id: "peer".into(),
        ..Default::default()
    }
    .encode_tlv(&mut bytes)
    .unwrap();
    peer.send(Frame::handshake(bytes.freeze())).await.unwrap();
    assert_eq!(
        peer.next().await.unwrap().unwrap().header.frame_type,
        frame_type::HANDSHAKE_ACK
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while sys.links_snapshot().await.len() != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    peer
}
async fn business(peer: &mut Peer) -> Frame {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = peer.next().await.unwrap().unwrap();
            match frame.header.frame_type {
                frame_type::HEARTBEAT => peer.send(Frame::heartbeat_ack()).await.unwrap(),
                frame_type::HEARTBEAT_ACK => {}
                _ => return frame,
            }
        }
    })
    .await
    .unwrap()
}
async fn request(
    sys: &Arc<RemoteActorSystem>,
    v2: bool,
) -> Result<Vec<String>, parrot_remote::RemoteError> {
    if v2 {
        sys.deploy_component(
            "peer",
            ComponentDeploy {
                name: "test".into(),
                version: "1".into(),
                artifact: AdminArtifactRef::Props {
                    factory: "unused".into(),
                },
                instances: AdminInstancePolicy::Singleton,
                config: None,
            },
        )
        .await
    } else {
        sys.spawn_named("peer", "/requested", "unused")
            .await
            .map(|actor| vec![parrot_api::address::ActorRef::path(&actor)])
    }
}
fn response(v2: bool, id: u64, success: bool, path: &str) -> Frame {
    if v2 {
        let reply = if success {
            AdminReplyV2::Deployed {
                req_id: id,
                instances: vec![path.into()],
            }
        } else {
            AdminReplyV2::Failed {
                req_id: id,
                code: parrot_remote::ErrCode::Forbidden as u16,
                detail: "forged".into(),
            }
        };
        parrot_remote::admin_v2::reply_frame_v2(id, "", &reply)
    } else {
        let reply = if success {
            AdminReply::Spawned {
                req_id: id,
                path: path.into(),
            }
        } else {
            AdminReply::Failed {
                req_id: id,
                code: parrot_remote::ErrCode::Forbidden as u16,
                detail: "forged".into(),
            }
        };
        parrot_remote::admin::reply_frame(id, "", &reply)
    }
}
async fn barrier(peer: &mut Peer, id: u64) {
    // The same socket's subsequent ASK is dispatched after the preceding
    // management frame. Its actual reply provides an ordering barrier.
    peer.send(Frame::ask(
        id,
        "/missing",
        "missing",
        bytes::Bytes::new(),
        None,
    ))
    .await
    .unwrap();
    let response = business(peer).await;
    assert_eq!(response.header.correlation_id, id);
    assert_eq!(response.header.frame_type, frame_type::REPLY_ERR);
}
fn id(frame: &Frame, v2: bool) -> u64 {
    assert_eq!(frame.header.frame_type, frame_type::SYSTEM_EVENT);
    let req_id = match parrot_remote::admin::decode_sys_event(&frame.payload).unwrap() {
        SysEvent::AdminCommand(cmd) if !v2 => cmd.req_id(),
        SysEvent::AdminCommandV2(cmd) if v2 => cmd.req_id(),
        other => panic!("unexpected event {other:?}"),
    };
    assert_eq!(req_id, frame.header.correlation_id);
    req_id
}
fn expected(v2: bool) -> Vec<String> {
    vec![if v2 {
        "/user/actual"
    } else {
        "parrot://peer/user/actual"
    }
    .into()]
}
#[tokio::test]
async fn tcp_wrong_connection_and_mismatched_envelope_cannot_complete_admin_requests() {
    for v2 in [false, true] {
        let sys = system(&format!("admin-spoof-{v2}")).await;
        let mut wrong = peer(&sys, 1).await;
        let mut legitimate = peer(&sys, 2).await;
        let caller = sys.clone();
        let task = tokio::spawn(async move { request(&caller, v2).await });
        let req = id(&business(&mut legitimate).await, v2);
        for (i, frame) in [
            response(v2, req, true, "/user/forged"),
            response(v2, req, false, ""),
            Frame::reply_err(req, "", parrot_remote::ErrCode::Forbidden, "forged"),
        ]
        .into_iter()
        .enumerate()
        {
            wrong.send(frame).await.unwrap();
            barrier(&mut wrong, 9000 + i as u64).await;
        }
        let mut mismatch = response(v2, req, true, "/user/wrong-header");
        mismatch.header.correlation_id = req + 1000;
        legitimate.send(mismatch).await.unwrap();
        barrier(&mut legitimate, 9100).await;
        let mut malformed = response(v2, req, true, "/user/malformed");
        let mut payload = malformed.payload.to_vec();
        payload.push(0);
        malformed.payload = payload.into();
        legitimate.send(malformed).await.unwrap();
        barrier(&mut legitimate, 9101).await;
        legitimate
            .send(response(v2, req, true, "/user/actual"))
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        sys.shutdown().await.unwrap();
        assert_eq!(result.unwrap(), expected(v2));
    }
}
#[tokio::test]
async fn tcp_connection_replacement_isolated_and_disconnect_finishes_old_admin_request() {
    for v2 in [false, true] {
        let sys = system(&format!("admin-replace-{v2}")).await;
        let mut old = peer(&sys, 1).await;
        let caller = sys.clone();
        let old_request = tokio::spawn(async move { request(&caller, v2).await });
        id(&business(&mut old).await, v2);
        let mut new = peer(&sys, 2).await;
        let caller = sys.clone();
        let new_request = tokio::spawn(async move { request(&caller, v2).await });
        let req = id(&business(&mut new).await, v2);
        drop(old);
        assert!(tokio::time::timeout(Duration::from_secs(1), old_request)
            .await
            .expect("connection failure must precede 10/30s request timeout")
            .unwrap()
            .is_err());
        new.send(response(v2, req, true, "/user/actual"))
            .await
            .unwrap();
        assert_eq!(new_request.await.unwrap().unwrap(), expected(v2));
        let caller = sys.clone();
        let recovery = tokio::spawn(async move { request(&caller, v2).await });
        let req = id(&business(&mut new).await, v2);
        new.send(response(v2, req, true, "/user/actual"))
            .await
            .unwrap();
        assert_eq!(recovery.await.unwrap().unwrap(), expected(v2));
        sys.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn tcp_shutdown_finishes_both_admin_versions_with_distinct_ids() {
    let sys = system("admin-both-close").await;
    let mut peer = peer(&sys, 1).await;
    let caller = sys.clone();
    let v1 = tokio::spawn(async move { request(&caller, false).await });
    let first = business(&mut peer).await.header.correlation_id;
    let caller = sys.clone();
    let v2 = tokio::spawn(async move { request(&caller, true).await });
    let second = business(&mut peer).await.header.correlation_id;
    assert_ne!(first, second);
    sys.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        assert!(v1.await.unwrap().is_err());
        assert!(v2.await.unwrap().is_err());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hostile_admin_payloads_are_rejected_without_breaking_the_connection() {
    let sys = system("admin-hostile").await;
    let mut peer = peer(&sys, 1).await;
    let mut impossible = parrot_remote::admin::encode_admin_reply(&AdminReply::Spawned {
        req_id: 1,
        path: String::new(),
    })
    .to_vec();
    assert_eq!(impossible.pop(), Some(0));
    impossible
        .extend(bincode::serde::encode_to_vec(u64::MAX, bincode::config::standard()).unwrap());
    let mut frame = response(false, 1, true, "");
    frame.payload = impossible.into();
    peer.send(frame).await.unwrap();
    barrier(&mut peer, 9200).await;
    for v2 in [false, true] {
        let id = if v2 { 9301 } else { 9300 };
        let payload = if v2 {
            parrot_remote::admin_v2::encode_admin_cmd_v2(
                &parrot_remote::admin_v2::AdminCommandV2::StopComponent {
                    req_id: id,
                    path_prefix: "/missing".into(),
                },
            )
        } else {
            parrot_remote::admin::encode_admin_cmd(&parrot_remote::admin::AdminCommand::AdminStop {
                req_id: id,
                target_path: "/missing".into(),
                reply_to: String::new(),
            })
        };
        let mut valid = response(v2, id, true, "");
        valid.payload = payload;
        let mut invalid = valid.clone();
        let mut payload = invalid.payload.to_vec();
        payload.push(0);
        invalid.payload = payload.into();
        peer.send(invalid).await.unwrap();
        assert_invalid_command_reply(&business(&mut peer).await, v2, id);
        barrier(&mut peer, id + 10).await;
        // Well-formed control traffic still gets a typed failure from Empty.
        peer.send(valid).await.unwrap();
        let reply = business(&mut peer).await;
        assert_eq!(reply.header.correlation_id, id);
        match parrot_remote::admin::decode_sys_event(&reply.payload).unwrap() {
            SysEvent::AdminReply(AdminReply::Failed { req_id, code, .. }) if !v2 => {
                assert_eq!(req_id, id);
                assert_eq!(code, parrot_remote::ErrCode::ActorNotFound as u16);
            }
            SysEvent::AdminReplyV2(AdminReplyV2::Failed { req_id, code, .. }) if v2 => {
                assert_eq!(req_id, id);
                assert_eq!(code, parrot_remote::admin_v2::v2_err::DIALECT_MISMATCH);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }
    sys.shutdown().await.unwrap();
}

struct CountOperations(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl LocalLookup for CountOperations {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        None
    }
}
#[async_trait::async_trait]
impl parrot_remote::admin_v2::ComponentExecutor for CountOperations {
    async fn deploy(&self, _: u64, _: &ComponentDeploy) -> AdminReplyV2 {
        panic!("unexpected deploy")
    }
    async fn drain(&self, _: u64, _: &str, _: u64) -> AdminReplyV2 {
        panic!("unexpected drain")
    }
    async fn stop(&self, req_id: u64, _: &str) -> AdminReplyV2 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        AdminReplyV2::Stopped { req_id }
    }
    async fn status(&self, _: u64, _: &str) -> AdminReplyV2 {
        panic!("unexpected status")
    }
}
fn assert_invalid_command_reply(frame: &Frame, v2: bool, id: u64) {
    assert_eq!(frame.header.frame_type, frame_type::SYSTEM_EVENT);
    assert_eq!(frame.header.correlation_id, id);
    let (req_id, code) = match parrot_remote::admin::decode_sys_event(&frame.payload).unwrap() {
        SysEvent::AdminReply(AdminReply::Failed { req_id, code, .. }) if !v2 => (req_id, code),
        SysEvent::AdminReplyV2(AdminReplyV2::Failed { req_id, code, .. }) if v2 => (req_id, code),
        other => panic!("expected command rejection, got {other:?}"),
    };
    assert_eq!(req_id, id);
    assert_eq!(
        code,
        if v2 {
            parrot_remote::admin_v2::v2_err::DIALECT_MISMATCH
        } else {
            parrot_remote::ErrCode::ProtocolViolation as u16
        }
    );
}
async fn invalid_command_receipt(v2: bool, malformed: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let counter = Arc::new(CountOperations(AtomicUsize::new(0)));
    let sys = RemoteActorSystem::new(
        RemoteConfig::tcp(
            format!("admin-invalid-{v2}-{malformed}"),
            Some("127.0.0.1:0".parse().unwrap()),
        )
        .with_insecure_development(),
        counter.clone(),
    )
    .unwrap();
    sys.set_component_executor(Some(counter.clone()));
    sys.start().await.unwrap();
    let mut peer = peer(&sys, 1).await;
    let mut valid = response(v2, 42, true, "");
    valid.payload = if v2 {
        parrot_remote::admin_v2::encode_admin_cmd_v2(
            &parrot_remote::admin_v2::AdminCommandV2::StopComponent {
                req_id: 42,
                path_prefix: "/target".into(),
            },
        )
    } else {
        parrot_remote::admin::encode_admin_cmd(&parrot_remote::admin::AdminCommand::AdminStop {
            req_id: 42,
            target_path: "/target".into(),
            reply_to: String::new(),
        })
    };
    let bad_frames = if malformed {
        let mut tag_only = valid.clone();
        tag_only.payload = valid.payload.slice(..1);
        let mut truncated = valid.clone();
        truncated.payload = valid.payload.slice(..valid.payload.len() - 1);
        let mut trailing = valid.clone();
        let mut bytes = valid.payload.to_vec();
        bytes.push(0);
        trailing.payload = bytes.into();
        vec![tag_only, truncated, trailing]
    } else {
        let mut mismatch = valid.clone();
        mismatch.header.correlation_id = 43;
        vec![mismatch]
    };
    for bad in bad_frames {
        let cid = bad.header.correlation_id;
        peer.send(bad).await.unwrap();
        assert_invalid_command_reply(&business(&mut peer).await, v2, cid);
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            0,
            "invalid command reached operations"
        );
    }
    peer.send(valid).await.unwrap();
    let reply = business(&mut peer).await;
    assert_eq!(reply.header.correlation_id, 42);
    match parrot_remote::admin::decode_sys_event(&reply.payload).unwrap() {
        SysEvent::AdminReplyV2(AdminReplyV2::Stopped { req_id: 42 }) if v2 => {}
        SysEvent::AdminReply(AdminReply::Failed {
            req_id: 42, code, ..
        }) if !v2 => {
            assert_eq!(code, parrot_remote::ErrCode::ActorNotFound as u16);
        }
        other => panic!("unexpected valid command receipt: {other:?}"),
    }
    assert_eq!(
        counter.0.load(Ordering::SeqCst),
        1,
        "valid command did not execute exactly once"
    );
    sys.shutdown().await.unwrap();
}
#[tokio::test]
async fn tcp_invalid_command_v1_mismatch_has_receipt_without_execution() {
    invalid_command_receipt(false, false).await;
}
#[tokio::test]
async fn tcp_invalid_command_v2_mismatch_has_receipt_without_execution() {
    invalid_command_receipt(true, false).await;
}
#[tokio::test]
async fn tcp_invalid_command_v1_malformed_has_receipt_without_execution() {
    invalid_command_receipt(false, true).await;
}
#[tokio::test]
async fn tcp_invalid_command_v2_malformed_has_receipt_without_execution() {
    invalid_command_receipt(true, true).await;
}
