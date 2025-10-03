//! P2 集群 POC 测试：SWIM 成员关系 + Receptionist。

use remote_poc::*;
use std::time::Duration;

// ---------- SWIM 基本成员关系 ----------

#[tokio::test]
async fn swim_membership_merge_and_gossip() {
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");

    // B 自举：gossip 自己的表给 A
    let table_b = b.members().await;
    a.merge_membership(table_b).await;
    let table_a = a.members().await;
    b.merge_membership(table_a).await;

    // 双方都看到 2 成员且 Alive
    for n in [&a, &b] {
        let m = n.members().await;
        assert_eq!(m.len(), 2, "{:?} should see 2 members", n.node_id);
        assert!(m.iter().all(|x| x.status == MemberStatus::Alive));
    }
}

// ---------- 故障检测：直接/间接探测 → suspect → dead ----------

#[tokio::test]
async fn swim_failure_detection_suspect_then_dead() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    let c = SwimNode::new("C");

    // 三节点互相认识
    for x in [&a, &b, &c] {
        let t = x.members().await;
        a.merge_membership(t.clone()).await;
        b.merge_membership(t.clone()).await;
        c.merge_membership(t).await;
    }

    // C 宕机（分区：发往 C 的报文全丢）
    bus.partition("C");

    // A 直接探测 C → 不可达
    let out = a
        .probe(&bus, "C", &["B".to_string()], Duration::from_millis(30))
        .await;
    assert!(!out.reachable, "partitioned node must be unreachable");
    assert!(out.used_indirect, "should have tried indirect");

    // suspect
    a.mark_suspect("C").await;
    let m = a.members().await;
    assert_eq!(
        m.iter().find(|x| x.node_id == "C").unwrap().status,
        MemberStatus::Suspect
    );

    // gossip 传播 suspect（A → B）
    b.merge_membership(a.members().await).await;
    let mb = b.members().await;
    assert_eq!(
        mb.iter().find(|x| x.node_id == "C").unwrap().status,
        MemberStatus::Suspect,
        "suspect 状态应随 gossip 传播"
    );

    // 超时升级 dead
    a.mark_dead("C").await;
    let m = a.members().await;
    assert_eq!(m.iter().find(|x| x.node_id == "C").unwrap().status, MemberStatus::Dead);
}

// ---------- 被疑节点反驳（refute） ----------

#[tokio::test]
async fn swim_refute_beats_stale_suspect() {
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");

    // A 的表：B suspect @ inc 0（陈旧）
    a.merge_membership(vec![Member {
        node_id: "B".into(),
        addr: "B".into(),
        incarnation: 0,
        status: MemberStatus::Suspect,
    }])
    .await;

    // B 反驳：inc 1 Alive
    b.refute().await;
    a.merge_membership(b.members().await).await;

    let m = a.members().await;
    let bm = m.iter().find(|x| x.node_id == "B").unwrap();
    assert_eq!((bm.status, bm.incarnation), (MemberStatus::Alive, 1));
}

// ---------- Receptionist：注册与订阅推送 ----------

#[tokio::test]
async fn receptionist_register_and_subscribe() {
    let r = Receptionist::new();

    // 先订阅（worker 服务的消费者）
    let mut rx = r.subscribe("worker").await;
    // 注册两个提供者
    r.register("worker", "node-1", "parrot://node-1/user/w1").await;
    r.register("worker", "node-2", "parrot://node-2/user/w2").await;

    // 收到两次推送：1 条与 2 条
    let first = rx.recv().await.unwrap();
    assert_eq!(first.len(), 1);
    let second = rx.recv().await.unwrap();
    assert_eq!(second.len(), 2);
    assert!(second.contains(&("node-2".into(), "parrot://node-2/user/w2".into())));
}

// ---------- 组合场景：集群发现 → 远程调用（P2 × P1 联动） ----------

#[tokio::test]
async fn discovery_then_remote_call() {
    CodecRegistry::reset();
    install_poc_messages();

    // 两节点集群 + 发现
    let r = Receptionist::new();
    // 先订阅（register 的推送不丢）
    let mut sub = r.subscribe("echo").await;

    // P1 远程层：B 起 echo actor
    let (ea, eb) = endpoint_pair().await;
    let ts = eb.local.get_thread_system("main").unwrap();
    ts.spawn_at::<EchoForDiscovery>(EchoForDiscovery, "/user/echo", None, Default::default())
        .await
        .unwrap();

    // B 注册服务 → A 收到推送
    r.register("echo", "B", "/user/echo").await;
    let entries = tokio::time::timeout(std::time::Duration::from_secs(3), sub.recv())
        .await
        .expect("recv within 3s")
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, "/user/echo");

    // 发现结果 → 远程 ask（P1 链路）
    let remote = ea.remote_ref("/user/echo");
    let pong = remote.send(Box::new(Ping(7))).await.unwrap();
    assert_eq!(pong.downcast_ref::<Pong>().unwrap().0, 7);
}

use parrot_api::address::ActorRef as _;
use parrot::thread::context::ThreadContext;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
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
                Err(parrot_api::errors::ActorError::MessageHandlingError("unsupported".into()))
            }
        })
    }
    fn receive_message_with_engine<'a>(
        &'a mut self,
        _m: BoxedMessage,
        _c: &'a mut Self::Context,
        _e: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        None
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
