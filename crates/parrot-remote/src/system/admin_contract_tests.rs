use super::*;
use crate::{admin::AdminReply, admin_v2::AdminReplyV2, frame::frame_type};

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}
fn system(name: &str) -> Arc<RemoteActorSystem> {
    let sys = RemoteActorSystem::new(RemoteConfig::mem(name), Arc::new(Empty)).unwrap();
    sys.install_admin_hook();
    sys
}
fn link(sys: &RemoteActorSystem) -> (FrameSender, mpsc::Receiver<Frame>) {
    let (tx, rx) = mpsc::channel(1);
    let sender = FrameSender::new(tx, "peer");
    sys.links.write().unwrap().push((
        "peer".into(),
        sender.clone(),
        Arc::new(NodeStatus::default()),
    ));
    (sender, rx)
}
fn pending(sys: &RemoteActorSystem, v2: bool) -> usize {
    if v2 {
        sys.admin_pending_v2.len()
    } else {
        sys.admin_pending.len()
    }
}
async fn request(sys: &Arc<RemoteActorSystem>, v2: bool) -> Result<u64, RemoteError> {
    request_status(sys, v2).await.map(|(id, _)| id)
}
async fn request_status(
    sys: &Arc<RemoteActorSystem>,
    v2: bool,
) -> Result<(u64, Option<(u16, String)>), RemoteError> {
    if v2 {
        sys.admin_roundtrip_v2(
            "peer",
            crate::admin_v2::AdminCommandV2::StopComponent {
                req_id: 0,
                path_prefix: "/test".into(),
            },
        )
        .await
        .map(|reply| match reply {
            AdminReplyV2::Failed {
                req_id,
                code,
                detail,
            } => (req_id, Some((code, detail))),
            other => (other.req_id(), None),
        })
    } else {
        sys.admin_roundtrip(
            "peer",
            crate::admin::AdminCommand::AdminStop {
                req_id: 0,
                target_path: "/test".into(),
                reply_to: String::new(),
            },
        )
        .await
        .map(|reply| match reply {
            AdminReply::Failed {
                req_id,
                code,
                detail,
            } => (req_id, Some((code, detail))),
            other => (other.req_id(), None),
        })
    }
}
fn reply(v2: bool, id: u64) -> Frame {
    let mut frame = Frame::heartbeat();
    frame.header.frame_type = frame_type::SYSTEM_EVENT;
    frame.header.correlation_id = id;
    frame.payload = if v2 {
        crate::admin_v2::encode_admin_reply_v2(&AdminReplyV2::Stopped { req_id: id })
    } else {
        crate::admin::encode_admin_reply(&AdminReply::Stopped { req_id: id })
    };
    frame
}
#[tokio::test]
async fn unknown_peer_does_not_leak_admin_pending() {
    for v2 in [false, true] {
        let sys = system(&format!("admin-unknown-{v2}"));
        assert!(request(&sys, v2).await.is_err());
        assert_eq!(
            pending(&sys, v2),
            0,
            "unknown peer left pending entry for v2={v2}"
        );
        sys.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn cancelled_admin_request_reclaims_pending() {
    for v2 in [false, true] {
        let sys = system(&format!("admin-cancel-{v2}"));
        let (_sender, mut rx) = link(&sys);
        let caller = sys.clone();
        let task = tokio::spawn(async move { request(&caller, v2).await });
        rx.recv().await.unwrap();
        assert_eq!(pending(&sys, v2), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(
            pending(&sys, v2),
            0,
            "cancel left pending entry for v2={v2}"
        );
        sys.shutdown().await.unwrap();
    }
}
#[tokio::test(start_paused = true)]
async fn admin_deadline_includes_queue_admission() {
    use futures::poll;
    for v2 in [false, true] {
        let sys = system(&format!("admin-full-{v2}"));
        let (sender, _rx) = link(&sys);
        sender.send(Frame::heartbeat()).await.unwrap();
        let mut req = Box::pin(request(&sys, v2));
        assert!(poll!(req.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(31)).await;
        let result = poll!(req.as_mut());
        drop(req);
        sys.shutdown().await.unwrap();
        assert!(
            matches!(result, std::task::Poll::Ready(Err(_))),
            "timeout excluded writer admission for v2={v2}"
        );
    }
}
#[tokio::test]
async fn wrong_connection_cannot_consume_admin_reply() {
    for v2 in [false, true] {
        let sys = system(&format!("admin-source-{v2}"));
        let (sender, mut rx) = link(&sys);
        let (wrong_tx, _wrong_rx) = mpsc::channel(1);
        let wrong = FrameSender::new(wrong_tx, "peer");
        let caller = sys.clone();
        let task = tokio::spawn(async move { request(&caller, v2).await });
        let frame = rx.recv().await.unwrap();
        let id = frame.header.correlation_id;
        sys.ingress.dispatch(reply(v2, id), &wrong, "peer").await;
        let kept = pending(&sys, v2);
        sys.ingress.dispatch(reply(v2, id), &sender, "peer").await;
        let result = task.await.unwrap();
        sys.shutdown().await.unwrap();
        assert_eq!(kept, 1, "wrong physical connection consumed v2={v2}");
        assert_eq!(result.unwrap(), id);
    }
}
#[tokio::test]
async fn reply_header_and_payload_ids_must_agree() {
    for v2 in [false, true] {
        let sys = system(&format!("admin-id-{v2}"));
        let (sender, mut rx) = link(&sys);
        let caller = sys.clone();
        let task = tokio::spawn(async move { request(&caller, v2).await });
        let id = rx.recv().await.unwrap().header.correlation_id;
        let mut invalid = reply(v2, id);
        invalid.header.correlation_id = id + 1;
        sys.ingress.dispatch(invalid, &sender, "peer").await;
        let kept = pending(&sys, v2);
        sys.ingress.dispatch(reply(v2, id), &sender, "peer").await;
        let _ = task.await.unwrap();
        sys.shutdown().await.unwrap();
        assert_eq!(kept, 1, "mismatched header consumed v2={v2}");
    }
}
#[tokio::test]
async fn shutdown_finishes_admin_v1_without_waiting_for_its_timeout() {
    let sys = system("admin-shutdown");
    let (_sender, mut rx) = link(&sys);
    let caller = sys.clone();
    let mut task = tokio::spawn(async move { request(&caller, false).await });
    rx.recv().await.unwrap();
    sys.shutdown().await.unwrap();
    let result = tokio::time::timeout(Duration::from_millis(300), &mut task).await;
    if result.is_err() {
        task.abort();
        let _ = task.await;
    }
    assert!(
        result.is_ok(),
        "shutdown left admin v1 waiting for reply timeout"
    );
    assert_eq!(pending(&sys, false), 0);
}
#[tokio::test]
async fn administrative_failure_preserves_request_id() {
    let a = crate::admin::AdminPending::default();
    let b = crate::admin_v2::AdminPendingV2::default();
    let (tx, rx) = tokio::sync::oneshot::channel();
    a.insert(42, tx).unwrap();
    a.fail_all("closed");
    let (tx2, rx2) = tokio::sync::oneshot::channel();
    b.insert(43, tx2).unwrap();
    b.fail_all("closed");
    assert_eq!(rx.await.unwrap().req_id(), 42);
    assert_eq!(rx2.await.unwrap().req_id(), 43);
}

#[tokio::test(start_paused = true)]
async fn reply_timeout_and_closed_writer_reclaim_admin_slots() {
    use futures::poll;
    for v2 in [false, true] {
        let sys = system(&format!("admin-timeout-{v2}"));
        let (sender, mut rx) = link(&sys);
        let mut req = Box::pin(request(&sys, v2));
        assert!(poll!(req.as_mut()).is_pending());
        rx.recv().await.unwrap();
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(matches!(
            poll!(req.as_mut()),
            std::task::Poll::Ready(Err(_))
        ));
        drop(req);
        assert_eq!(pending(&sys, v2), 0);
        let mut req = Box::pin(request(&sys, v2));
        assert!(poll!(req.as_mut()).is_pending());
        rx.recv().await.unwrap();
        sender.close();
        assert!(matches!(
            poll!(req.as_mut()),
            std::task::Poll::Ready(Err(_))
        ));
        drop(req);
        assert_eq!(pending(&sys, v2), 0);
        sys.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn pending_capacity_rejects_before_sending_and_recovers_after_cancellation() {
    use futures::poll;
    for v2 in [false, true] {
        let mut cfg = RemoteConfig::mem(format!("admin-capacity-{v2}"));
        cfg.callback_capacity = 1;
        let sys = RemoteActorSystem::new(cfg, Arc::new(Empty)).unwrap();
        sys.install_admin_hook();
        let (sender, mut rx) = link(&sys);
        let mut first = Box::pin(request(&sys, v2));
        assert!(poll!(first.as_mut()).is_pending());
        rx.recv().await.unwrap();
        assert!(matches!(
            request(&sys, v2).await,
            Err(RemoteError::CallbacksFull(1))
        ));
        assert!(rx.try_recv().is_err(), "rejected request reached writer");
        drop(first);
        assert_eq!(pending(&sys, v2), 0);
        let mut next = Box::pin(request(&sys, v2));
        assert!(poll!(next.as_mut()).is_pending());
        let id = rx.recv().await.unwrap().header.correlation_id;
        sys.ingress.dispatch(reply(v2, id), &sender, "peer").await;
        assert_eq!(next.await.unwrap(), id);
        sys.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn admin_uses_latest_live_connection_and_old_close_does_not_fail_new_request() {
    use futures::poll;
    for v2 in [false, true] {
        let sys = system(&format!("admin-replacement-{v2}"));
        let (old, mut old_rx) = link(&sys);
        let mut first = Box::pin(request_status(&sys, v2));
        assert!(poll!(first.as_mut()).is_pending());
        let first_id = old_rx.recv().await.unwrap().header.correlation_id;
        let (new, mut new_rx) = link(&sys);
        let mut second = Box::pin(request(&sys, v2));
        assert!(poll!(second.as_mut()).is_pending());
        let id = new_rx
            .try_recv()
            .expect("request selected old connection")
            .header
            .correlation_id;
        old.close();
        admin_disconnect_handler(&sys.ingress, &sys.admin_pending, &sys.admin_pending_v2)(&old);
        assert_eq!(
            first.await.unwrap(),
            (
                first_id,
                Some((
                    crate::ErrCode::ConnectionLost as u16,
                    "connection lost".into()
                ))
            )
        );
        assert_eq!(pending(&sys, v2), 1);
        sys.ingress.dispatch(reply(v2, id), &new, "peer").await;
        assert_eq!(second.await.unwrap(), id);
        sys.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn legacy_admin_error_does_not_collide_with_ordinary_callback() {
    use futures::poll;
    for v2 in [false, true] {
        let sys = system(&format!("admin-namespace-{v2}"));
        let (sender, mut rx) = link(&sys);
        let ordinary_id = sys.callbacks.next_cid();
        let (tx, mut ordinary) = tokio::sync::oneshot::channel();
        sys.callbacks
            .insert_on(ordinary_id, "peer", tx, &sender)
            .unwrap();
        let mut admin = Box::pin(request_status(&sys, v2));
        assert!(poll!(admin.as_mut()).is_pending());
        let id = rx.recv().await.unwrap().header.correlation_id;
        assert_ne!(id, ordinary_id);
        sys.ingress
            .dispatch(
                Frame::reply_err(id, "", crate::ErrCode::Forbidden, "denied"),
                &sender,
                "peer",
            )
            .await;
        assert_eq!(
            admin.await.unwrap(),
            (
                id,
                Some((crate::ErrCode::Forbidden as u16, "denied".into()))
            )
        );
        assert!(matches!(
            ordinary.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(sys.callbacks.len(), 1);
        sys.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn pending_duplicates_unbound_sources_and_stale_guards_preserve_other_waiters() {
    use crate::admin::AdminPending;
    let table = AdminPending::new(2);
    let (tx, _rx) = mpsc::channel(1);
    let sender = FrameSender::new(tx, "peer");
    let (reply_rx, guard) = table.register_on(42, &sender).unwrap();
    let (replacement, _) = tokio::sync::oneshot::channel();
    assert!(table.insert_on(42, replacement, &sender).is_err());
    table.remove(42);
    assert!(reply_rx.await.is_err());
    let (tx, rx) = tokio::sync::oneshot::channel();
    table.insert(42, tx).unwrap();
    drop(guard);
    assert_eq!(table.len(), 1, "old guard removed replacement");
    assert!(!table.complete_from(AdminReply::Stopped { req_id: 42 }, &sender));
    assert!(table.complete(AdminReply::Stopped { req_id: 42 }));
    assert_eq!(rx.await.unwrap().req_id(), 42);
    table.close("closed");
    let (tx, _) = tokio::sync::oneshot::channel();
    assert!(table.insert_on(43, tx, &sender).is_err());
}

#[tokio::test]
async fn saturated_admin_reply_queue_closes_connection_without_waiting() {
    for v2 in [false, true] {
        let sys = system(&format!("admin-return-full-{v2}"));
        let (sender, _rx) = link(&sys);
        sender.send(Frame::heartbeat()).await.unwrap();
        let payload = if v2 {
            crate::admin_v2::encode_admin_cmd_v2(&crate::admin_v2::AdminCommandV2::StopComponent {
                req_id: 42,
                path_prefix: "/missing".into(),
            })
        } else {
            crate::admin::encode_admin_cmd(&crate::admin::AdminCommand::AdminStop {
                req_id: 42,
                target_path: "/missing".into(),
                reply_to: String::new(),
            })
        };
        let mut frame = reply(v2, 42);
        frame.payload = payload;
        tokio::time::timeout(
            Duration::from_secs(1),
            sys.ingress.dispatch(frame, &sender, "peer"),
        )
        .await
        .expect("full reply queue blocked dispatcher");
        assert!(sender.is_closed());
        sys.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn inconsistent_command_header_does_not_execute_stop() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Count(AtomicUsize);
    #[async_trait::async_trait]
    impl LocalLookup for Count {
        async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            None
        }
    }
    let local = Arc::new(Count(AtomicUsize::new(0)));
    let sys = RemoteActorSystem::new(
        RemoteConfig::mem("admin-bad-command-id").with_insecure_development(),
        local.clone(),
    )
    .unwrap();
    sys.install_admin_hook();
    let (sender, mut rx) = link(&sys);
    let mut frame = reply(false, 43);
    frame.payload = crate::admin::encode_admin_cmd(&crate::admin::AdminCommand::AdminStop {
        req_id: 42,
        target_path: "/target".into(),
        reply_to: String::new(),
    });
    sys.ingress.dispatch(frame.clone(), &sender, "peer").await;
    assert_eq!(local.0.load(Ordering::SeqCst), 0);
    let rejected = rx.try_recv().expect("invalid command must get a receipt");
    assert_eq!(rejected.header.correlation_id, 43);
    assert!(matches!(
        crate::admin::handle_admin_reply_frame(&rejected).unwrap(),
        Some(AdminReply::Failed { req_id: 43, code, .. })
            if code == crate::ErrCode::ProtocolViolation as u16
    ));
    frame.header.correlation_id = 42;
    sys.ingress.dispatch(frame, &sender, "peer").await;
    assert_eq!(local.0.load(Ordering::SeqCst), 1);
    let frame = rx.recv().await.unwrap();
    assert_eq!(frame.header.correlation_id, 42);
    assert!(matches!(
        crate::admin::handle_admin_reply_frame(&frame).unwrap(),
        Some(AdminReply::Failed { req_id: 42, .. })
    ));
    sys.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_admin_command_closes_saturated_reply_connection() {
    for v2 in [false, true] {
        for malformed in [false, true] {
            let sys = system(&format!("invalid-admin-full-{v2}-{malformed}"));
            let (sender, mut rx) = link(&sys);
            sender.send(Frame::heartbeat()).await.unwrap();
            let mut frame = reply(v2, 43);
            frame.payload = if v2 {
                crate::admin_v2::encode_admin_cmd_v2(
                    &crate::admin_v2::AdminCommandV2::StopComponent {
                        req_id: 42,
                        path_prefix: "/target".into(),
                    },
                )
            } else {
                crate::admin::encode_admin_cmd(&crate::admin::AdminCommand::AdminStop {
                    req_id: 42,
                    target_path: "/target".into(),
                    reply_to: String::new(),
                })
            };
            if malformed {
                frame.payload = frame.payload.slice(..1);
            }
            tokio::time::timeout(
                Duration::from_secs(1),
                sys.ingress.dispatch(frame, &sender, "peer"),
            )
            .await
            .expect("invalid command blocked on full queue");
            assert!(sender.is_closed(), "rejection was silently discarded");
            assert_eq!(
                rx.try_recv().unwrap().header.frame_type,
                frame_type::HEARTBEAT
            );
            assert!(rx.try_recv().is_err());
            sys.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn incorrect_executor_reply_id_returns_protocol_failure_for_original_request() {
    use crate::admin_v2::*;
    struct Wrong;
    #[async_trait::async_trait]
    impl ComponentExecutor for Wrong {
        async fn deploy(&self, _: u64, _: &ComponentDeploy) -> AdminReplyV2 {
            panic!("unexpected")
        }
        async fn drain(&self, _: u64, _: &str, _: u64) -> AdminReplyV2 {
            panic!("unexpected")
        }
        async fn stop(&self, _: u64, _: &str) -> AdminReplyV2 {
            AdminReplyV2::Stopped { req_id: 999 }
        }
        async fn status(&self, _: u64, _: &str) -> AdminReplyV2 {
            panic!("unexpected")
        }
    }
    let (tx, mut rx) = mpsc::channel(1);
    let back = FrameSender::new(tx, "peer");
    handle_admin_command_v2(
        AdminCommandV2::StopComponent {
            req_id: 42,
            path_prefix: "/test".into(),
        },
        Some(&Wrong),
        &back,
        "",
    )
    .await;
    let frame = rx.recv().await.unwrap();
    assert_eq!(frame.header.correlation_id, 42);
    match crate::admin::decode_sys_event(&frame.payload).unwrap() {
        crate::admin::SysEvent::AdminReplyV2(AdminReplyV2::Failed { req_id, code, .. }) => {
            assert_eq!(req_id, 42);
            assert_eq!(code, crate::ErrCode::ProtocolViolation as u16);
        }
        other => panic!("wrong reply: {other:?}"),
    }
}

#[tokio::test]
async fn admin_deadline_covers_discovery_gate_slots_and_real_handshake() {
    use tokio::io::AsyncReadExt;
    for v2 in [false, true] {
        for stage in ["gate", "slots", "handshake"] {
            let sys = RemoteActorSystem::new(
                RemoteConfig::tcp(format!("admin-budget-{stage}-{v2}"), None)
                    .with_insecure_development(),
                Arc::new(Empty),
            )
            .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            sys.nodes.reconcile_membership(vec![NodeAddr::tcp(
                "peer",
                listener.local_addr().unwrap(),
            )
            .into()]);
            let gate = Arc::new(tokio::sync::Mutex::new(()));
            let _gate_guard = if stage == "gate" {
                sys.discovery_gates
                    .lock()
                    .unwrap()
                    .insert("peer".into(), Arc::downgrade(&gate));
                Some(gate.lock().await)
            } else {
                None
            };
            let _slots = if stage == "slots" {
                Some(
                    sys.discovery_slots
                        .clone()
                        .acquire_many_owned(4)
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            let mut listener = Some(listener);
            let observer = if stage == "handshake" {
                let listener = listener.take().unwrap();
                Some(tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    socket.read_to_end(&mut bytes).await.unwrap();
                    bytes
                }))
            } else {
                None
            };
            let timeout = Duration::from_millis(50);
            let result = tokio::time::timeout(Duration::from_secs(1), async {
                if v2 {
                    sys.admin_exchange(
                        "peer",
                        &sys.admin_pending_v2,
                        1,
                        bytes::Bytes::new(),
                        timeout,
                        0,
                    )
                    .await
                    .map(|_| ())
                } else {
                    sys.admin_exchange(
                        "peer",
                        &sys.admin_pending,
                        1,
                        bytes::Bytes::new(),
                        timeout,
                        0,
                    )
                    .await
                    .map(|_| ())
                }
            })
            .await
            .expect("admin's own deadline must include discovery waits");
            assert!(result.unwrap_err().to_string().contains("admin timeout"));
            assert_eq!(pending(&sys, v2), 0);
            assert!(sys.links_snapshot().await.is_empty());
            if let Some(observer) = observer {
                let bytes = tokio::time::timeout(Duration::from_secs(1), observer)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    !bytes.is_empty(),
                    "a real HELLO must precede timeout and EOF"
                );
            }
            if let Some(listener) = listener {
                assert!(
                    tokio::time::timeout(Duration::from_millis(10), listener.accept())
                        .await
                        .is_err(),
                    "waiting for {stage} must not initiate a physical connection"
                );
            }
            sys.shutdown().await.unwrap();
        }
    }
}
