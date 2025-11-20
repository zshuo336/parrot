//! P4 state-machine scenarios and local facade tests. The sharding membership
//! changes and singleton time below are injected; these are not real node-kill,
//! network partition or distributed singleton takeover acceptance tests.
//! - `sharding_membership_removal`：注入成员移除及精确代次退休状态
//! - `sharding_affinity_l1`：同 key 连续消息命中同宿主
//! - `sharding_local_retirement_and_state_transfer`：本地实例退休及显式状态恢复
//! - `singleton_takeover`：注入时钟下的本地租约接管
//! - `facade_prefix_route`：`/user/entity-*` 前缀通配（§7.1）

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use parrot::system::ParrotActorSystem;
use parrot::thread::config::ThreadActorConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, EmptyConfig};
use parrot_api::address::{ActorPath, ActorRef, RetirementStatus, stop_for_owned_cleanup};
use parrot_api::system::{ActorSystem, ActorSystemConfig};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_remote::sharding::{EntityStatus, ShardCoordinator, entity_key_of};
use parrot_remote::singleton::{
    Candidate, LeaseParams, SingletonLease, SingletonState, SingletonTransition,
};

// ── 共享：计数实体 actor ─────────────────────────────────

/// 实体收 TELL 计数（sharding 零丢失断言用）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Tick(pub u64);

macro_rules! reg {
    ($t:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &BoxedMessage| {
                    let m = msg.downcast_ref::<$t>().ok_or(concat!("downcast ", $key))?;
                    Ok(m.0.to_le_bytes().to_vec())
                },
                decode: |b: &[u8]| {
                    let a: [u8; 8] = b.try_into().map_err(|_| "Tick requires exactly eight bytes")?;
                    Ok(Box::new(<$t>::from_bytes(u64::from_le_bytes(a))) as BoxedMessage)
                },
            }
        }
    };
}

impl Tick {
    fn from_bytes(v: u64) -> Self {
        Tick(v)
    }
}

reg!(Tick, "bin:u:Tick");

fn tick_codec() -> &'static parrot_api::message::CodecRegistration {
    parrot_api::message::inventory::iter::<parrot_api::message::CodecRegistration>
        .into_iter()
        .find(|codec| codec.type_key == "bin:u:Tick")
        .expect("registered Tick codec")
}

#[test]
fn tick_codec_rejects_short_payloads_without_panicking() {
    let codec = tick_codec();
    for length in 0..8 {
        let bytes = vec![0; length];
        let result = std::panic::catch_unwind(|| (codec.decode)(&bytes));
        assert!(result.is_ok(), "short fixture payload must not unwind");
        assert!(result.unwrap().is_err());
    }
}

#[test]
fn tick_codec_rejects_trailing_bytes_and_preserves_exact_values() {
    let codec = tick_codec();
    for length in [9, 16, 1024] {
        assert!((codec.decode)(&vec![0; length]).is_err());
    }
    for value in [0, 1, u64::MAX] {
        let message = Box::new(Tick(value)) as BoxedMessage;
        let bytes = (codec.encode)(&message).unwrap();
        assert_eq!(bytes, value.to_le_bytes());
        let decoded = (codec.decode)(&bytes).unwrap();
        assert_eq!(decoded.downcast_ref::<Tick>(), Some(&Tick(value)));
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CounterState(Vec<u64>);
struct Apply(u64);
struct Snapshot;

/// The test host explicitly transfers this ordered effect log. The runtime
/// does not persist or deduplicate it on behalf of a distributed shard router.
struct Counter {
    state: CounterState,
    disposed: Arc<AtomicUsize>,
}

impl Drop for Counter {
    fn drop(&mut self) {
        self.disposed.fetch_add(1, Ordering::SeqCst);
    }
}

impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(Apply(id)) = msg.downcast_ref::<Apply>() {
                self.state.0.push(*id);
                return Ok(Box::new(()) as BoxedMessage);
            }
            if msg.is::<Snapshot>() {
                return Ok(Box::new(self.state.clone()) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "unhandled".into(),
            ))
        })
    }
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
}

async fn spawn_entity(
    ts: &Arc<ThreadActorSystem>,
    key: &str,
    state: CounterState,
    disposed: Arc<AtomicUsize>,
) -> Box<dyn ActorRef> {
    Box::new(
        ts.spawn_at(
            Counter { state, disposed },
            &format!("/user/entity-{key}"),
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap(),
    )
}

// ── D1 · sharding ───────────────────────────────────────

/// Inject membership removal and acknowledge retirement before local readmission.
#[test]
fn sharding_membership_removal() {
    let nodes = vec!["n1".to_string(), "n2".into(), "n3".into()];
    let coord = ShardCoordinator::new(&nodes);

    let mut leases = BTreeMap::new();
    for i in 0..100 {
        let key = format!("k{i}");
        let lease = coord.begin_activation(&key).unwrap();
        assert!(coord.activated(&lease));
        leases.insert(key, lease);
    }
    let before = coord.entities();
    let expected: BTreeSet<_> = before
        .iter()
        .filter(|(_, state)| state.node == "n2")
        .map(|(key, _)| key.clone())
        .collect();
    assert!(!expected.is_empty());
    let moves = coord.rebalance(&["n1".into(), "n3".into()]).unwrap();
    assert_eq!(moves.len(), expected.len());
    assert_eq!(
        moves
            .iter()
            .map(|item| item.owner.key().to_owned())
            .collect::<BTreeSet<_>>(),
        expected
    );
    let draining = coord.entities();
    for (key, state) in &before {
        assert_eq!(draining[key].node, state.node);
        if expected.contains(key) {
            assert_eq!(draining[key].status, EntityStatus::Migrating);
            assert!(coord.begin_activation(key).is_none());
        } else {
            assert_eq!(draining[key], *state);
        }
    }
    for item in moves {
        assert_eq!(item.owner, leases[item.owner.key()]);
        assert_eq!(item.owner.node(), "n2");
        assert!(matches!(item.target.as_deref(), Some("n1" | "n3")));
        assert!(coord.passivated(&item.owner));
        let replacement = coord.begin_activation(item.owner.key()).unwrap();
        assert_eq!(Some(replacement.node()), item.target.as_deref());
        assert!(coord.activated(&replacement));
        assert!(!coord.passivated(&item.owner));
    }
    for (key, state) in coord.entities() {
        assert_eq!(state.status, EntityStatus::Active);
        assert_eq!(coord.route(&key).as_ref(), Some(&state.node));
    }
}

/// 同 key 连续消息命中同宿主（ADR-14 两级亲和的 L1——环层）。
#[test]
fn sharding_affinity_l1() {
    let coord = ShardCoordinator::new(&["a".into(), "b".into(), "c".into(), "d".into()]);
    for key in ["u1", "u2", "u3", "entity-42", "sensor-99"] {
        let first = coord.route(key).unwrap();
        for _ in 0..100 {
            assert_eq!(coord.route(key).unwrap(), first, "key {key} must stick");
        }
    }
}

/// Two real local instances with explicit checkpoint transfer. This is not a
/// network drain, automatic routing, durable recovery or retry/deduplication gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sharding_local_retirement_and_state_transfer() {
    let old_system = ThreadActorSystem::shared(Default::default());
    let new_system = ThreadActorSystem::shared(Default::default());
    let coord = ShardCoordinator::new(&["n1".into()]);
    let key = "drain-1";
    let old_lease = coord.begin_activation(key).unwrap();
    let old_disposed = Arc::new(AtomicUsize::new(0));
    let old = spawn_entity(
        &old_system,
        key,
        CounterState::default(),
        old_disposed.clone(),
    )
    .await;
    assert!(coord.activated(&old_lease));
    for id in 1..=10 {
        old.deliver(Box::new(Apply(id))).await.unwrap();
    }
    let moves = coord.rebalance(&["n2".into()]).unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].owner, old_lease);
    assert_eq!(moves[0].target.as_deref(), Some("n2"));
    assert!(coord.begin_activation(key).is_none());
    assert_eq!(old_disposed.load(Ordering::SeqCst), 0);
    // The host closes admission here, then reads the already admitted work.
    let checkpoint = tokio::time::timeout(Duration::from_secs(3), old.send(Box::new(Snapshot)))
        .await
        .unwrap()
        .unwrap();
    let checkpoint = *checkpoint.downcast::<CounterState>().unwrap();
    assert_eq!(checkpoint.0, (1..=10).collect::<Vec<_>>());
    tokio::time::timeout(Duration::from_secs(3), stop_for_owned_cleanup(old.as_ref()))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        old.retirement_status(),
        RetirementStatus::Retired(Ok(()))
    ));
    assert_eq!(old_disposed.load(Ordering::SeqCst), 1);
    assert!(old.send(Box::new(Apply(999))).await.is_err());
    assert!(coord.passivated(&old_lease));

    let new_lease = coord.begin_activation(key).unwrap();
    assert_eq!(new_lease.node(), "n2");
    assert_ne!(old_lease, new_lease);
    let new_disposed = Arc::new(AtomicUsize::new(0));
    let replacement = spawn_entity(&new_system, key, checkpoint, new_disposed.clone()).await;
    assert!(coord.activated(&new_lease));
    assert!(!coord.passivated(&old_lease));
    for id in 11..=15 {
        replacement.deliver(Box::new(Apply(id))).await.unwrap();
    }
    let restored =
        tokio::time::timeout(Duration::from_secs(3), replacement.send(Box::new(Snapshot)))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        restored.downcast_ref::<CounterState>().unwrap().0,
        (1..=15).collect::<Vec<_>>()
    );
    assert_eq!(new_disposed.load(Ordering::SeqCst), 0);
    assert_eq!(coord.entities()[key].status, EntityStatus::Active);
    tokio::time::timeout(
        Duration::from_secs(3),
        stop_for_owned_cleanup(replacement.as_ref()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(new_disposed.load(Ordering::SeqCst), 1);
    assert!(coord.passivated(&new_lease));
    old_system.shutdown_internal().await.unwrap();
    new_system.shutdown_internal().await.unwrap();
}

// ── D2 · singleton ──────────────────────────────────────

/// Local model with injected confirmations/time; no process or network is killed.
#[test]
fn singleton_takeover() {
    let mut follower = SingletonLease::new(
        Candidate {
            node: "b".into(),
            seq: 5,
        },
        LeaseParams::default(),
    );
    let t0 = Instant::now();
    let mut leader = SingletonLease::new(
        Candidate {
            node: "a".into(),
            seq: 3,
        },
        LeaseParams::default(),
    );
    // a 竞选成功
    assert_eq!(
        leader.tick(t0, true),
        Some(SingletonTransition::Acquired(Candidate {
            node: "a".into(),
            seq: 3
        }))
    );
    // b 观察 a 持有
    follower.observe(
        t0,
        &Candidate {
            node: "a".into(),
            seq: 3,
        },
    );
    assert_eq!(follower.state(), SingletonState::Follower);
    // Stop feeding observations of a; drive only b's injected clock.
    let mut took_over_at: Option<Duration> = None;
    for s in 1..=6u64 {
        let t = t0 + Duration::from_secs(s * 3);
        // Missing model observations eventually expire b's local view.
        if let Some(SingletonTransition::Acquired(me)) = follower.tick(t, true) {
            assert_eq!(me.node, "b");
            took_over_at = Some(t.duration_since(t0));
            break;
        }
    }
    let wait = took_over_at.expect("no takeover within 18s");
    assert!(
        wait <= Duration::from_secs(13),
        "model takeover wait {wait:?} exceeds the configured lease/tick schedule"
    );
    assert_eq!(follower.state(), SingletonState::Leader);
}

// ── §7.1 facade 前缀路由 ────────────────────────────────

/// `/user/entity-*` 前缀处理器：未显式 spawn 的实体路径命中通配。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_prefix_route() {
    let facade = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let ts = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .unwrap();

    // A local echo handler verifies prefix selection only.
    struct Router;
    impl Actor for Router {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                // 原样回（echo 语义——路由命中断言用）
                if let Some(t) = msg.downcast_ref::<Tick>() {
                    return Ok(Box::new(Tick(t.0)) as BoxedMessage);
                }
                Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "unhandled".into(),
                ))
            })
        }
        fn state(&self) -> parrot_api::actor::ActorState {
            parrot_api::actor::ActorState::Running
        }
    }
    let router = ts
        .spawn_at(
            Router,
            "/user/shard-router",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    let router_arc: Arc<dyn ActorRef> = Arc::new(router.clone());

    facade
        .register_prefix_handler("/user/entity-", router_arc.clone())
        .unwrap();

    // 未显式 spawn 的实体路径 → 前缀通配命中
    let hit = facade
        .get_actor(&ActorPath::placeholder("/user/entity-not-spawned"))
        .await
        .expect("prefix route hit");
    let r = tokio::time::timeout(Duration::from_secs(3), hit.send(Box::new(Tick(7))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.downcast_ref::<Tick>().unwrap().0, 7);

    // 非前缀路径不命中
    assert!(
        facade
            .get_actor(&ActorPath::placeholder("/user/other-thing"))
            .await
            .is_none(),
        "non-prefix path must not hit"
    );

    // 非法前缀（无 '-'/'/' 尾锚）拒绝
    assert!(
        facade
            .register_prefix_handler("/user/entity", router_arc.clone())
            .is_err()
    );
    stop_for_owned_cleanup(router_arc.as_ref()).await.unwrap();
    ts.shutdown_internal().await.unwrap();
}

/// entity_key_of：`/user/entity-{key}` → key 提取。
#[test]
fn entity_key_extraction() {
    assert_eq!(entity_key_of("/user/entity-abc"), Some("abc"));
    assert_eq!(entity_key_of("/user/regular"), None);
    assert_eq!(entity_key_of("parrot://n/user/entity-x1"), Some("x1"));
}
