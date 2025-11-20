use super::*;
use crate::mirror::notification_contracts::{check, isolated_named, park, probe, run, Empty};
use futures::FutureExt;
use std::{future::Future, pin::Pin};

fn committed(system: &Arc<RemoteActorSystem>) -> bool {
    let Ok(gate) = system.link_gate.try_lock() else {
        return false;
    };
    let Ok(links) = system.links.try_read() else {
        return false;
    };
    let Ok(uplink) = system.uplink.try_read() else {
        return false;
    };
    let Ok(addresses) = system.dial_addrs.try_lock() else {
        return false;
    };
    if !links.is_empty()
        || uplink.is_some()
        || !addresses.is_empty()
        || system.receptionist.try_lock().is_err()
        || system.receptionist_assemblies.try_lock().is_err()
    {
        return false;
    }
    drop((gate, links, uplink, addresses));
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let (tx2, _rx2) = tokio::sync::oneshot::channel();
    let (tx3, _rx3) = tokio::sync::oneshot::channel();
    let fenced = system.callbacks.insert(9001, "late", tx).is_err()
        && system.admin_pending.insert(9001, tx2).is_err()
        && system.admin_pending_v2.insert(9001, tx3).is_err()
        && system.background.is_closed()
        && system.direct_connections.is_closed()
        && system
            .ingress
            .register_wire_mirror_group(vec![(
                crate::mirror::WireMirrorPolicy::new("/late", "/shadow"),
                crate::mirror::WireMirrorOptions::default(),
            )])
            .is_err()
        && system
            .transport
            .listen("127.0.0.1:0".parse().unwrap())
            .now_or_never()
            .is_some_and(|result| result.is_err())
        && system
            .ingress
            .control_mirror(crate::mirror_control::MirrorControl::Open {
                owner: "late".into(),
            })
            .now_or_never()
            .is_some_and(|result| result.is_err())
        && parrot_api::receptionist::ReceptionistGateway::register(
            &crate::receptionist::ReceptionistFacade::for_system(system),
            crate::receptionist::ApiKey::new("late").unwrap(),
            "/late".into(),
        )
        .now_or_never()
        .is_some_and(|result| result.is_err());
    if fenced {
        system.begin_close();
    }
    fenced
}

fn cohort(source: u8, throwing: u8) {
    run(async {
        let system = RemoteActorSystem::new(
            RemoteConfig::tcp("shutdown-notification", None),
            Arc::new(Empty),
        )
        .unwrap();
        let (tx, _writer) = mpsc::channel(128);
        let sender = FrameSender::new(tx, "shutdown-peer");
        let status = Arc::new(NodeStatus::default());
        status.set(NodeState::Connected);
        let address = NodeAddr::tcp("shutdown-peer", "127.0.0.1:9".parse().unwrap());
        system.nodes.connected(address.clone());
        let node_status = system.nodes.get("shutdown-peer").unwrap().1;
        system.links.write().unwrap().push(Link {
            address,
            sender: sender.clone(),
            status: status.clone(),
            role: crate::handshake::TopologyRole::Hub,
            dial_addr: Some("tcp://127.0.0.1:9".into()),
        });
        *system.uplink.write().unwrap() = Some(sender.clone());
        system
            .dial_addrs
            .lock()
            .unwrap()
            .insert("shutdown-peer".into(), "tcp://127.0.0.1:9".into());
        let mut waiters: Vec<Pin<Box<dyn Future<Output = ()> + '_>>> = Vec::new();
        let mut probes = Vec::new();
        let mirrors = if source == 4 {
            Some(
                system
                    .ingress
                    .register_wire_mirror_group(vec![
                        (
                            crate::mirror::WireMirrorPolicy::new("/a", "/shadow"),
                            Default::default(),
                        ),
                        (
                            crate::mirror::WireMirrorPolicy::new("/b", "/shadow"),
                            Default::default(),
                        ),
                    ])
                    .unwrap(),
            )
        } else {
            None
        };
        for id in 1..=40 {
            let weak = Arc::downgrade(&system);
            let status = status.clone();
            let node_status = node_status.clone();
            let sender_check = sender.clone();
            let p = probe(throwing, move || {
                let system = weak.upgrade().unwrap();
                committed(&system)
                    && sender_check.is_closed()
                    && status.get() == NodeState::Disconnected
                    && node_status.get() == NodeState::Disconnected
                    && system.nodes.get("shutdown-peer").is_none()
            });
            let mut waiter: Pin<Box<dyn Future<Output = ()> + '_>> = match source {
                0 => {
                    let mut shutdown = system.shutdown_signal();
                    // This source tests commit ordering; public callers are tested separately.
                    Box::pin(crate::transport::notify_safely(async move {
                        let _ = shutdown.wait_for(|closed| *closed).await;
                    }))
                }
                1 => Box::pin(async {
                    assert!(system
                        .background
                        .run(std::future::pending::<Result<(), RemoteError>>())
                        .await
                        .is_err());
                }),
                2 => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    system.callbacks.insert(id, "shutdown-peer", tx).unwrap();
                    Box::pin(async move {
                        assert!(
                            matches!(rx.await.unwrap(), crate::registry::ReplyPayload::Err(crate::ErrCode::ConnectionLost, detail) if detail == "system shutdown")
                        );
                    })
                }
                3 => Box::pin(sender.closed()),
                4 => {
                    let handle = mirrors
                        .as_ref()
                        .unwrap()
                        .handle_for(if id % 2 == 0 { "/a" } else { "/b" })
                        .unwrap();
                    Box::pin(crate::transport::notify_safely(
                        crate::mirror::notification_contracts::cancelled(&handle),
                    ))
                }
                5 => {
                    let mut stream = system.receptionist.lock().unwrap().subscribe(
                        &crate::receptionist::ApiKey::new(format!("shutdown-{id}")).unwrap(),
                        1,
                    );
                    Box::pin(async move {
                        assert!(stream.recv().await.is_none());
                    })
                }
                6 => Box::pin({
                    let state = system.probes.clone();
                    let sender = sender.clone();
                    let shutdown = system.shutdown_signal();
                    async move {
                        assert_eq!(
                            state
                                .exchange(
                                    sender,
                                    "target".into(),
                                    false,
                                    Duration::from_secs(10),
                                    shutdown
                                )
                                .await,
                            crate::swim::probe::ProbeOutcome::Unavailable
                        );
                    }
                }),
                _ => unreachable!(),
            };
            park(waiter.as_mut(), &p);
            waiters.push(waiter);
            probes.push(p);
        }
        let cleanup = crate::ingress::unwind::Cleanup::default();
        let result = cleanup.call(|| system.begin_close());
        system.link_gate.clear_poison();
        let mut failures = 0;
        for (waiter, p) in waiters.into_iter().zip(&probes) {
            failures += usize::from(
                cleanup
                    .call(|| {
                        assert!(
                            waiter.now_or_never().is_some(),
                            "shutdown waiter still pending"
                        );
                        check(p);
                    })
                    .is_err(),
            );
        }
        let _ = cleanup.call(|| system.begin_close());
        assert!(result.is_ok(), "notification escaped system shutdown");
        assert_eq!(
            failures, 0,
            "shutdown cohort violated notification contracts"
        );
        system.shutdown().await.unwrap();
    });
}

fn public_waiters(source: u8, throwing: u8) {
    run(async {
        let id = uuid::Uuid::new_v4();
        let system = RemoteActorSystem::new(
            RemoteConfig::mem(format!("close-source-{id}")),
            Arc::new(Empty),
        )
        .unwrap();
        let peer = RemoteActorSystem::new(
            RemoteConfig::mem(format!("close-peer-{id}")),
            Arc::new(Empty),
        )
        .unwrap();
        let address = NodeAddr::mem(&peer.config.node_id);
        let weak = Arc::downgrade(&system);
        let discovery = Arc::new(tokio::sync::Mutex::new(()));
        let discovery_guard = discovery.lock().await;
        system
            .discovery_gates
            .lock()
            .unwrap()
            .insert("discovered".into(), Arc::downgrade(&discovery));
        system
            .nodes
            .reconcile_membership(vec![NodeAddr::mem("discovered").into()]);
        let membership_guard = system.membership.lock().await;
        let lifecycle_guard = if source == 4 {
            Some(system.started.lock().await)
        } else {
            None
        };
        let (tx, _writer) = mpsc::channel(128);
        let sender = FrameSender::new(tx, "admin-peer");
        let status = Arc::new(NodeStatus::default());
        status.set(NodeState::Connected);
        system.links.write().unwrap().push(Link {
            address: NodeAddr::mem("admin-peer"),
            sender,
            status,
            role: crate::handshake::TopologyRole::Normal,
            dial_addr: None,
        });
        let mut waiters: Vec<Pin<Box<dyn Future<Output = ()> + '_>>> = Vec::new();
        let mut probes = Vec::new();
        for req_id in 1..=40 {
            let owner = Arc::downgrade(&system);
            let p = probe(throwing, move || committed(&owner.upgrade().unwrap()));
            let mut waiter: Pin<Box<dyn Future<Output = ()> + '_>> = match source {
                0 => Box::pin(async {
                    assert!(system.start().await.is_err());
                }),
                1 => Box::pin(async {
                    assert!(system.connect(&address).await.is_err());
                }),
                2 => Box::pin(async {
                    assert!(
                        RemoteActorSystem::resolve_sender(&weak, "discovered", false)
                            .await
                            .is_none()
                    );
                }),
                3 => Box::pin({
                    let system = &system;
                    async move {
                        assert!(matches!(system
                            .admin_exchange(
                                "admin-peer",
                                &system.admin_pending,
                                req_id,
                                bytes::Bytes::from_static(b"admin"),
                                Duration::from_secs(30),
                                0
                            )
                            .await,
                            Ok(crate::admin::AdminReply::Failed { req_id: actual, code, detail })
                            if actual == req_id && code == crate::ErrCode::ConnectionLost as u16 && detail == "system shutdown"));
                    }
                }),
                4 => Box::pin(async {
                    system.shutdown().await.unwrap();
                }),
                _ => unreachable!(),
            };
            park(waiter.as_mut(), &p);
            waiters.push(waiter);
            probes.push(p);
        }
        if source == 3 {
            assert_eq!(system.admin_pending.len(), 40);
        }
        let cleanup = crate::ingress::unwind::Cleanup::default();
        let result = if source == 4 {
            cleanup.call(|| drop(lifecycle_guard))
        } else {
            cleanup.call(|| system.begin_close())
        };
        system.link_gate.clear_poison();
        let mut failures = 0;
        for (mut waiter, p) in waiters.into_iter().zip(&probes) {
            // Keep the original observer across cooperative yields and mutex handoffs.
            if cleanup.call(|| check(p)).is_err() {
                failures += 1;
                cleanup.dispose(waiter);
                continue;
            }
            let mut ready = false;
            for _ in 0..128 {
                let waker = std::task::Waker::from(p.clone());
                let polled = cleanup.call(|| {
                    waiter
                        .as_mut()
                        .poll(&mut std::task::Context::from_waker(&waker))
                });
                if matches!(polled, Ok(std::task::Poll::Ready(()))) {
                    ready = true;
                    break;
                }
                if polled.is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            failures += usize::from(!ready);
            failures += usize::from(cleanup.call(|| check(p)).is_err());
            cleanup.dispose(waiter);
        }
        drop((membership_guard, discovery_guard));
        system.link_gate.clear_poison();
        let _ = cleanup.call(|| system.begin_close());
        assert!(result.is_ok(), "notification escaped public shutdown");
        assert_eq!(
            failures, 0,
            "public operation lost notification or completion"
        );
        system.shutdown().await.unwrap();
        peer.shutdown().await.unwrap();
    });
}

macro_rules! public_contracts {
    ($($name:ident: $source:expr, $throwing:expr;)*) => {$ (
        #[test]
        fn $name() {
            isolated_named(concat!("system::shutdown_notification_contracts::", stringify!($name)), || public_waiters($source, $throwing));
        }
    )*};
}
public_contracts! {
    start_waiters: 0, 0;
    start_panicking_waker: 0, 1;
    start_panicking_payload: 0, 2;
    connect_waiters: 1, 0;
    connect_panicking_waker: 1, 1;
    connect_panicking_payload: 1, 2;
    discovery_waiters: 2, 0;
    discovery_panicking_waker: 2, 1;
    discovery_panicking_payload: 2, 2;
    admin_waiters: 3, 0;
    admin_panicking_waker: 3, 1;
    admin_panicking_payload: 3, 2;
    shutdown_waiters: 4, 0;
    shutdown_panicking_waker: 4, 1;
    shutdown_panicking_payload: 4, 2;
}

fn rejected_link(throwing: u8) {
    run(async {
        let system =
            RemoteActorSystem::new(RemoteConfig::tcp("rejected-link", None), Arc::new(Empty))
                .unwrap();
        system.begin_close();
        let (tx, _writer) = mpsc::channel(1);
        let sender = FrameSender::new(tx, "late-peer");
        let owner = Arc::downgrade(&system);
        let p = probe(throwing, move || {
            owner.upgrade().unwrap().link_gate.try_lock().is_ok()
        });
        let mut closed = Box::pin(sender.closed());
        park(closed.as_mut(), &p);
        let (_closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let conn = ConnectionHandle {
            node_id: "late-peer".into(),
            sender: sender.clone(),
            info: crate::transport::ConnectionInfo {
                local: None,
                peer: None,
                scheme: "mem",
                established_at: std::time::Instant::now(),
            },
            peer_role: crate::handshake::TopologyRole::Normal,
            peer_dial_addr: None,
            closed: closed_rx,
        };
        assert!(crate::ingress::unwind::Cleanup::default()
            .call(|| system.register_link(conn))
            .unwrap()
            .is_err());
        assert!(closed.now_or_never().is_some());
        check(&p);
        assert!(system.links.read().unwrap().is_empty());
        system.shutdown().await.unwrap();
    });
}
macro_rules! rejection_contracts {
    ($($name:ident: $throwing:expr;)*) => {$ (
        #[test]
        fn $name() {
            isolated_named(concat!("system::shutdown_notification_contracts::", stringify!($name)), || rejected_link($throwing));
        }
    )*};
}
rejection_contracts! {
    rejected_connection_unlocks_before_notification: 0;
    rejected_connection_panicking_waker: 1;
    rejected_connection_panicking_payload: 2;
}

#[test]
fn runtime_free_close_permanently_rejects_all_callback_entry_points() {
    let system =
        RemoteActorSystem::new(RemoteConfig::tcp("closed-callbacks", None), Arc::new(Empty))
            .unwrap();
    let (tx, _writer) = mpsc::channel(1);
    let sender = FrameSender::new(tx, "peer");
    system.begin_close();
    for id in 1..=40 {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        assert!(system.callbacks.insert(id, "peer", tx).is_err());
        assert!(rx.try_recv().is_err());
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        assert!(system.callbacks.insert_on(id, "peer", tx, &sender).is_err());
        assert!(rx.try_recv().is_err());
        assert!(system.callbacks.register_on(id, "peer", &sender).is_err());
    }
    assert!(system.callbacks.is_empty());
    assert!(system
        .transport
        .listen("127.0.0.1:0".parse().unwrap())
        .now_or_never()
        .unwrap()
        .is_err());
}

#[test]
fn concurrent_shutdown_waits_for_the_close_owner_to_finish_notifications() {
    isolated_named("system::shutdown_notification_contracts::concurrent_shutdown_waits_for_the_close_owner_to_finish_notifications", || {
        run(async {
            use std::task::{Context, Wake, Waker};
            struct BlockingWake {
                entered: std::sync::mpsc::SyncSender<()>,
                release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            }
            impl Wake for BlockingWake {
                fn wake(self: Arc<Self>) { self.wake_by_ref(); }
                fn wake_by_ref(self: &Arc<Self>) {
                    self.entered.send(()).unwrap();
                    self.release.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
                }
            }
            let system = RemoteActorSystem::new(RemoteConfig::tcp("concurrent-close", None), Arc::new(Empty)).unwrap();
            let (tx, mut rx) = tokio::sync::oneshot::channel();
            system.callbacks.insert(1, "peer", tx).unwrap();
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
            let waker = Waker::from(Arc::new(BlockingWake { entered: entered_tx, release: std::sync::Mutex::new(release_rx) }));
            assert!(Pin::new(&mut rx).poll(&mut Context::from_waker(&waker)).is_pending());
            let owner = system.clone();
            let thread = std::thread::spawn(move || owner.begin_close());
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut shutdown = Box::pin(system.shutdown());
            let unlocked = system.link_gate.try_lock().is_ok();
            let pending = unlocked && shutdown.as_mut().poll(&mut Context::from_waker(futures::task::noop_waker_ref())).is_pending();
            release_tx.send(()).unwrap();
            thread.join().unwrap();
            assert!(unlocked, "close callback held the system gate");
            assert!(pending, "concurrent shutdown completed before notification ownership retired");
            shutdown.await.unwrap();
            assert!(matches!(rx.try_recv().unwrap(), crate::registry::ReplyPayload::Err(crate::ErrCode::ConnectionLost, _)));
        });
    });
}

macro_rules! contracts {
    ($($name:ident: $source:expr, $throwing:expr;)*) => {$ (
        #[test]
        fn $name() {
            isolated_named(concat!("system::shutdown_notification_contracts::", stringify!($name)), || cohort($source, $throwing));
        }
    )*};
}
contracts! {
    watch_commit: 0, 0;
    watch_panicking_waker: 0, 1;
    watch_panicking_payload: 0, 2;
    background_commit: 1, 0;
    background_panicking_waker: 1, 1;
    background_panicking_payload: 1, 2;
    callbacks_commit: 2, 0;
    callbacks_panicking_waker: 2, 1;
    callbacks_panicking_payload: 2, 2;
    links_commit: 3, 0;
    links_panicking_waker: 3, 1;
    links_panicking_payload: 3, 2;
    mirrors_commit: 4, 0;
    mirrors_panicking_waker: 4, 1;
    mirrors_panicking_payload: 4, 2;
    receptionist_commit: 5, 0;
    receptionist_panicking_waker: 5, 1;
    receptionist_panicking_payload: 5, 2;
    probes_commit: 6, 0;
    probes_panicking_waker: 6, 1;
    probes_panicking_payload: 6, 2;
}
