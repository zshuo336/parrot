//! Cluster protocol contracts using real message receive loops.
use remote_poc::*;
use std::time::Duration;

#[tokio::test]
async fn swim_membership_merge_and_gossip() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    let ar = a.start_manual(&bus).unwrap();
    let br = b.start_manual(&bus).unwrap();
    b.gossip(&bus, "A").await.unwrap();
    assert!(
        b.probe(&bus, "A", &[], Duration::from_secs(1))
            .await
            .unwrap()
            .reachable
    );
    a.gossip(&bus, "B").await.unwrap();
    assert!(
        a.probe(&bus, "B", &[], Duration::from_secs(1))
            .await
            .unwrap()
            .reachable
    );
    for node in [&a, &b] {
        assert_eq!(
            node.members()
                .await
                .into_iter()
                .map(|m| (m.node_id, m.status, m.incarnation))
                .collect::<Vec<_>>(),
            vec![
                ("A".into(), MemberStatus::Alive, 0),
                ("B".into(), MemberStatus::Alive, 0)
            ]
        );
    }
    ar.shutdown().await.unwrap();
    br.shutdown().await.unwrap();
    assert_eq!(bus.registered_count(), 0);
}

#[tokio::test]
async fn swim_failure_detection_suspect_then_dead() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    let c = SwimNode::new("C");
    let ar = a.start_manual(&bus).unwrap();
    let br = b.start_manual(&bus).unwrap();
    let cr = c.start_manual(&bus).unwrap();
    a.merge_membership(c.members().await).await.unwrap();
    assert!(
        a.probe(&bus, "C", &[], Duration::from_secs(1))
            .await
            .unwrap()
            .reachable
    );
    bus.partition("C");
    let out = a
        .probe(&bus, "C", &["B".into()], Duration::from_millis(20))
        .await
        .unwrap();
    assert!(!out.reachable);
    assert!(out.used_indirect);
    assert!(a.mark_suspect("C", 0).await);
    a.gossip(&bus, "B").await.unwrap();
    assert!(
        a.probe(&bus, "B", &[], Duration::from_secs(1))
            .await
            .unwrap()
            .reachable
    );
    let observed = b
        .members()
        .await
        .into_iter()
        .find(|m| m.node_id == "C")
        .unwrap();
    assert_eq!(
        (observed.status, observed.incarnation),
        (MemberStatus::Suspect, 0)
    );
    assert!(a.mark_dead("C", 0).await);
    assert_eq!(
        a.members()
            .await
            .into_iter()
            .find(|m| m.node_id == "C")
            .unwrap()
            .status,
        MemberStatus::Dead
    );
    ar.shutdown().await.unwrap();
    br.shutdown().await.unwrap();
    cr.shutdown().await.unwrap();
    assert_eq!(bus.registered_count(), 0);
}

#[tokio::test]
async fn swim_refute_beats_stale_suspect() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    let ar = a.start_manual(&bus).unwrap();
    let br = b.start_manual(&bus).unwrap();
    a.merge_membership(vec![Member {
        node_id: "B".into(),
        addr: "B".into(),
        incarnation: 0,
        status: MemberStatus::Suspect,
    }])
    .await
    .unwrap();
    a.gossip(&bus, "B").await.unwrap();
    assert!(
        a.probe(&bus, "B", &[], Duration::from_secs(1))
            .await
            .unwrap()
            .reachable
    );
    b.gossip(&bus, "A").await.unwrap();
    assert!(
        b.probe(&bus, "A", &[], Duration::from_secs(1))
            .await
            .unwrap()
            .reachable
    );
    let member = a
        .members()
        .await
        .into_iter()
        .find(|m| m.node_id == "B")
        .unwrap();
    assert_eq!(
        (member.status, member.incarnation),
        (MemberStatus::Alive, 1)
    );
    ar.shutdown().await.unwrap();
    br.shutdown().await.unwrap();
}

#[tokio::test]
async fn receptionist_register_and_subscribe() {
    let r = Receptionist::new();
    let mut rx = r.subscribe("worker").await;
    assert!(rx.recv().await.unwrap().entries.is_empty());
    assert!(r.register("worker", "node-1", "/user/w1").await.unwrap());
    let first = rx.recv().await.unwrap();
    assert_eq!(first.entries, vec![("node-1".into(), "/user/w1".into())]);
    assert!(r.register("worker", "node-2", "/user/w2").await.unwrap());
    let second = rx.recv().await.unwrap();
    assert!(second.revision > first.revision);
    assert_eq!(
        second.entries,
        vec![
            ("node-1".into(), "/user/w1".into()),
            ("node-2".into(), "/user/w2".into())
        ]
    );
}

#[tokio::test]
async fn discovery_then_remote_call() {
    install_poc_messages();
    let r = Receptionist::new();
    let (ea, eb) = endpoint_pair().await;
    eb.local
        .get_thread_system("main")
        .unwrap()
        .spawn_at::<EchoForDiscovery>(EchoForDiscovery, "/user/echo", None, Default::default())
        .await
        .unwrap();
    r.register("echo", "B", "/user/echo").await.unwrap();
    let mut sub = r.subscribe("echo").await;
    let entries = tokio::time::timeout(Duration::from_secs(3), sub.recv())
        .await
        .unwrap()
        .unwrap()
        .entries;
    assert_eq!(entries, vec![("B".into(), "/user/echo".into())]);
    // Use the actual discovered path, not a separately hard-coded destination.
    let pong = ea
        .remote_ref(&entries[0].1)
        .send(Box::new(Ping(7)))
        .await
        .unwrap();
    assert_eq!(pong.downcast_ref::<Pong>().unwrap().0, 7);
}

use parrot::thread::context::ThreadContext;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef as _;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

struct EchoForDiscovery;
impl Actor for EchoForDiscovery {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(Ping(n)) = m.downcast_ref::<Ping>() {
                Ok(Box::new(Pong(*n)) as BoxedMessage)
            } else {
                Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "unsupported".into(),
                ))
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
