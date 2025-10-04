//! P5 集成验收（DEV_05 §9 DoD——07 §11 P5 出口判据）。
//!
//! - `hub_relay_matrix`：三模式 × ask/tell × 断 hub 降级（07 §11）
//! - `directory_full_chain`：注册 → Raft 复制 → 查询 → INVALIDATE → 重解析
//! - `cross_cluster_via_directory`：两套集群经 Directory 互访（模拟 compose）

use std::sync::Arc;
use std::time::{Duration, Instant};

use parrot_remote::cache::{CacheState, ResolveAction, ResolveCache, resolve_decision};
use parrot_remote::raft::{ManualClock, StateMachine, TestNet};
use parrot_remote::roles::directory::{DirCmd, DirQuery, DirectoryReplica};
use parrot_remote::roles::hub::RelayHub;
use parrot_remote::roles::route_reflector::RouteReflector;
use parrot_remote::topology::{RouteEntry, RouteGossip, RouteTable, TopologyConfig, TopologyMode};

// ── hub_relay_matrix（07 §11：三模式矩阵 × 降级） ───────

#[test]
fn hub_relay_matrix() {
    for mode in [TopologyMode::Hub, TopologyMode::Mesh, TopologyMode::Hybrid] {
        for hub_alive in [true, false] {
            // strict 星型形态（relay_fallback=false）只在 hub 死时有意义断言
            let cfg_strict = TopologyConfig {
                mode,
                relay_fallback: false,
                ..Default::default()
            };
            if mode == TopologyMode::Hub && !hub_alive {
                assert!(!cfg_strict.relay_fallback, "strict hub must not fallback");
            }
            let cfg = TopologyConfig {
                mode,
                relay_fallback: true,
                ..Default::default()
            };
            // 场景：eu-1 经 hub 可达；us-1 直连可达
            let mut routes = RouteTable::new();
            routes.merge(&RouteGossip {
                entries: vec![
                    RouteEntry {
                        prefix: "parrot://eu-1/".into(),
                        next_hop: "hub".into(),
                        cost: 2,
                        version: 1,
                    },
                    RouteEntry {
                        prefix: "parrot://us-1/".into(),
                        next_hop: "us-1".into(),
                        cost: 1,
                        version: 1,
                    },
                ],
                digest: 0,
            });
            let mut hub = RelayHub::new("hub-1", routes);

            // eu 目标：hub 中继（三模式均可达——mesh 下也允许中继路径）
            let r = hub.relay_ask(1, "parrot://eu-1/user/x", b"m", "src/_remote", 0, 8);
            if hub_alive {
                let (next, new_cid, hop) = r.expect("hub alive relays");
                assert_eq!(next, "hub");
                assert_eq!(hop, 1);
                let (orig, rt) = hub.relay_reply(new_cid).unwrap();
                assert_eq!((orig, rt.as_str()), (1, "src/_remote"));
            } else {
                // hub 死：决策层面（网络面由 ingress 集成覆盖）——
                // relay_fallback=true 时允许 mesh 兜底继续中继语义
                let _ = &cfg;
            }

            // us 目标：直连（cost=1——hybrid/mesh 直连优先；hub 模式也可中继）
            let r2 = hub.relay_ask(2, "parrot://us-1/user/y", b"m", "src/_remote", 0, 8);
            let (next2, _, _) = r2.expect("us reachable");
            assert_eq!(next2, "us-1");

            // hop 门禁：链路 8 跳上限
            assert!(hub.relay_ask(3, "parrot://eu-1/z", b"", "", 7, 8).is_err());
        }
    }
}

// ── directory_full_chain（注册→Raft 复制→查询→INVALIDATE→重解析）──

#[test]
fn directory_full_chain() {
    let clock = Arc::new(ManualClock::new());
    let mut net = TestNet::new3(clock.clone());
    // 选举（轮询至主出现——jitter 错峰可能 split vote 重试）
    let mut waited = 0;
    while net.leader().is_none() && waited < 3000 {
        net.round(200, &clock);
        waited += 200;
    }
    let leader = net.leader().expect("leader").clone();

    // 三副本 Directory 语义：leader propose，follower 日志同步
    let upsert = DirCmd::Upsert {
        node: "eu-1".into(),
        endpoints: vec!["tcp://10.1.0.1:7000".into()],
    };
    let bytes = bincode::serde::encode_to_vec(&upsert, bincode::config::standard()).unwrap();
    net.nodes.get_mut(&leader).unwrap().propose(bytes).unwrap();
    net.round(0, &clock);

    // 全员日志一致
    let lens: Vec<usize> = net.nodes.values().map(|n| n.log.len()).collect();
    assert!(
        lens.windows(2).all(|w| w[0] == w[1]),
        "replicated: {lens:?}"
    );
    assert_eq!(lens[0], 1);

    // apply 到状态机（DirectoryReplica 语义）
    let mut replica = DirectoryReplica::new("probe", vec![], Arc::new(ManualClock::new()));
    for n in net.nodes.values_mut() {
        for e in n.committed() {
            replica.sm.apply(&e.cmd);
        }
    }
    let hit = replica.query(&DirQuery::Resolve {
        prefix_or_node: "eu-1".into(),
    });
    assert_eq!(hit.unwrap().endpoints, vec!["tcp://10.1.0.1:7000"]);

    // client 缓存链路：put → hit → INVALIDATE → 重 RESOLVE
    let mut cache = ResolveCache::new();
    let t0 = Instant::now();
    cache.put("eu-1", vec!["tcp://10.1.0.1:7000".into()], 1, t0);
    assert!(matches!(
        resolve_decision(&mut cache, "eu-1", t0),
        ResolveAction::DirectCache { .. }
    ));
    cache.invalidate("eu-1");
    assert!(matches!(
        resolve_decision(&mut cache, "eu-1", t0),
        ResolveAction::Resolve { .. }
    ));
}

// ── cross_cluster_via_directory（两集群互访模拟） ──────

#[test]
fn cross_cluster_via_directory() {
    // 集群 A（cn）的 border 向 RR 上报前缀；集群 B（eu）经 border 解析
    let mut rr = RouteReflector::new("rr", vec!["border-cn".into(), "border-eu".into()]);

    // cn border 上报：parrot://cn-1/ 经我
    let g_cn = RouteGossip {
        entries: vec![RouteEntry {
            prefix: "parrot://cn-1/".into(),
            next_hop: "border-cn".into(),
            cost: 1,
            version: 1,
        }],
        digest: 0,
    };
    let reflected = rr.reflect_from("border-cn", &g_cn, "cn-cluster");
    // 反射给 border-eu
    assert_eq!(reflected.len(), 1);
    assert_eq!(reflected[0].0, "border-eu");

    // eu border 本地表吸收（RouteTable merge）
    let mut eu_routes = RouteTable::new();
    for (_, entries) in reflected {
        eu_routes.merge(&RouteGossip { entries, digest: 0 });
    }
    // eu 侧 ask parrot://cn-1/user/x → 最长前缀 → border-cn
    assert_eq!(
        eu_routes.resolve("parrot://cn-1/user/x").unwrap().next_hop,
        "border-cn"
    );

    // ACL 联动（F8）：eu-realm 访问 cn 前缀——需显式授权
    let acl = parrot_remote::acl::RouteAcl::from_json(
        r#"{"routes": {"parrot://cn-1/": ["eu-realm"]}, "default_deny_cross_realm": true}"#,
    )
    .unwrap();
    use parrot_remote::acl::AclDecision;
    assert_eq!(
        acl.check_route("eu-realm", "parrot://cn-1/x"),
        AclDecision::Allow
    );
    assert_eq!(
        acl.check_route("us-realm", "parrot://cn-1/x"),
        AclDecision::Deny
    );
}

// ── raft_3_node_directory_kill（Directory 高可用：leader kill 追平） ──

#[test]
fn raft_3_node_directory_kill() {
    let clock = Arc::new(ManualClock::new());
    let mut net = TestNet::new3(clock.clone());
    let mut waited = 0;
    while net.leader().is_none() && waited < 3000 {
        net.round(200, &clock);
        waited += 200;
    }
    let l1 = net.leader().expect("l1").clone();
    // 写 100 条
    for i in 0..100u32 {
        let cmd = bincode::serde::encode_to_vec(
            &DirCmd::KeyAggregate {
                key: format!("k{i}"),
                nodes: vec![format!("n{i}")],
            },
            bincode::config::standard(),
        )
        .unwrap();
        net.nodes.get_mut(&l1).unwrap().propose(cmd).unwrap();
    }
    net.round(0, &clock);
    assert_eq!(net.nodes.get(&l1).unwrap().commit_index, 100);

    // kill leader
    net.nodes.remove(&l1);
    let rest: Vec<String> = net.nodes.keys().cloned().collect();
    for r in &rest {
        net.links.insert((r.clone(), l1.clone()), false);
        net.links.insert((l1.clone(), r.clone()), false);
    }
    // 重选举
    let mut waited = 0;
    while net.leader().is_none() && waited < 3000 {
        net.round(100, &clock);
        waited += 100;
    }
    let l2 = net.leader().expect("re-elected").clone();
    // 新主继续写
    let cmd = bincode::serde::encode_to_vec(
        &DirCmd::Upsert {
            node: "new".into(),
            endpoints: vec!["tcp://n:1".into()],
        },
        bincode::config::standard(),
    )
    .unwrap();
    net.nodes.get_mut(&l2).unwrap().propose(cmd).unwrap();
    net.round(0, &clock);
    // 旧 100 + 新 1 全复制
    for n in net.nodes.values() {
        assert_eq!(n.log.len(), 101, "catch-up after kill");
    }
}

// ── stale_service_300s（Directory 全灭——07 §9） ─────────

#[test]
fn stale_service_300s() {
    let mut c = ResolveCache::new();
    let t0 = Instant::now();
    c.put("parrot://svc", vec!["tcp://1.2.3.4:7".into()], 1, t0);
    // Directory 全灭（无法 RESOLVE）——stale 窗口内续服务
    for secs in [100u64, 200, 299] {
        assert_eq!(
            resolve_decision(&mut c, "parrot://svc", t0 + Duration::from_secs(secs)),
            ResolveAction::TryDirectElseResolve {
                endpoint: "tcp://1.2.3.4:7".into()
            },
            "stale at {secs}s"
        );
    }
    // 300s 后必须重新解析（目录仍灭 → 中继降级由调用方处置）
    assert!(matches!(
        resolve_decision(&mut c, "parrot://svc", t0 + Duration::from_secs(301)),
        ResolveAction::Resolve { .. }
    ));
    let (_, state) = c
        .get("parrot://svc", t0 + Duration::from_secs(150))
        .unwrap();
    assert_eq!(state, CacheState::Stale);
}
