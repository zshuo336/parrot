//! Membership state-machine contracts and real three-node memory gossip.
//! No process kill or network-partition timing claim is made by the pure tests.

use std::sync::Arc;
use std::time::Duration;

use parrot_remote::swim::{Member, MemberEvent, MemberStatus, Membership, NodeAddrWire};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};

struct NopLookup;

#[async_trait::async_trait]
impl LocalLookup for NopLookup {
    async fn lookup(&self, _path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}

fn member(id: &str, alive: bool) -> Member {
    Member {
        node_id: id.into(),
        addr: NodeAddrWire {
            node_id: id.into(),
            scheme: "mem".into(),
            host: "127.0.0.1".into(),
            port: 0,
        },
        status: if alive {
            MemberStatus::Alive
        } else {
            MemberStatus::Dead
        },
        incarnation: 0,
        status_until_ms: 0,
        metadata: parrot_remote::bytes::Bytes::new(),
    }
}

/// The local state machine changes Suspect to Dead at its configured deadline.
/// This injects logical time; it does not measure failure-detection latency.
#[test]
fn swim_suspicion_deadline_state_machine() {
    let mut m = Membership::new(0);
    for id in ["a", "b", "c"] {
        m.merge_event(&MemberEvent::Upsert(member(id, true)));
    }
    assert_eq!(m.alive_nodes().len(), 3);

    // kill c：T+0 起 probe 失败（第一轮 probe_interval 内即标记）
    m.set_clock(500); // 首轮 probe tick
    assert!(m.mark_suspect("c"), "first failed probe marks Suspect");

    // suspect_timeout=3s：T+3.5s（500 首检 + 3000 超时）内多数派（a,b）标 c=Dead
    m.set_clock(500 + 3000);
    let (changed, _) = m.tick();
    assert!(
        changed
            .iter()
            .any(|x| x.node_id == "c" && x.status == MemberStatus::Dead),
        "c must be Dead at 3.5s budget"
    );
    // 多数派视角：alive = {a, b}（c 不在 alive_nodes）
    assert_eq!(m.alive_nodes().len(), 2);
}

/// Pure merge/refute contract. No transport partition or wall-clock claim.
#[test]
fn swim_refute_merge_state_machine() {
    let mut a = Membership::new(0); // 节点 A 视角
    let mut b = Membership::new(0); // 节点 B 视角
    // 分区前：双方互见 Alive
    a.merge_event(&MemberEvent::Upsert(member("a", true)));
    a.merge_event(&MemberEvent::Upsert(member("b", true)));
    b.merge_event(&MemberEvent::Upsert(member("a", true)));
    b.merge_event(&MemberEvent::Upsert(member("b", true)));

    // 注入不同视图：A 视角 B Suspect→Dead(inc=0)；B 视角 A Suspect(inc=0)；
    // 且互相收到对方的 Suspect 事件（本人表里自己也 Suspect——refute 触发条件）
    a.mark_suspect("b");
    a.set_clock(3100);
    a.tick(); // a 表: b → Dead
    b.mark_suspect("a");
    // A 广播 Suspect(a) 到 B 表 / B 广播 Suspect(b) 到 A 表（gossip 车传播）
    let b_suspect_of_a = b.members["a"].clone(); // Suspect(0)
    a.merge_event(&MemberEvent::Upsert(b_suspect_of_a)); // a 表: a → Suspect(0)
    assert_eq!(a.members["a"].status, MemberStatus::Suspect);
    // 对称：B 收到 A 广播的 Suspect(b)（分区双向静默——A 的 probe 也失败）
    let a_suspect_b = member("b", false);
    let a_suspect_b = Member {
        status: MemberStatus::Suspect,
        ..a_suspect_b
    };
    b.merge_event(&MemberEvent::Upsert(a_suspect_b)); // b 表: b → Suspect(0)
    assert_eq!(b.members["b"].status, MemberStatus::Suspect);

    // 愈合：双方各自 refute（本人收到 Suspect 自己 → inc+1 Alive 广播）
    let a_refute = a.refute("a").unwrap(); // A: (1, Alive)
    let b_refute = b.refute("b").unwrap(); // B: (1, Alive)
    assert_eq!(a_refute.status, MemberStatus::Alive);
    assert_eq!(b_refute.status, MemberStatus::Alive);

    // 交换 refute 事件：Dead(0) < Alive(1) / Suspect(0) < Alive(1) 偏序覆盖
    assert!(
        b.merge_event(&MemberEvent::Upsert(a_refute)),
        "A Alive(1) heals B Suspect(0)"
    );
    assert!(
        a.merge_event(&MemberEvent::Upsert(b_refute)),
        "B Alive(1) heals A Dead(0)"
    );
    // 两表全部回 Alive，digest 一致
    for m in a.members.values() {
        assert_eq!(m.status, MemberStatus::Alive);
    }
    for m in b.members.values() {
        assert_eq!(m.status, MemberStatus::Alive);
    }
    assert_eq!(a.digest(), b.digest(), "partitions converge after heal");
}

/// gossip 全链路：三节点 mem 集群，事件传播到非直连节点（经 gossip 车间接）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swim_gossip_propagation_mem3() {
    // A-B、A-C 联接（C 与 B 非直连——B 的事件经 A 中转）
    let a = RemoteActorSystem::new(RemoteConfig::mem("sw-a"), Arc::new(NopLookup)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("sw-b"), Arc::new(NopLookup)).unwrap();
    let c = RemoteActorSystem::new(RemoteConfig::mem("sw-c"), Arc::new(NopLookup)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    c.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();
    a.connect_mem_pair(&c).await.unwrap();

    // No handcrafted payloads or direct merge calls: B must learn C through
    // the running transport, system hook and periodic gossip tasks via A.
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let from_b = b.membership.lock().await;
            let from_c = c.membership.lock().await;
            if from_b
                .members
                .get("sw-c")
                .is_some_and(|m| m.status == MemberStatus::Alive)
                && from_c
                    .members
                    .get("sw-b")
                    .is_some_and(|m| m.status == MemberStatus::Alive)
                && from_b.members.len() == 3
                && from_b.digest() == from_c.digest()
            {
                break;
            }
            drop(from_b);
            drop(from_c);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    c.shutdown().await.unwrap();
    result.expect("periodic gossip must reach the non-adjacent node and converge");
}
