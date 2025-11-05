//! S1（DEV_06 §2）入站 gossip 接线端到端测试。
//!
//! 与 `swim.rs` 内纯函数测试的区别：这里走**真实系统路径**——
//! RemoteActorSystem → Ingress.dispatch → SysEventHook（AdminHook）→
//! membership 合并 → digest 失配回发 full_sync（经真实 FrameSender）。
//!
//! 防回归点：P6 初版 `handle_gossip` 只被单测调用、从未接入系统入站
//! 路径（出站发 gossip 没人消费）——本文件锁死该接线。

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use parrot_remote::frame::{Frame, FrameHeader, PROTOCOL_VERSION, frame_type};
use parrot_remote::swim::{
    Member, MemberEvent, MemberStatus, MembershipGossip, NodeAddrWire, encode_gossip,
};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};

struct NopLookup;

#[async_trait::async_trait]
impl LocalLookup for NopLookup {
    async fn lookup(&self, _path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}

fn member(id: &str, status: MemberStatus, inc: u64) -> Member {
    Member {
        node_id: id.into(),
        addr: NodeAddrWire {
            node_id: id.into(),
            scheme: "mem".into(),
            host: "127.0.0.1".into(),
            port: 0,
        },
        status,
        incarnation: inc,
        status_until_ms: 0,
        metadata: parrot_remote::bytes::Bytes::new(),
    }
}

fn gossip_frame(g: &MembershipGossip) -> Frame {
    Frame {
        header: FrameHeader {
            frame_len: 0,
            version: PROTOCOL_VERSION,
            frame_type: frame_type::SYSTEM_EVENT,
            flags: 0,
            correlation_id: 0,
            hop_count: 0,
            hop_limit: 8,
            seq: parrot_remote::frame::SEQ_NONE,
        },
        path: String::new(),
        type_key: String::new(),
        payload: encode_gossip(g),
    }
}

/// S1-INT-1 入站 push 半程：A 发全量事件帧 → B 系统 membership 真实合并。
///
/// 验证链路：encode → Frame → mem transport → B.ingress.dispatch →
/// AdminHook.MembershipGossip → handle_gossip → B.membership 状态变化。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_int_inbound_gossip_merges_into_system_membership() {
    let a = RemoteActorSystem::new(RemoteConfig::mem("gw-a"), Arc::new(NopLookup)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("gw-b"), Arc::new(NopLookup)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();

    // A 侧构造全量事件（A 视角：a、x、y 三个 Alive 成员）
    let events = vec![
        MemberEvent::Upsert(member("gw-a", MemberStatus::Alive, 0)),
        MemberEvent::Upsert(member("x", MemberStatus::Alive, 7)),
        MemberEvent::Upsert(member("y", MemberStatus::Alive, 11)),
    ];
    let digest = {
        let mut probe = parrot_remote::swim::Membership::new(0);
        for ev in &events {
            probe.merge_event(ev);
        }
        probe.digest()
    };
    let g = MembershipGossip {
        events,
        seen_from: "gw-a".into(),
        digest,
        full_sync: None,
    };

    // 经真实链路发送：A 的 links 里找到 → B 的 sender
    let sender = {
        let links = a.links_snapshot().await;
        links
            .iter()
            .find(|(n, _, _)| n == "gw-b")
            .map(|(_, s, _)| s.clone())
            .expect("A→B link 存在")
    };
    sender
        .send(gossip_frame(&g))
        .await
        .expect("gossip frame sent");

    let members = wait_members(&b, &["gw-a", "gw-b", "x", "y"]).await;
    assert_eq!(members["x"], member("x", MemberStatus::Alive, 7));
    assert_eq!(members["y"], member("y", MemberStatus::Alive, 11));
    assert!(members.values().all(|m| m.status == MemberStatus::Alive));

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

/// S1-INT-2 pull 半程：B 发落后 digest 探测 → A 系统回 full_sync 帧。
///
/// 验证：A 的 membership 已有成员而 B 探测帧 digest 过期 →
/// A 钩子经真实 FrameSender 回 MembershipGossip{full_sync}。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_int_stale_probe_triggers_full_sync_reply() {
    let a = RemoteActorSystem::new(RemoteConfig::mem("fs-a"), Arc::new(NopLookup)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("fs-b"), Arc::new(NopLookup)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();

    // 预置 A 侧 membership（直接注入——模拟 A 已收敛的成员表）
    {
        let mut m = a.membership.lock().await;
        m.merge_event(&MemberEvent::Upsert(member("fs-a", MemberStatus::Alive, 0)));
        m.merge_event(&MemberEvent::Upsert(member("n1", MemberStatus::Alive, 17)));
        m.merge_event(&MemberEvent::Upsert(member("n2", MemberStatus::Alive, 23)));
    }

    // B → A 发 digest 落后的纯探测帧（A 判定需回全量）
    let stale_probe = MembershipGossip {
        events: vec![],
        seen_from: "fs-b".into(),
        digest: a.membership.lock().await.digest() ^ 1, // 必定不同，而不是猜测常数
        full_sync: None,
    };
    let sender = {
        let links = b.links_snapshot().await;
        links
            .iter()
            .find(|(n, _, _)| n == "fs-a")
            .map(|(_, s, _)| s.clone())
            .expect("B→A link")
    };
    sender
        .send(gossip_frame(&stale_probe))
        .await
        .expect("probe sent");

    let members = wait_members(&b, &["fs-a", "fs-b", "n1", "n2"]).await;
    assert_eq!(members["n1"], member("n1", MemberStatus::Alive, 17));
    assert_eq!(members["n2"], member("n2", MemberStatus::Alive, 23));
    assert!(members.values().all(|m| m.status == MemberStatus::Alive));

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

async fn wait_members(system: &RemoteActorSystem, expected: &[&str]) -> HashMap<String, Member> {
    let members = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            {
                let membership = system.membership.lock().await;
                if expected
                    .iter()
                    .all(|id| membership.members.contains_key(*id))
                {
                    break membership.members.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expected identities never reached the actual system membership");
    assert_eq!(
        members.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        expected.iter().copied().collect::<BTreeSet<_>>()
    );
    members
}

fn barrier(req_id: u64) -> Frame {
    let mut frame = gossip_frame(&MembershipGossip {
        events: vec![],
        seen_from: String::new(),
        digest: 0,
        full_sync: None,
    });
    frame.header.correlation_id = req_id;
    frame.payload = parrot_remote::admin_v2::encode_admin_cmd_v2(
        &parrot_remote::admin_v2::AdminCommandV2::MetricsReportV2 { req_id },
    );
    frame
}

async fn full_syncs_before_barrier(
    inbound: &mut tokio::sync::mpsc::Receiver<(
        Frame,
        parrot_remote::transport::FrameSender,
        String,
    )>,
    req_id: u64,
) -> Vec<MembershipGossip> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut full_syncs = Vec::new();
        loop {
            let (frame, _, from) = inbound.recv().await.expect("peer closed before barrier");
            assert_eq!(from, "ss-a");
            if frame.header.frame_type != frame_type::SYSTEM_EVENT {
                continue;
            }
            match parrot_remote::admin::decode_sys_event(&frame.payload).unwrap() {
                parrot_remote::admin::SysEvent::AdminReplyV2(reply) => {
                    assert_eq!(frame.header.correlation_id, req_id);
                    assert!(matches!(reply,
                        parrot_remote::admin_v2::AdminReplyV2::MeasuredMetrics { req_id: id, .. }
                        if id == req_id));
                    return full_syncs;
                }
                parrot_remote::admin::SysEvent::MembershipGossip(body) => {
                    let gossip = parrot_remote::swim::decode_gossip(&body).unwrap();
                    if gossip.full_sync.is_some() {
                        full_syncs.push(gossip);
                    }
                }
                other => panic!("unexpected peer system event: {other:?}"),
            }
        }
    })
    .await
    .expect("same-connection processing barrier timed out")
}

/// Observe actual frames: equal digest is silent, unequal digest returns exactly
/// one full table, then equal digest is silent again. Membership alone cannot
/// reveal redundant replies. The command follows the probe in the same physical
/// connection's serialized control lane, so no fixed sleep proves absence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_int_steady_state_no_full_sync() {
    use parrot_remote::handshake::{HandshakeBody, caps};
    use parrot_remote::transport::{Transport, tcp::TcpTransport};

    let a = RemoteActorSystem::new(
        RemoteConfig::tcp("ss-a", Some("127.0.0.1:0".parse().unwrap())).with_insecure_development(),
        Arc::new(NopLookup),
    )
    .unwrap();
    a.start().await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let peer = TcpTransport::new(
        HandshakeBody {
            node_id: "ss-b".into(),
            capabilities: caps::BIN | caps::MEMBERSHIP_GOSSIP | caps::MEASURED_METRICS,
            ..Default::default()
        },
        tx,
        Arc::new(|_| {}),
        shutdown,
        None,
    );
    let connection = peer
        .connect(&parrot_remote::NodeAddr::tcp(
            "ss-a",
            a.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let expected = wait_members(&a, &["ss-a", "ss-b"]).await;
    connection.sender.send(barrier(1)).await.unwrap();
    assert!(full_syncs_before_barrier(&mut rx, 1).await.is_empty());
    let digest = a.membership.lock().await.digest();

    for (req_id, probe_digest, expected_count) in
        [(2, digest, 0), (3, digest ^ 1, 1), (4, digest, 0)]
    {
        connection
            .sender
            .send(gossip_frame(&MembershipGossip {
                events: vec![],
                seen_from: "ss-b".into(),
                digest: probe_digest,
                full_sync: None,
            }))
            .await
            .unwrap();
        connection.sender.send(barrier(req_id)).await.unwrap();
        let replies = full_syncs_before_barrier(&mut rx, req_id).await;
        assert_eq!(
            replies.len(),
            expected_count,
            "incorrect full-sync traffic for probe {req_id}"
        );
        for reply in replies {
            assert_eq!(reply.seen_from, "ss-a");
            assert_eq!(reply.digest, digest);
            let members = reply.full_sync.unwrap();
            assert_eq!(members.len(), expected.len());
            assert_eq!(
                members
                    .into_iter()
                    .map(|m| (m.node_id.clone(), m))
                    .collect::<HashMap<_, _>>(),
                expected
            );
        }
    }
    peer.shutdown().await;
    a.shutdown().await.unwrap();
}
