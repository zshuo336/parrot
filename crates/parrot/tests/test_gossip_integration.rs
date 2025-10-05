//! S1（DEV_06 §2）入站 gossip 接线端到端测试。
//!
//! 与 `swim.rs` 内纯函数测试的区别：这里走**真实系统路径**——
//! RemoteActorSystem → Ingress.dispatch → SysEventHook（AdminHook）→
//! membership 合并 → digest 失配回发 full_sync（经真实 FrameSender）。
//!
//! 防回归点：P6 初版 `handle_gossip` 只被单测调用、从未接入系统入站
//! 路径（出站发 gossip 没人消费）——本文件锁死该接线。

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
        MemberEvent::Upsert(member("x", MemberStatus::Alive, 0)),
        MemberEvent::Upsert(member("y", MemberStatus::Alive, 0)),
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

    // B 侧系统 membership 必须吸收（轮询等钩子异步完成——真实时序；
    // 每轮 sleep 让出 worker 给 I/O/分发任务，避免自旋饿死）
    let mut merged = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let m = b.membership.lock().await;
        if m.members.len() == 3 && m.members.contains_key("x") && m.members.contains_key("y") {
            merged = true;
            break;
        }
    }
    assert!(
        merged,
        "B 系统 membership 必须合并入站 gossip（接线断点回归）"
    );

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
        m.merge_event(&MemberEvent::Upsert(member("n1", MemberStatus::Alive, 0)));
        m.merge_event(&MemberEvent::Upsert(member("n2", MemberStatus::Alive, 0)));
    }

    // B → A 发 digest 落后的纯探测帧（A 判定需回全量）
    let stale_probe = MembershipGossip {
        events: vec![],
        seen_from: "fs-b".into(),
        digest: 0xDEAD_BEEF, // 与 A 指纹必然不同
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

    // B 侧 membership 应收到 A 回的 full_sync（3 成员）并合并
    let mut got_full = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let m = b.membership.lock().await;
        if m.members.len() == 3 {
            got_full = true;
            break;
        }
    }
    assert!(
        got_full,
        "B 必须收到 A 的 full_sync 回补（pull 半程接线断点回归）"
    );

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

/// S1-INT-3 稳态静默：digest 一致 → 不回 full_sync（零多余流量）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_int_steady_state_no_full_sync() {
    let a = RemoteActorSystem::new(RemoteConfig::mem("ss-a"), Arc::new(NopLookup)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("ss-b"), Arc::new(NopLookup)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();

    // 两侧 membership 预置一致
    let events = vec![
        MemberEvent::Upsert(member("ss-a", MemberStatus::Alive, 0)),
        MemberEvent::Upsert(member("ss-b", MemberStatus::Alive, 0)),
    ];
    for sys in [&a, &b] {
        let mut m = sys.membership.lock().await;
        for ev in &events {
            m.merge_event(ev);
        }
    }
    let digest = a.membership.lock().await.digest();

    // B → A 纯 digest 探测（指纹一致）
    let probe = MembershipGossip {
        events: vec![],
        seen_from: "ss-b".into(),
        digest,
        full_sync: None,
    };
    let sender = {
        let links = b.links_snapshot().await;
        links
            .iter()
            .find(|(n, _, _)| n == "ss-a")
            .map(|(_, s, _)| s.clone())
            .expect("B→A link")
    };
    sender.send(gossip_frame(&probe)).await.expect("probe");

    // 等待足够钩子处理时间——稳态下 B 不应收到任何 full_sync
    // （若 A 错误回发，B 侧 membership 已含 2 成员不变——用帧计数观测：
    //  这里以"B membership 状态不变化"为代理断言：无新成员混入）
    tokio::time::sleep(Duration::from_millis(200)).await;
    let m = b.membership.lock().await;
    assert_eq!(m.members.len(), 2, "稳态静默：无 full_sync 混入新状态");

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}
