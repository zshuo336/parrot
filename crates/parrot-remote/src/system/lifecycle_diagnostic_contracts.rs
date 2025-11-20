use super::*;
use futures::FutureExt;
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex, Weak,
    },
};

#[derive(Default)]
struct Fields {
    message: String,
    node: String,
}
impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            "node" => self.node = format!("{value:?}").trim_matches('"').to_owned(),
            _ => {}
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_owned(),
            "node" => self.node = value.to_owned(),
            _ => {}
        }
    }
}

pub(crate) struct Probe {
    message: &'static str,
    pub(crate) armed: AtomicBool,
    pub(crate) calls: AtomicUsize,
    pub(crate) seen: tokio::sync::Notify,
    subject: Mutex<(Weak<RemoteActorSystem>, Option<String>, bool)>,
    retiring: Mutex<Option<FrameSender>>,
    locked: AtomicBool,
    premature: AtomicBool,
    reentered: AtomicUsize,
}
impl Probe {
    pub(crate) fn new(message: &'static str) -> Arc<Self> {
        Arc::new(Self {
            message,
            armed: false.into(),
            calls: 0.into(),
            seen: Default::default(),
            subject: Mutex::new((Weak::new(), None, false)),
            retiring: Mutex::new(None),
            locked: false.into(),
            premature: false.into(),
            reentered: 0.into(),
        })
    }
    fn watch(&self, system: &Arc<RemoteActorSystem>, node: Option<&str>, reenter: bool) {
        *self.subject.lock().unwrap() = (Arc::downgrade(system), node.map(str::to_owned), reenter);
        self.armed.store(true, Ordering::SeqCst);
    }
    fn observe(&self, event: &tracing::Event<'_>) -> bool {
        if !self.armed.load(Ordering::SeqCst) {
            return false;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let (subject, node, reenter) = self.subject.lock().unwrap().clone();
        if !fields.message.starts_with(self.message)
            || node.as_ref().is_some_and(|node| *node != fields.node)
        {
            return false;
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.notify_one();
        if let Some(system) = subject.upgrade() {
            let unlocked = system.link_gate.try_lock().is_ok()
                && system.links.try_write().is_ok()
                && system.uplink.try_write().is_ok()
                && system.dial_addrs.try_lock().is_ok();
            self.locked.fetch_or(!unlocked, Ordering::SeqCst);
            if unlocked
                && matches!(
                    self.message,
                    "connection closed (register_link watcher)" | "uplink cleared"
                )
            {
                let links = system.links.read().unwrap();
                let current = links
                    .iter()
                    .any(|link| link.node_id() == fields.node && !link.sender.is_closed());
                let retired = !links.iter().any(|link| {
                    self.retiring
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|old| old.same_connection(&link.sender))
                }) && system.nodes.get(&fields.node).is_none_or(|(_, status)| {
                    status.get()
                        == if current {
                            NodeState::Connected
                        } else {
                            NodeState::Disconnected
                        }
                }) && (current
                    || !system.dial_addrs.lock().unwrap().contains_key(&fields.node));
                self.premature.fetch_or(!retired, Ordering::SeqCst);
            }
            if unlocked && reenter {
                system.begin_close();
                self.reentered.fetch_add(1, Ordering::SeqCst);
            }
        }
        true
    }
    pub(crate) async fn wait(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.calls.load(Ordering::SeqCst) == 0 {
                self.seen.notified().await;
            }
        })
        .await
        .expect("production diagnostic did not execute");
    }
    fn check(&self, reenter: bool) {
        assert_eq!(self.calls.load(Ordering::SeqCst), 1);
        assert!(
            !self.locked.load(Ordering::SeqCst),
            "diagnostic held lifecycle state lock"
        );
        assert_eq!(self.reentered.load(Ordering::SeqCst), usize::from(reenter));
        assert!(
            !self.premature.load(Ordering::SeqCst),
            "diagnostic ran before local connection retirement committed"
        );
    }
}

pub(crate) async fn isolated(
    name: &str,
    target: &'static [&'static str],
    payload: bool,
    probe: Arc<Probe>,
    exercise: impl std::future::Future<Output = ()>,
) {
    let observed = probe.clone();
    super::diagnostic_contracts::isolated_observed(
        name,
        target,
        payload,
        1,
        Some(Arc::new(move |event| observed.observe(event))),
        exercise,
    )
    .await;
}

#[derive(Debug)]
struct Echo;
impl ActorRef for Echo {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(msg)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/echo".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
pub(crate) struct Lookup;
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/echo").then(|| Box::new(Echo) as BoxedActorRef)
    }
}
pub(crate) async fn echo(system: &Arc<RemoteActorSystem>, node: &str) {
    let reference = system.remote_ref(&format!("parrot://{node}/echo")).unwrap();
    let result = reference
        .send_with_timeout(
            Box::new(crate::ingress::TestAsk(19)),
            Some(Duration::from_secs(2)),
        )
        .await
        .unwrap();
    assert_eq!(
        result.downcast_ref::<crate::ingress::TestAsk>().unwrap().0,
        19
    );
}
fn config(id: &str, tcp: bool) -> RemoteConfig {
    let mut config = if tcp {
        RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap()))
    } else {
        RemoteConfig::mem(id)
    };
    config.heartbeat_enabled = false;
    config
}
async fn until(check: impl Fn() -> bool, detail: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !check() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect(detail);
}

async fn registration(probe: Arc<Probe>, reenter: bool) {
    let client =
        RemoteActorSystem::new(config("diagnostic-client", false), Arc::new(Lookup)).unwrap();
    let mut hub_config = config("diagnostic-hub", false);
    hub_config.topology_role = crate::TopologyRole::Hub;
    hub_config.direct_addr = Some("127.0.0.1:7999".into());
    let hub = RemoteActorSystem::new(hub_config, Arc::new(Lookup)).unwrap();
    hub.start().await.unwrap();
    client.start().await.unwrap();
    probe.watch(&client, Some("diagnostic-hub"), reenter);
    let result = AssertUnwindSafe(client.connect(&NodeAddr::mem("diagnostic-hub")))
        .catch_unwind()
        .await;
    assert!(
        !client.link_gate.is_poisoned(),
        "registration diagnostic poisoned connection gate"
    );
    assert!(
        !client.uplink.is_poisoned(),
        "registration diagnostic poisoned uplink"
    );
    result
        .expect("registration diagnostic escaped connect")
        .unwrap();
    probe.check(reenter);
    if reenter {
        assert!(*client.shutdown_tx.borrow());
        assert!(client.links.read().unwrap().is_empty());
        assert!(client.uplink.read().unwrap().is_none());
    } else {
        let sender = client.direct_sender_of("diagnostic-hub").unwrap();
        assert!(client
            .uplink
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .same_connection(&sender));
        assert_eq!(
            client.nodes.get("diagnostic-hub").unwrap().1.get(),
            NodeState::Connected
        );
        assert_eq!(
            client
                .dial_addrs
                .lock()
                .unwrap()
                .get("diagnostic-hub")
                .unwrap(),
            "127.0.0.1:7999"
        );
        echo(&client, "diagnostic-hub").await;
    }
    probe.armed.store(false, Ordering::SeqCst);
    client.shutdown().await.unwrap();
    hub.shutdown().await.unwrap();
}

async fn retirement(probe: Arc<Probe>, uplink: bool, reenter: bool) {
    let client =
        RemoteActorSystem::new(config("diagnostic-client", false), Arc::new(Lookup)).unwrap();
    let mut peer_config = config("diagnostic-peer", false);
    if uplink {
        peer_config.topology_role = crate::TopologyRole::Hub;
    }
    peer_config.direct_addr = Some("127.0.0.1:7998".into());
    let peer = RemoteActorSystem::new(peer_config, Arc::new(Lookup)).unwrap();
    peer.start().await.unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::mem("diagnostic-peer"))
        .await
        .unwrap();
    let (sender, status) = {
        let links = client.links.read().unwrap();
        (links[0].sender.clone(), links[0].status.clone())
    };
    let node_status = client.nodes.get("diagnostic-peer").unwrap().1;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client
                .membership
                .lock()
                .await
                .members
                .contains_key("diagnostic-peer")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("registered membership must exist before testing watcher progress");
    assert_eq!(
        client.membership.lock().await.members["diagnostic-peer"].status,
        crate::swim::MemberStatus::Alive
    );
    *probe.retiring.lock().unwrap() = Some(sender.clone());
    probe.watch(&client, Some("diagnostic-peer"), reenter);
    sender.close();
    probe.wait().await;
    until(
        || client.links.read().unwrap().is_empty(),
        "diagnostic prevented old physical link retirement",
    )
    .await;
    probe.check(reenter);
    assert_eq!(status.get(), NodeState::Disconnected);
    assert_eq!(node_status.get(), NodeState::Disconnected);
    assert!(client.uplink.read().unwrap().is_none());
    assert!(!client
        .dial_addrs
        .lock()
        .unwrap()
        .contains_key("diagnostic-peer"));
    assert!(!client.link_gate.is_poisoned());
    if !reenter {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let members = client.membership.lock().await;
                let member = &members.members["diagnostic-peer"];
                if member.status == crate::swim::MemberStatus::Suspect {
                    assert_eq!(
                        member.status_until_ms,
                        members
                            .now()
                            .saturating_add(client.config.swim.suspect_timeout.as_millis() as u64)
                    );
                    break;
                }
                drop(members);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("diagnostic interrupted watcher membership suspicion");
    }
    probe.armed.store(false, Ordering::SeqCst);
    if !reenter {
        client
            .connect(&NodeAddr::mem("diagnostic-peer"))
            .await
            .unwrap();
        assert!(!client
            .direct_sender_of("diagnostic-peer")
            .unwrap()
            .same_connection(&sender));
        echo(&client, "diagnostic-peer").await;
    }
    client.shutdown().await.unwrap();
    peer.shutdown().await.unwrap();
}

async fn accepting(probe: Arc<Probe>) {
    use tokio::io::AsyncWriteExt;
    let server = RemoteActorSystem::new(
        config("diagnostic-server", true).with_knobs(crate::transport::RuntimeKnobs {
            handshake_capacity: 1,
            heartbeat_enabled: false,
            ..Default::default()
        }),
        Arc::new(Lookup),
    )
    .unwrap();
    let client =
        RemoteActorSystem::new(config("diagnostic-client", true), Arc::new(Lookup)).unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    probe.watch(&server, None, false);
    let mut malformed = tokio::net::TcpStream::connect(server.local_addr().unwrap())
        .await
        .unwrap();
    malformed.write_all(&0u32.to_be_bytes()).await.unwrap();
    malformed.shutdown().await.unwrap();
    probe.wait().await;
    let address = NodeAddr::tcp("diagnostic-server", server.local_addr().unwrap());
    tokio::time::timeout(Duration::from_secs(3), client.connect(&address))
        .await
        .expect("diagnostic stopped accepting new TCP handshakes")
        .unwrap();
    echo(&client, "diagnostic-server").await;
    probe.check(false);
    probe.armed.store(false, Ordering::SeqCst);
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}

async fn replacement(probe: Arc<Probe>) {
    let client =
        RemoteActorSystem::new(config("diagnostic-client", false), Arc::new(Lookup)).unwrap();
    let mut peer_config = config("diagnostic-peer", false);
    peer_config.topology_role = crate::TopologyRole::Hub;
    peer_config.direct_addr = Some("127.0.0.1:7998".into());
    let peer = RemoteActorSystem::new(peer_config, Arc::new(Lookup)).unwrap();
    client.start().await.unwrap();
    peer.start().await.unwrap();
    client
        .connect(&NodeAddr::mem("diagnostic-peer"))
        .await
        .unwrap();
    let (old, old_status) = {
        let links = client.links.read().unwrap();
        (links[0].sender.clone(), links[0].status.clone())
    };
    client
        .connect(&NodeAddr::mem("diagnostic-peer"))
        .await
        .unwrap();
    let (current, current_status) = {
        let links = client.links.read().unwrap();
        assert_eq!(links.len(), 2);
        (links[1].sender.clone(), links[1].status.clone())
    };
    assert!(!old.same_connection(&current));
    *probe.retiring.lock().unwrap() = Some(old.clone());
    probe.watch(&client, Some("diagnostic-peer"), false);
    old.close();
    probe.wait().await;
    until(
        || client.links.read().unwrap().len() == 1,
        "diagnostic prevented retiring old generation with replacement",
    )
    .await;
    probe.check(false);
    assert_eq!(old_status.get(), NodeState::Disconnected);
    assert_eq!(current_status.get(), NodeState::Connected);
    assert_eq!(
        client.nodes.get("diagnostic-peer").unwrap().1.get(),
        NodeState::Connected
    );
    assert!(!current.is_closed());
    assert!(client
        .direct_sender_of("diagnostic-peer")
        .unwrap()
        .same_connection(&current));
    assert!(client
        .uplink
        .read()
        .unwrap()
        .as_ref()
        .unwrap()
        .same_connection(&current));
    assert_eq!(
        client
            .dial_addrs
            .lock()
            .unwrap()
            .get("diagnostic-peer")
            .unwrap(),
        "127.0.0.1:7998"
    );
    echo(&client, "diagnostic-peer").await;
    probe.armed.store(false, Ordering::SeqCst);
    client.shutdown().await.unwrap();
    peer.shutdown().await.unwrap();
}

macro_rules! lifecycle_contract {
    ($name:ident, $message:literal, $payload:literal, $body:expr) => {
        #[tokio::test]
        async fn $name() {
            let probe = Probe::new($message);
            isolated(
                concat!(
                    "system::lifecycle_diagnostic_contracts::",
                    stringify!($name)
                ),
                &["parrot_remote::system"],
                $payload,
                probe.clone(),
                $body(probe),
            )
            .await;
        }
    };
}
lifecycle_contract!(
    registration_survives_diagnostic_panic,
    "uplink set",
    false,
    |probe| registration(probe, false)
);
lifecycle_contract!(
    registration_survives_diagnostic_payload_panic,
    "uplink set",
    true,
    |probe| registration(probe, false)
);
lifecycle_contract!(
    registration_diagnostic_can_reenter_shutdown,
    "uplink set",
    false,
    |probe| registration(probe, true)
);
lifecycle_contract!(
    watcher_survives_diagnostic_panic,
    "connection closed (register_link watcher)",
    false,
    |probe| retirement(probe, false, false)
);
lifecycle_contract!(
    watcher_survives_diagnostic_payload_panic,
    "connection closed (register_link watcher)",
    true,
    |probe| retirement(probe, false, false)
);
lifecycle_contract!(
    watcher_diagnostic_can_reenter_shutdown,
    "connection closed (register_link watcher)",
    false,
    |probe| retirement(probe, false, true)
);
lifecycle_contract!(
    uplink_retirement_survives_diagnostic_panic,
    "uplink cleared",
    false,
    |probe| retirement(probe, true, false)
);
lifecycle_contract!(
    uplink_retirement_survives_diagnostic_payload_panic,
    "uplink cleared",
    true,
    |probe| retirement(probe, true, false)
);
lifecycle_contract!(
    uplink_retirement_diagnostic_can_reenter_shutdown,
    "uplink cleared",
    false,
    |probe| retirement(probe, true, true)
);
lifecycle_contract!(
    accept_loop_survives_diagnostic_panic,
    "remote accept failed",
    false,
    accepting
);
lifecycle_contract!(
    accept_loop_survives_diagnostic_payload_panic,
    "remote accept failed",
    true,
    accepting
);
lifecycle_contract!(
    old_connection_diagnostic_preserves_replacement,
    "connection closed (register_link watcher)",
    false,
    replacement
);
lifecycle_contract!(
    old_connection_diagnostic_payload_preserves_replacement,
    "connection closed (register_link watcher)",
    true,
    replacement
);
