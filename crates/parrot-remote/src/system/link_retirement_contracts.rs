//! Actual physical-link retirement, routing and handshake metadata ownership.
use super::*;
use crate::{handshake::TopologyRole, topology::TopologyMode};
use futures::FutureExt;
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{panic::AssertUnwindSafe, sync::Mutex};

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}

#[derive(Clone, Debug, Default)]
struct Echo(Arc<Mutex<Vec<u64>>>);
impl Echo {
    fn record(&self, message: &BoxedMessage) {
        self.0
            .lock()
            .unwrap()
            .push(message.downcast_ref::<crate::ingress::TestAsk>().unwrap().0);
    }
}
impl ActorRef for Echo {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.record(&message);
            Ok(message)
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.record(&message);
            Ok(())
        })
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
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
#[async_trait::async_trait]
impl LocalLookup for Echo {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/echo").then(|| Box::new(self.clone()) as BoxedActorRef)
    }
}

fn config(id: String, role: TopologyRole, advertisement: Option<&str>) -> RemoteConfig {
    let mut config = RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap()))
        .with_insecure_development()
        .with_role(role);
    config.heartbeat_enabled = false;
    config.direct_addr = advertisement.map(str::to_owned);
    config
}
async fn connect(source: &Arc<RemoteActorSystem>, peer: &Arc<RemoteActorSystem>) {
    // Route-hint/DNS-style connection ownership, with no configured seed hiding
    // the active connection's endpoint in NodeTable::get.
    RemoteActorSystem::connect_weak_identity(
        &Arc::downgrade(source),
        &NodeAddr::tcp(&peer.config.node_id, peer.local_addr().unwrap()),
        false,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !peer
            .missing_direct_connections(&[&source.config.node_id])
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn retired(system: &RemoteActorSystem, sender: &FrameSender) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while system
            .links_snapshot()
            .await
            .iter()
            .any(|(_, current, _)| current.same_connection(sender))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("closed physical link did not retire");
}

struct Network {
    source: Arc<RemoteActorSystem>,
    first: Arc<RemoteActorSystem>,
    second: Arc<RemoteActorSystem>,
    target: Arc<RemoteActorSystem>,
    echo: Echo,
    old: FrameSender,
    latest: FrameSender,
}
impl Network {
    async fn new(
        same_node: bool,
        second_role: TopologyRole,
        second_address: Option<&str>,
        mode: TopologyMode,
    ) -> Self {
        let id = uuid::Uuid::new_v4();
        let mut source_config = config(format!("retire-source-{id}"), TopologyRole::Normal, None);
        source_config.topology.mode = mode;
        source_config.topology.relay_fallback = mode == TopologyMode::Hybrid;
        let source = RemoteActorSystem::new(source_config, Arc::new(Empty)).unwrap();
        let first = RemoteActorSystem::new(
            config(
                format!("retire-hub-{id}"),
                TopologyRole::Hub,
                Some("127.0.0.1:40001"),
            ),
            Arc::new(Empty),
        )
        .unwrap();
        let second_id = if same_node {
            first.config.node_id.clone()
        } else {
            format!("retire-other-{id}")
        };
        let second = RemoteActorSystem::new(
            config(second_id, second_role, second_address),
            Arc::new(Empty),
        )
        .unwrap();
        let echo = Echo::default();
        let target = RemoteActorSystem::new(
            config(format!("retire-target-{id}"), TopologyRole::Normal, None),
            Arc::new(echo.clone()),
        )
        .unwrap();
        for system in [&source, &first, &second, &target] {
            system.start().await.unwrap();
        }
        // The two hub listeners are real. The two DIRECT_ADDR strings are
        // deliberately distinct handshake metadata and are never dialed.
        connect(&target, &first).await;
        connect(&target, &second).await;
        connect(&source, &first).await;
        let old = source.direct_sender_of(&first.config.node_id).unwrap();
        connect(&source, &second).await;
        let latest = source.direct_sender_of(&second.config.node_id).unwrap();
        assert!(!old.same_connection(&latest));
        assert_eq!(source.links_snapshot().await.len(), 2);
        Self {
            source,
            first,
            second,
            target,
            echo,
            old,
            latest,
        }
    }
    async fn exchange(&self, value: u64) {
        let actor = self
            .source
            .remote_ref(&format!("parrot://{}/echo", self.target.config.node_id))
            .unwrap();
        self.exchange_with(&actor, value).await;
    }
    async fn exchange_with(&self, actor: &RemoteActorRef, value: u64) {
        let reply = actor
            .send_with_timeout(
                Box::new(crate::ingress::TestAsk(value)),
                Some(Duration::from_secs(2)),
            )
            .await
            .unwrap();
        assert_eq!(
            reply.downcast_ref::<crate::ingress::TestAsk>().unwrap().0,
            value
        );
        actor
            .deliver_with_ack(
                Box::new(crate::ingress::TestAsk(value + 1000)),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
    }
    fn selected(&self, sender: &FrameSender) {
        assert!(
            self.source
                .relay_sender()
                .is_some_and(|actual| actual.same_connection(sender)),
            "live authenticated relay alternative was not selected"
        );
    }
    async fn close_latest(&self) {
        self.latest.close();
        retired(&self.source, &self.latest).await;
    }
    fn address(&self, peer: &RemoteActorSystem, advertisement: Option<&str>) {
        assert_eq!(
            self.source.nodes.get(&peer.config.node_id).unwrap().0.addr,
            peer.local_addr().unwrap(),
            "node endpoint does not belong to surviving physical link"
        );
        let advertised = self
            .source
            .dial_addrs
            .lock()
            .unwrap()
            .get(&peer.config.node_id)
            .cloned();
        assert_eq!(
            advertised.as_deref(),
            advertisement,
            "advertised address does not belong to current live link"
        );
    }
    async fn finish(self) {
        for system in [&self.source, &self.first, &self.second, &self.target] {
            system.shutdown().await.unwrap();
        }
    }
}

async fn failover(same_node: bool, role: TopologyRole) {
    for mode in [TopologyMode::Hub, TopologyMode::Hybrid] {
        let network = Network::new(same_node, role, Some("127.0.0.1:40002"), mode).await;
        let result = AssertUnwindSafe(async {
            let actor = network
                .source
                .remote_ref(&format!("parrot://{}/echo", network.target.config.node_id))
                .unwrap();
            network.selected(&network.latest);
            network.exchange_with(&actor, 11).await;
            network.close_latest().await;
            assert!(!network.old.is_closed());
            network.selected(&network.old);
            network.address(&network.first, Some("127.0.0.1:40001"));
            let cached = network.source.uplink.read().unwrap().clone().unwrap();
            assert!(cached.same_connection(&network.old));
            network.exchange_with(&actor, 12).await;
            assert_eq!(*network.echo.0.lock().unwrap(), [11, 1011, 12, 1012]);
            let links = network.source.links_snapshot().await;
            assert_eq!(links.len(), 1);
            assert!(links[0].1.same_connection(&network.old));
            assert_eq!(links[0].2.get(), NodeState::Connected);
        })
        .catch_unwind()
        .await;
        network.finish().await;
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
#[tokio::test]
async fn newest_duplicate_hub_retirement_restores_live_relay_and_business() {
    failover(true, TopologyRole::Hub).await;
}
#[tokio::test]
async fn newest_border_retirement_restores_older_hub_and_business() {
    failover(true, TopologyRole::Border).await;
}
#[tokio::test]
async fn newest_hub_retirement_restores_other_hub_and_business() {
    failover(false, TopologyRole::Hub).await;
}

#[tokio::test]
async fn relay_selection_has_no_gap_before_the_close_watcher_runs() {
    let network = Network::new(
        true,
        TopologyRole::Hub,
        Some("127.0.0.1:40002"),
        TopologyMode::Hub,
    )
    .await;
    let result = AssertUnwindSafe(async {
        network.latest.close();
        // No await: the watcher cannot have committed its cleanup yet.
        assert_eq!(network.source.links.read().unwrap().len(), 2);
        network.selected(&network.old);
        network.exchange(21).await;
        assert_eq!(*network.echo.0.lock().unwrap(), [21, 1021]);
    })
    .catch_unwind()
    .await;
    network.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn ordinary_same_identity_link_is_not_a_replacement_uplink() {
    let network = Network::new(
        true,
        TopologyRole::Normal,
        Some("127.0.0.1:40002"),
        TopologyMode::Hub,
    )
    .await;
    let result = AssertUnwindSafe(async {
        network.selected(&network.old);
        network.exchange(31).await;
        network.old.close();
        retired(&network.source, &network.old).await;
        assert!(
            network.source.uplink.read().unwrap().is_none(),
            "retired uplink remained retained behind an ordinary link"
        );
        assert!(network.source.relay_sender().is_none());
        assert!(network
            .source
            .remote_ref(&format!("parrot://{}/echo", network.target.config.node_id))
            .is_err());
        network.address(&network.second, Some("127.0.0.1:40002"));
        assert_eq!(*network.echo.0.lock().unwrap(), [31, 1031]);
    })
    .catch_unwind()
    .await;
    network.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn registration_without_an_advertisement_retires_the_previous_address() {
    let network = Network::new(true, TopologyRole::Normal, None, TopologyMode::Hub).await;
    let result = AssertUnwindSafe(async {
        network.address(&network.second, None);
        network.close_latest().await;
        network.address(&network.first, Some("127.0.0.1:40001"));
        network.selected(&network.old);
        network.exchange(41).await;
        assert_eq!(*network.echo.0.lock().unwrap(), [41, 1041]);
    })
    .catch_unwind()
    .await;
    network.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn retirement_restores_the_survivors_endpoint_and_advertisement() {
    let network = Network::new(
        true,
        TopologyRole::Normal,
        Some("127.0.0.1:40002"),
        TopologyMode::Hub,
    )
    .await;
    let result = AssertUnwindSafe(async {
        network.address(&network.second, Some("127.0.0.1:40002"));
        network.close_latest().await;
        network.address(&network.first, Some("127.0.0.1:40001"));
        network.exchange(51).await;
        assert_eq!(*network.echo.0.lock().unwrap(), [51, 1051]);
    })
    .catch_unwind()
    .await;
    network.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn retiring_both_generations_releases_the_cached_uplink() {
    let network = Network::new(
        true,
        TopologyRole::Hub,
        Some("127.0.0.1:40002"),
        TopologyMode::Hub,
    )
    .await;
    let result = AssertUnwindSafe(async {
        network.close_latest().await;
        network.old.close();
        retired(&network.source, &network.old).await;
        assert!(network.source.links_snapshot().await.is_empty());
        assert!(
            network.source.uplink.read().unwrap().is_none(),
            "cached uplink outlived both physical connections"
        );
        assert!(network.source.dial_addrs.lock().unwrap().is_empty());
        assert!(network
            .source
            .nodes
            .get(&network.first.config.node_id)
            .is_none());
    })
    .catch_unwind()
    .await;
    network.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn retiring_the_older_link_keeps_the_latest_metadata() {
    let network = Network::new(
        true,
        TopologyRole::Hub,
        Some("127.0.0.1:40002"),
        TopologyMode::Hub,
    )
    .await;
    let result = AssertUnwindSafe(async {
        network.old.close();
        retired(&network.source, &network.old).await;
        network.selected(&network.latest);
        network.address(&network.second, Some("127.0.0.1:40002"));
        network.exchange(61).await;
        assert_eq!(*network.echo.0.lock().unwrap(), [61, 1061]);
    })
    .catch_unwind()
    .await;
    network.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn simultaneously_closed_generations_do_not_preserve_connection_ownership() {
    let network = Network::new(
        true,
        TopologyRole::Hub,
        Some("127.0.0.1:40002"),
        TopologyMode::Hub,
    )
    .await;
    let result = AssertUnwindSafe(async {
        let status = network
            .source
            .nodes
            .get(&network.first.config.node_id)
            .unwrap()
            .1;
        network.latest.close();
        network.old.close();
        assert_eq!(network.source.links.read().unwrap().len(), 2);
        assert!(network.source.relay_sender().is_none());
        retired(&network.source, &network.latest).await;
        retired(&network.source, &network.old).await;
        assert_eq!(status.get(), NodeState::Disconnected);
        assert!(network
            .source
            .nodes
            .get(&network.first.config.node_id)
            .is_none());
        assert!(network.source.dial_addrs.lock().unwrap().is_empty());
        assert!(network.source.uplink.read().unwrap().is_none());
    })
    .catch_unwind()
    .await;
    network.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn retiring_all_connections_preserves_configured_seed_ownership() {
    let network = Network::new(
        true,
        TopologyRole::Hub,
        Some("127.0.0.1:40002"),
        TopologyMode::Hub,
    )
    .await;
    let result = AssertUnwindSafe(async {
        let seed = NodeAddr::tcp(
            &network.first.config.node_id,
            "127.0.0.1:40003".parse().unwrap(),
        );
        network.source.nodes.add_seed(seed.clone());
        let status = network.source.nodes.get(&seed.node_id).unwrap().1;
        network.close_latest().await;
        let restored = network.source.nodes.get(&seed.node_id).unwrap().0;
        assert_eq!(restored, seed);
        assert_eq!(status.get(), NodeState::Connected);
        network.selected(&network.old);
        network.exchange(71).await;
        network.old.close();
        retired(&network.source, &network.old).await;
        let retained = network.source.nodes.get(&seed.node_id).unwrap().0;
        assert_eq!(retained, seed);
        assert_eq!(status.get(), NodeState::Disconnected);
        assert!(network.source.dial_addrs.lock().unwrap().is_empty());
        assert!(network.source.uplink.read().unwrap().is_none());
        assert_eq!(*network.echo.0.lock().unwrap(), [71, 1071]);
    })
    .catch_unwind()
    .await;
    network.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn untrusted_tcp_hub_role_does_not_create_an_uplink() {
    let id = uuid::Uuid::new_v4();
    let mut local = RemoteConfig::tcp(format!("untrusted-source-{id}"), None);
    local.heartbeat_enabled = false;
    let source = RemoteActorSystem::new(local, Arc::new(Empty)).unwrap();
    let peer = RemoteActorSystem::new(
        config(format!("untrusted-hub-{id}"), TopologyRole::Hub, None),
        Arc::new(Empty),
    )
    .unwrap();
    source.start().await.unwrap();
    peer.start().await.unwrap();
    let result = AssertUnwindSafe(async {
        connect(&source, &peer).await;
        assert!(source.relay_sender().is_none());
        assert!(source.uplink.read().unwrap().is_none());
        assert!(source.remote_ref("parrot://untrusted-target/echo").is_err());
    })
    .catch_unwind()
    .await;
    source.shutdown().await.unwrap();
    peer.shutdown().await.unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn route_hint_can_replace_closed_links_before_watchers_retire_them() {
    let network = Network::new(
        true,
        TopologyRole::Hub,
        Some("127.0.0.1:40002"),
        TopologyMode::Hybrid,
    )
    .await;
    let result = AssertUnwindSafe(async {
        network.latest.close();
        network.old.close();
        assert_eq!(network.source.links.read().unwrap().len(), 2);
        let router = SystemRelay {
            system: Arc::downgrade(&network.source),
            self_node: network.source.config.node_id.clone(),
        };
        crate::ingress::RelayRouter::on_hint(
            &router,
            &network.first.config.node_id,
            &network.first.local_addr().unwrap().to_string(),
        );
        let fresh = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(sender) = network
                    .source
                    .direct_sender_of(&network.first.config.node_id)
                {
                    break sender;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed registry entries suppressed a fresh route hint");
        assert!(!fresh.same_connection(&network.old));
        assert!(!fresh.same_connection(&network.latest));
        network.selected(&fresh);
        network.exchange(81).await;
        assert_eq!(*network.echo.0.lock().unwrap(), [81, 1081]);
    })
    .catch_unwind()
    .await;
    network.finish().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
