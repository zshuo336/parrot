//! Direct-peer discovery uses the system's owned lifetime and verified link identity.
use super::RemoteActorSystem;
use crate::{
    acl::AclDecision,
    frame::{frame_type, Frame, FrameHeader, PROTOCOL_VERSION, SEQ_NONE},
    handshake::caps,
    receptionist::{decode_sync, encode_sync, Notifications, ReceptionistFacade, ReceptionistSync},
    swim::MemberStatus,
    transport::FrameSender,
};
use std::{
    collections::HashSet,
    sync::{Arc, Weak},
    time::Duration,
};

fn frame(payload: bytes::Bytes) -> Frame {
    Frame {
        header: FrameHeader {
            frame_len: 0,
            version: PROTOCOL_VERSION,
            frame_type: frame_type::SYSTEM_EVENT,
            flags: 0,
            correlation_id: 0,
            hop_count: 0,
            hop_limit: 1,
            seq: SEQ_NONE,
        },
        path: String::new(),
        type_key: String::new(),
        payload,
    }
}

impl RemoteActorSystem {
    /// Shared local/network table, fenced by this system's shutdown. The configured
    /// receptionist ACL applies to this node's operations and incoming peer keys.
    /// Local actor paths are qualified with this system's verified node identity.
    pub fn receptionist_gateway(self: &Arc<Self>) -> ReceptionistFacade {
        ReceptionistFacade::for_system(self)
    }

    pub(super) fn start_receptionist(self: &Arc<Self>) {
        if !self.config.membership_gossip_enabled() {
            return;
        }
        let weak = Arc::downgrade(self);
        let membership = self.membership.clone();
        let period = self
            .config
            .swim
            .gossip_interval
            .max(Duration::from_millis(1));
        self.spawn_background(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                // Do not retain the system while waiting on its public membership lock.
                let members = membership.lock().await;
                let Some(system) = weak.upgrade() else { break };
                let available: HashSet<_> = members
                    .members
                    .values()
                    .filter(|member| member.status != MemberStatus::Dead)
                    .map(|member| member.node_id.clone())
                    .collect();
                let (snapshot, notifications) = {
                    let mut table = system.receptionist.lock().unwrap();
                    let mut notifications = Notifications::default();
                    for event in table.remove_unavailable(&system.config.node_id, &available) {
                        notifications.extend(table.broadcast_deferred(&event));
                    }
                    let snapshot = ReceptionistSync {
                        from: system.config.node_id.clone(),
                        entries: table.snapshot(&system.config.node_id),
                    };
                    (snapshot, notifications)
                };
                drop(members);
                if let Err(error) = notifications.dispatch() {
                    tracing::warn!(%error, "receptionist retirement notification failed");
                }
                let payload = encode_sync(&snapshot);
                let mut legacy_payload = None;
                let mut seen = HashSet::new();
                let senders: Vec<_> = system
                    .links
                    .read()
                    .unwrap()
                    .iter()
                    .rev()
                    .filter(|(node, sender, _)| !sender.is_closed() && seen.insert(node.clone()))
                    .map(|(_, sender, _)| sender.clone())
                    .collect();
                for sender in senders {
                    if system
                        .config
                        .security
                        .control_policy()
                        .allows_cluster(&sender)
                        && sender.peer_capabilities() & caps::RECEPTIONIST_SYNC != 0
                    {
                        // A bounded full queue drops this attempt. The next periodic
                        // complete snapshot repairs lost registration AND withdrawal.
                        let payload =
                            if sender.peer_capabilities() & caps::RECEPTIONIST_NATIVE_PATHS != 0 {
                                &payload
                            } else {
                                legacy_payload.get_or_insert_with(|| {
                                    encode_sync(&ReceptionistSync {
                                        from: snapshot.from.clone(),
                                        entries: snapshot
                                            .entries
                                            .iter()
                                            .filter(|entry| {
                                                crate::mirror::remote_shadow(&entry.remote_path)
                                                    .is_some()
                                            })
                                            .cloned()
                                            .collect(),
                                    })
                                })
                            };
                        let _ = sender.send_frame_sync(frame(payload.clone()));
                    }
                }
            }
        });
    }
}

pub(super) async fn receive(weak: Weak<RemoteActorSystem>, body: &[u8], back: &FrameSender) {
    let Ok(sync) = decode_sync(body) else {
        back.close();
        return;
    };
    let membership = {
        let Some(system) = weak.upgrade() else { return };
        system.membership.clone()
    };
    let members = membership.lock().await;
    let Some(system) = weak.upgrade() else { return };
    if sync.from != back.node_id()
        || sync.from == system.config.node_id
        || !system.config.security.control_policy().allows_cluster(back)
        || back.peer_capabilities() & caps::RECEPTIONIST_SYNC == 0
        || sync.entries.iter().any(|entry| {
            entry.node != sync.from
                || crate::actor_path::split(&entry.remote_path).is_none_or(|(node, local)| {
                    node != sync.from
                        || (local.starts_with("actix://")
                            && back.peer_capabilities() & caps::RECEPTIONIST_NATIVE_PATHS == 0)
                })
                || system
                    .config
                    .receptionist_acl
                    .check(&sync.from, entry.key.as_str())
                    != AclDecision::Allow
        })
    {
        back.close();
        return;
    }
    // Membership removal and snapshot merge are serialized under the same lock;
    // queued frames from dead or superseded connections cannot revive old services.
    if members
        .members
        .get(&sync.from)
        .is_none_or(|member| member.status == MemberStatus::Dead)
    {
        return;
    }
    let _gate = system.link_gate.lock().unwrap();
    if *system.shutdown_tx.borrow()
        || system
            .direct_sender_of(&sync.from)
            .is_none_or(|current| !current.same_connection(back))
    {
        return;
    }
    let mut table = system.receptionist.lock().unwrap();
    let mut notifications = Notifications::default();
    for event in table.merge_sync(&sync) {
        notifications.extend(table.broadcast_deferred(&event));
    }
    drop(table);
    drop(_gate);
    drop(members);
    if let Err(error) = notifications.dispatch() {
        tracing::warn!(%error, "receptionist snapshot notification failed");
    }
}

#[cfg(test)]
mod wake_boundary_network_contracts {
    use super::*;
    use crate::{acl::AclRules, LocalLookup, NodeAddr, RemoteConfig};
    use parrot_api::receptionist::{
        ReceptionistEvent, ReceptionistGateway, ReceptionistKey, ReceptionistStream,
    };
    use parrot_api::types::BoxedActorRef;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Wake, Waker};

    struct Empty;

    #[async_trait::async_trait]
    impl LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
            None
        }
    }

    struct Locks {
        system: Weak<RemoteActorSystem>,
        calls: AtomicUsize,
        table: AtomicBool,
        members: AtomicBool,
        gate: AtomicBool,
        panic_once: AtomicBool,
    }

    impl Wake for Locks {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            if let Some(system) = self.system.upgrade() {
                self.table
                    .fetch_or(system.receptionist.try_lock().is_err(), Ordering::SeqCst);
                self.members
                    .fetch_or(system.membership.try_lock().is_err(), Ordering::SeqCst);
                self.gate
                    .fetch_or(system.link_gate.try_lock().is_err(), Ordering::SeqCst);
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.panic_once.swap(false, Ordering::SeqCst),
                "controlled network wake panic"
            );
        }
    }

    fn key() -> ReceptionistKey {
        ReceptionistKey::new("edge/network-wake").unwrap()
    }

    fn node(name: &str) -> Arc<RemoteActorSystem> {
        let config = RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development()
            .with_receptionist_acl(AclRules::permissive())
            .with_swim_config(crate::swim::SwimConfig {
                gossip_interval: Duration::from_millis(5),
                ..Default::default()
            });
        RemoteActorSystem::new(config, Arc::new(Empty)).unwrap()
    }

    fn probe(system: &Arc<RemoteActorSystem>, panic_once: bool) -> Arc<Locks> {
        Arc::new(Locks {
            system: Arc::downgrade(system),
            calls: AtomicUsize::new(0),
            table: AtomicBool::new(false),
            members: AtomicBool::new(false),
            gate: AtomicBool::new(false),
            panic_once: AtomicBool::new(panic_once),
        })
    }

    fn park(stream: &mut ReceptionistStream, probe: &Arc<Locks>) {
        let waker = Waker::from(probe.clone());
        assert!(stream
            .poll_recv(&mut Context::from_waker(&waker))
            .is_pending());
    }

    fn unlocked(probe: &Locks) {
        assert!(
            !probe.table.load(Ordering::SeqCst),
            "network callback held table lock"
        );
        assert!(
            !probe.members.load(Ordering::SeqCst),
            "network callback held membership lock"
        );
        assert!(
            !probe.gate.load(Ordering::SeqCst),
            "network callback held connection gate"
        );
    }

    async fn observed(probe: &Locks, calls: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while probe.calls.load(Ordering::SeqCst) < calls {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("real network notification did not arrive");
    }

    #[tokio::test]
    async fn real_tcp_snapshot_wakes_after_all_outer_locks_and_survives_callback_panic() {
        let a = node("wake-net-a");
        let b = node("wake-net-b");
        a.start().await.unwrap();
        b.start().await.unwrap();
        let mut first = a.receptionist_gateway().subscribe(key()).await.unwrap();
        let mut sibling = a.receptionist_gateway().subscribe(key()).await.unwrap();
        let throwing = probe(&a, true);
        let healthy = probe(&a, false);
        park(&mut first, &throwing);
        park(&mut sibling, &healthy);
        b.receptionist_gateway()
            .register(key(), "/user/first".into())
            .await
            .unwrap();
        a.connect(&NodeAddr {
            node_id: b.config.node_id.clone(),
            identity_is_explicit: true,
            scheme: "tcp".into(),
            addr: b.local_addr().unwrap(),
        })
        .await
        .unwrap();
        observed(&throwing, 1).await;
        observed(&healthy, 1).await;
        for probe in [&throwing, &healthy] {
            unlocked(probe);
        }
        assert!(!a.receptionist.is_poisoned());
        let expected = ReceptionistEvent::Registered {
            key: key(),
            remote_path: "parrot://wake-net-b/user/first".into(),
        };
        assert_eq!(first.try_recv().unwrap(), expected);
        assert_eq!(sibling.try_recv().unwrap(), expected);
        park(&mut first, &throwing);
        park(&mut sibling, &healthy);
        b.receptionist_gateway()
            .deregister(key(), "/user/first".into())
            .await
            .unwrap();
        observed(&throwing, 2).await;
        observed(&healthy, 2).await;
        let expected = ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: "parrot://wake-net-b/user/first".into(),
        };
        assert_eq!(first.try_recv().unwrap(), expected);
        assert_eq!(sibling.try_recv().unwrap(), expected);
        for probe in [&throwing, &healthy] {
            unlocked(probe);
        }
        b.shutdown().await.unwrap();
        a.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn periodic_retirement_releases_membership_and_table_before_every_wake() {
        let system = node("wake-retirement");
        system.receptionist.lock().unwrap().register(
            "departed",
            &key(),
            "parrot://departed/user/service",
        );
        let mut first = system
            .receptionist_gateway()
            .subscribe(key())
            .await
            .unwrap();
        let mut sibling = system
            .receptionist_gateway()
            .subscribe(key())
            .await
            .unwrap();
        assert!(matches!(
            first.try_recv(),
            Ok(ReceptionistEvent::Registered { .. })
        ));
        assert!(matches!(
            sibling.try_recv(),
            Ok(ReceptionistEvent::Registered { .. })
        ));
        let throwing = probe(&system, true);
        let healthy = probe(&system, false);
        park(&mut first, &throwing);
        park(&mut sibling, &healthy);
        system.start().await.unwrap();
        observed(&throwing, 1).await;
        observed(&healthy, 1).await;
        for probe in [&throwing, &healthy] {
            unlocked(probe);
        }
        let expected = ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: "parrot://departed/user/service".into(),
        };
        assert_eq!(first.try_recv().unwrap(), expected);
        assert_eq!(sibling.try_recv().unwrap(), expected);
        assert!(!system.receptionist.is_poisoned());
        assert!(system
            .receptionist
            .lock()
            .unwrap()
            .snapshot("departed")
            .is_empty());
        system.receptionist.lock().unwrap().register(
            "departed-again",
            &key(),
            "parrot://departed-again/user/service",
        );
        park(&mut first, &throwing);
        park(&mut sibling, &healthy);
        observed(&throwing, 2).await;
        observed(&healthy, 2).await;
        for probe in [&throwing, &healthy] {
            unlocked(probe);
        }
        let expected = ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: "parrot://departed-again/user/service".into(),
        };
        assert_eq!(first.try_recv().unwrap(), expected);
        assert_eq!(sibling.try_recv().unwrap(), expected);
        assert!(system
            .receptionist
            .lock()
            .unwrap()
            .snapshot("departed-again")
            .is_empty());
        system
            .receptionist_gateway()
            .register(key(), "/user/live".into())
            .await
            .unwrap();
        assert!(matches!(
            first.try_recv(),
            Ok(ReceptionistEvent::Registered { .. })
        ));
        system.shutdown().await.unwrap();
    }
}
