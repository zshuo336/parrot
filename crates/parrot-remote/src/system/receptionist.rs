//! Source-bound discovery uses the system's owned lifetime and verified link identity.
pub(super) mod routes;
pub(super) mod segments;
use super::{MembershipOwner, RemoteActorSystem};
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
            let mut outgoing = segments::Outgoing::default();
            let mut send_cursor = 0usize;
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let mut retired_assemblies = segments::RetiredAssemblies::default();
                let Some(owner) = MembershipOwner::acquire(&weak, &membership).await else { break };
                let system = &owner.system;
                let members = &owner.members;
                let gate = system.link_gate.lock().unwrap();
                if system.is_closing() { break; }
                let senders = current_senders(system);
                retired_assemblies.extend(system.receptionist_assemblies.lock().unwrap().prune(std::time::Instant::now()));
                let available: HashSet<_> = members
                    .members
                    .values()
                    .filter(|member| member.status != MemberStatus::Dead)
                    .map(|member| member.node_id.clone())
                    .collect();
                let (snapshot, routed, notifications) = {
                    let mut routes = system.receptionist_routes.lock().unwrap();
                    let mut table = system.receptionist.lock().unwrap();
                    let mut notifications = Notifications::default();
                    for event in routes.prune(&senders.iter().map(FrameSender::dispatch_key).collect(), &available, &mut table) {
                        notifications.extend(table.broadcast_deferred(&event));
                    }
                    for event in table.remove_unavailable(&system.config.node_id, &available) {
                        notifications.extend(table.broadcast_deferred(&event));
                    }
                    let snapshot = ReceptionistSync {
                        from: system.config.node_id.clone(),
                        entries: table.snapshot(&system.config.node_id),
                    };
                    let routed = routes.snapshot(snapshot.clone(), "");
                    (snapshot, routed, notifications)
                };
                drop(gate);
                let system = owner.release();
                drop(retired_assemblies);
                if let Err(error) = notifications.dispatch() {
                    super::diagnostic(|| tracing::warn!(%error, "receptionist retirement notification failed"));
                }
                let payload = encode_sync(&snapshot);
                let mut legacy_payload = None;
                let mut senders = senders;
                if !senders.is_empty() {
                    let count = senders.len();
                    senders.rotate_left(send_cursor % count);
                    send_cursor = send_cursor.wrapping_add(1);
                }
                outgoing.prune(&senders.iter().map(FrameSender::dispatch_key).collect());
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
                        let routed_payload;
                        let payload = if sender.peer_capabilities() & caps::RECEPTIONIST_ROUTES != 0 {
                            let mut projected = routed.clone();
                            projected.views.retain(|view| !view.path.iter().any(|node| node == sender.node_id()));
                            if sender.peer_capabilities() & caps::RECEPTIONIST_NATIVE_PATHS == 0 {
                                for view in &mut projected.views {
                                    view.entries.retain(|entry| crate::mirror::remote_shadow(&entry.remote_path).is_some());
                                }
                                projected.views.retain(|view| !view.entries.is_empty());
                            }
                            routed_payload = routes::encode(&projected);
                            &routed_payload
                        } else {
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
                            }
                        };
                        if let Err(error) = outgoing.send(&sender, payload) {
                            super::diagnostic(|| tracing::debug!(node = sender.node_id(), %error, "receptionist synchronization deferred"));
                        }
                    }
                }
            }
        });
    }
}

fn current_senders(system: &RemoteActorSystem) -> Vec<FrameSender> {
    let mut seen = HashSet::new();
    system
        .links
        .read()
        .unwrap()
        .iter()
        .rev()
        .filter(|link| !link.sender.is_closed() && seen.insert(link.node_id().to_owned()))
        .map(|link| link.sender.clone())
        .collect()
}

pub(super) async fn receive(weak: Weak<RemoteActorSystem>, body: &[u8], back: &FrameSender) {
    let Ok(sync) = decode_sync(body) else {
        back.close();
        return;
    };
    if sync.from != back.node_id() {
        back.close();
        return;
    }
    receive_views(
        weak,
        vec![routes::Advertisement {
            path: vec![sync.from],
            entries: sync.entries,
        }],
        back,
        false,
    )
    .await;
}

pub(super) async fn receive_routes(weak: Weak<RemoteActorSystem>, body: &[u8], back: &FrameSender) {
    let decoded: Result<routes::Sync, _> = parrot_api::message::serde_remote_deserialize(body);
    let Ok(sync) = decoded else {
        back.close();
        return;
    };
    if sync.from != back.node_id()
        || back.peer_capabilities() & caps::RECEPTIONIST_ROUTES == 0
        || sync.views.len() > routes::MAX_VIEWS
    {
        back.close();
        return;
    }
    receive_views(weak, sync.views, back, true).await;
}

fn project_views(
    system: &RemoteActorSystem,
    views: Vec<routes::Advertisement>,
    back: &FrameSender,
    routed: bool,
) -> Result<Vec<routes::Advertisement>, ()> {
    let mut owners = HashSet::new();
    let mut keys = HashSet::new();
    let mut result = Vec::new();
    for view in views {
        if view.path.is_empty()
            || view.path.len() > routes::MAX_HOPS
            || view.path.last().is_none_or(|node| node != back.node_id())
            || view.path.iter().any(|node| {
                node == &system.config.node_id
                    || parrot_config::endpoint::validate_node_id(node).is_err()
            })
            || view.path.iter().collect::<HashSet<_>>().len() != view.path.len()
        {
            return Err(());
        }
        let owner = view.path[0].clone();
        owners.insert(owner.clone());
        if owners.len() > routes::MAX_ORIGINS {
            return Err(());
        }
        if !routed && owner != back.node_id() {
            return Err(());
        }
        let mut grouped: std::collections::BTreeMap<_, Vec<_>> = std::collections::BTreeMap::new();
        let mut unique = HashSet::new();
        for entry in view.entries {
            if entry.node != owner
                || crate::receptionist::validate_event(&entry.key, &entry.remote_path).is_err()
                || crate::actor_path::split(&entry.remote_path).is_none_or(|(node, local)| {
                    node != owner
                        || (local.starts_with("actix://")
                            && back.peer_capabilities() & caps::RECEPTIONIST_NATIVE_PATHS == 0)
                })
            {
                return Err(());
            }
            let allowed = system
                .config
                .receptionist_acl
                .check(&owner, entry.key.as_str())
                == AclDecision::Allow;
            if owner == back.node_id() && !allowed {
                return Err(());
            }
            if !allowed
                || (owner != back.node_id()
                    && system
                        .config
                        .receptionist_relay_acl
                        .check(back.node_id(), entry.key.as_str())
                        != AclDecision::Allow)
            {
                continue;
            }
            if unique.insert(entry.clone()) {
                grouped
                    .entry(entry.key.as_str().to_owned())
                    .or_default()
                    .push(entry);
            }
        }
        for (key, entries) in grouped {
            if !keys.insert((owner.clone(), key)) {
                return Err(());
            }
            result.push(routes::Advertisement {
                path: view.path.clone(),
                entries,
            });
            if result.len() > routes::MAX_VIEWS {
                return Err(());
            }
        }
    }
    Ok(result)
}

async fn receive_views(
    weak: Weak<RemoteActorSystem>,
    views: Vec<routes::Advertisement>,
    back: &FrameSender,
    routed: bool,
) {
    let membership = {
        let Some(system) = weak.upgrade() else { return };
        system.membership.clone()
    };
    let Some(owner) = MembershipOwner::acquire(&weak, &membership).await else {
        return;
    };
    let system = &owner.system;
    let members = &owner.members;
    let projected = project_views(system, views, back, routed);
    if !system.config.security.control_policy().allows_cluster(back)
        || back.peer_capabilities() & caps::RECEPTIONIST_SYNC == 0
        || projected.is_err()
    {
        let _system = owner.release();
        back.close();
        return;
    }
    // Membership removal and snapshot merge are serialized under the same lock;
    // queued frames from dead or superseded connections cannot revive old services.
    if members
        .members
        .get(back.node_id())
        .is_none_or(|member| member.status == MemberStatus::Dead)
    {
        return;
    }
    let _gate = system.link_gate.lock().unwrap();
    if system.is_closing()
        || system
            .direct_sender_of(back.node_id())
            .is_none_or(|current| !current.same_connection(back))
    {
        return;
    }
    let available: HashSet<_> = members
        .members
        .values()
        .filter(|member| member.status != MemberStatus::Dead)
        .map(|member| member.node_id.clone())
        .collect();
    let mut views = projected.unwrap();
    views.retain(|view| view.path.iter().all(|node| available.contains(node)));
    let senders = current_senders(system);
    let current = senders.iter().map(FrameSender::dispatch_key).collect();
    system.reconcile_membership_routes(members);
    let mut routes = system.receptionist_routes.lock().unwrap();
    let mut table = system.receptionist.lock().unwrap();
    system.receptionist_assemblies.lock().unwrap().discard(back);
    let mut events = routes.prune(&current, &available, &mut table);
    let replacement = routes.replace(back, views, &mut table);
    let admission_error = replacement.as_ref().err().map(ToString::to_string);
    events.extend(replacement.unwrap_or_default());
    let mut notifications = Notifications::default();
    for event in events {
        notifications.extend(table.broadcast_deferred(&event));
    }
    drop(table);
    drop(routes);
    drop(_gate);
    let _system = owner.release();
    if let Some(error) = admission_error {
        super::diagnostic(
            || tracing::debug!(node = back.node_id(), %error, "receptionist snapshot admission deferred"),
        );
    }
    if let Err(error) = notifications.dispatch() {
        super::diagnostic(|| tracing::warn!(%error, "receptionist snapshot notification failed"));
    }
}

pub(super) async fn receive_segment(
    weak: Weak<RemoteActorSystem>,
    body: &[u8],
    back: &FrameSender,
    routed: bool,
) {
    // Keep retired sender ownership outside every table, membership and link guard.
    let mut retired_assemblies = segments::RetiredAssemblies::default();
    let membership = {
        let Some(system) = weak.upgrade() else { return };
        system.membership.clone()
    };
    let Some(owner) = MembershipOwner::acquire(&weak, &membership).await else {
        return;
    };
    let system = &owner.system;
    let members = &owner.members;
    if back.node_id() == system.config.node_id
        || !back.supports_receptionist_segments()
        || back.peer_capabilities() & caps::RECEPTIONIST_SYNC == 0
        || !system.config.security.control_policy().allows_cluster(back)
        || (routed && back.peer_capabilities() & caps::RECEPTIONIST_ROUTES == 0)
    {
        let _system = owner.release();
        back.close();
        return;
    }
    let gate = system.link_gate.lock().unwrap();
    if system.is_closing()
        || members
            .members
            .get(back.node_id())
            .is_none_or(|member| member.status == MemberStatus::Dead)
        || system
            .direct_sender_of(back.node_id())
            .is_none_or(|current| !current.same_connection(back))
    {
        return;
    }
    let result = system
        .receptionist_assemblies
        .lock()
        .unwrap()
        .push_deferred(
            back,
            body,
            std::time::Instant::now(),
            routed,
            &mut retired_assemblies,
        );
    drop(gate);
    drop(owner);
    drop(retired_assemblies);
    match result {
        Ok(Some(body)) if routed => receive_routes(weak, &body, back).await,
        Ok(Some(body)) => receive(weak, &body, back).await,
        Ok(None) => {}
        Err(_) => back.close(),
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
        snapshot_wakes().await;
    }

    async fn snapshot_wakes() {
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
        let original = a
            .direct_sender_of(&b.config.node_id)
            .expect("snapshot diagnostics must preserve the original TCP connection");
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
        let preserved = !original.is_closed()
            && a.direct_sender_of(&b.config.node_id)
                .is_some_and(|current| current.same_connection(&original));
        b.shutdown().await.unwrap();
        a.shutdown().await.unwrap();
        assert!(
            preserved,
            "snapshot diagnostics closed or replaced its connection"
        );
    }

    #[tokio::test]
    async fn periodic_retirement_releases_membership_and_table_before_every_wake() {
        periodic_retirement().await;
    }

    async fn periodic_retirement() {
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

    macro_rules! diagnostic_contract {
        ($name:ident, $throwing_payload:expr, $exercise:ident) => {
            #[tokio::test]
            async fn $name() {
                super::super::diagnostic_contracts::isolated(
                    concat!(module_path!(), "::", stringify!($name))
                        .strip_prefix("parrot_remote::")
                        .unwrap(),
                    &["parrot_remote::system::receptionist"],
                    $throwing_payload,
                    1,
                    $exercise(),
                )
                .await;
            }
        };
    }
    diagnostic_contract!(
        snapshot_diagnostic_panic_preserves_sync,
        false,
        snapshot_wakes
    );
    diagnostic_contract!(
        snapshot_diagnostic_payload_panic_preserves_sync,
        true,
        snapshot_wakes
    );
    diagnostic_contract!(
        retirement_diagnostic_panic_preserves_sync,
        false,
        periodic_retirement
    );
    diagnostic_contract!(
        retirement_diagnostic_payload_panic_preserves_sync,
        true,
        periodic_retirement
    );
}
