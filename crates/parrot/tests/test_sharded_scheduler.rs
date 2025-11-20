//! Instrumented sharded scheduling experiments.
//! S1 compares one fixed-order ask workload; it is not a general speedup proof.
//! S2 observes actual owner threads, CPU results and overlapping probe progress.
//! S4 measures current sequential asks; it does not count allocations.
//! Both workloads verify exact handling counts and actual retirement outside
//! their timing windows, including when a worker or assertion fails.
//! Timing values are descriptive and depend on the host and workload.

mod engine_stress_common;
mod sharded_isolation;

use engine_stress_common::*;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::message::Message;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt;

#[derive(Default)]
struct Observation {
    initialized: AtomicUsize,
    handled: AtomicU64,
    dropped: AtomicUsize,
}

impl Observation {
    fn verify_handled(&self, expected: u64) {
        assert_eq!(self.initialized.load(Ordering::SeqCst), 1);
        assert_eq!(self.handled.load(Ordering::SeqCst), expected);
        assert_eq!(self.dropped.load(Ordering::SeqCst), 0);
    }

    fn verify_retired(&self) {
        assert_eq!(self.dropped.load(Ordering::SeqCst), 1);
    }
}

// 复用压测 BenchActor 形态（本地重定义，逻辑对等）
struct BenchActor {
    observation: Arc<Observation>,
}

impl Drop for BenchActor {
    fn drop(&mut self) {
        self.observation.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl Actor for BenchActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.observation.initialized.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let r = dispatch(self, m);
        Box::pin(async move { r })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn dispatch(a: &mut BenchActor, m: BoxedMessage) -> ActorResult<BoxedMessage> {
    if let Some(e) = m.downcast_ref::<Echo>() {
        a.observation.handled.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(e.value) as BoxedMessage)
    } else if let Some(t) = m.downcast_ref::<CpuTask>() {
        let r = burn_cpu(t.iterations, t.salt);
        Ok(Box::new(r) as BoxedMessage)
    } else {
        Err(parrot_api::errors::ActorError::MessageHandlingError(
            "unknown".into(),
        ))
    }
}

use parrot_api::errors::ActorError;

impl Message for Echo {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for CpuTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}

async fn ask_echo(r: &parrot::thread::address::ThreadActorRef<BenchActor>, v: u64) -> u64 {
    let rep = r
        .ask(Box::new(Echo { value: v }) as BoxedMessage)
        .await
        .unwrap();
    *rep.downcast::<u64>().unwrap()
}

async fn setup() -> (ParrotActorSystem, Arc<ThreadActorSystem>) {
    let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
        .await
        .unwrap();
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    parrot
        .register_thread_system("shardbench".into(), ts.clone(), true)
        .await
        .unwrap();
    (parrot, ts)
}

fn cfg_sharded(key: &str) -> ThreadActorConfig {
    ThreadActorConfig {
        scheduling_mode: Some(SchedulingMode::Sharded {
            affinity_key: key.into(),
        }),
        ..Default::default()
    }
}

async fn retire_group(
    system: &ThreadActorSystem,
    actors: &[parrot::thread::address::ThreadActorRef<BenchActor>],
    observations: &[Arc<Observation>],
) {
    let mut receipts = Vec::new();
    for actor in actors {
        receipts.push(system.stop_actor(&actor.path()).await);
    }
    for receipt in receipts {
        receipt.expect("individual experiment actor retirement");
    }
    for actor in actors {
        assert!(
            !actor.is_alive().await,
            "individual retirement left experiment actor live"
        );
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while !system.actor_paths().is_empty()
            || observations
                .iter()
                .any(|stats| stats.dropped.load(Ordering::SeqCst) != 1)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("previous experiment group must actually retire before the next measurement");
    for stats in observations {
        stats.verify_retired();
    }
}

async fn finish<T>(
    parrot: ParrotActorSystem,
    system: &ThreadActorSystem,
    observations: &[Arc<Observation>],
    outcome: std::thread::Result<T>,
) -> T {
    let shutdown = system.shutdown_internal().await;
    let facade = parrot.internal_shutdown().await;
    let result = match outcome {
        Ok(result) => result,
        Err(panic) => {
            let drops: Vec<_> = observations
                .iter()
                .map(|stats| stats.dropped.load(Ordering::SeqCst))
                .collect();
            eprintln!(
                "experiment cleanup: system={shutdown:?}, facade={facade:?}, registry={}, drops={drops:?}",
                system.actor_paths().len()
            );
            std::panic::resume_unwind(panic);
        }
    };
    shutdown.expect("experiment system retirement");
    facade.expect("experiment facade retirement");
    assert!(system.actor_paths().is_empty());
    for stats in observations {
        stats.verify_retired();
    }
    result
}

async fn measure_group(
    system: &Arc<ThreadActorSystem>,
    observations: &mut Vec<Arc<Observation>>,
    sharded: bool,
    actors: usize,
    per_actor: u64,
    warmup: u64,
) -> Duration {
    let start_observations = observations.len();
    let mut refs = Vec::new();
    for i in 0..actors {
        let observation = Arc::new(Observation::default());
        observations.push(observation.clone());
        refs.push(
            system
                .spawn_at(
                    BenchActor { observation },
                    &format!("/s1/{sharded}/{i}"),
                    None,
                    if sharded {
                        cfg_sharded(&format!("domain-{}", i % 4))
                    } else {
                        ThreadActorConfig::default()
                    },
                )
                .await
                .unwrap(),
        );
    }
    for actor in &refs {
        for _ in 0..warmup {
            assert_eq!(ask_echo(actor, 1).await, 1);
        }
    }
    let started = Instant::now();
    let mut jobs = Vec::new();
    for (i, actor) in refs.iter().cloned().enumerate() {
        jobs.push(tokio::spawn(async move {
            for k in 0..per_actor {
                let value = i as u64 * 1000 + k;
                assert_eq!(ask_echo(&actor, value).await, value);
            }
        }));
    }
    let results = futures::future::join_all(jobs).await;
    for result in results {
        result.expect("every experiment worker must finish");
    }
    let elapsed = started.elapsed();
    for stats in &observations[start_observations..] {
        stats.verify_handled(warmup + per_actor);
    }
    retire_group(system, &refs, &observations[start_observations..]).await;
    elapsed
}

// ---------------------------------------------------------------------------
// S1: 亲和性 ask 吞吐 —— Sharded vs SharedPool
// ---------------------------------------------------------------------------

#[test]
#[ignore = "explicit sequential sharded/shared throughput experiment; use --release in isolation"]
fn s1_sharded_vs_shared_ask_throughput() {
    run_s1(8, 20_000, 200);
}

fn run_s1(actors: usize, per_actor: u64, warmup: u64) {
    assert!(actors > 0 && per_actor > 0, "empty S1 workload");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let (parrot, ts) = setup().await;
        let mut observations = Vec::new();
        let result = std::panic::AssertUnwindSafe(async {
            let sharded = measure_group(&ts, &mut observations, true, actors, per_actor, warmup).await;
            let shared = measure_group(&ts, &mut observations, false, actors, per_actor, warmup).await;
            (sharded, shared)
        })
        .catch_unwind()
        .await;
        let (shard_wall, pool_wall) = finish(parrot, &ts, &observations, result).await;
        let total = actors as u64 * per_actor;
        println!(
            "[S1] sharded: {} msgs in {:.3}s = {:.0}k/s | shared-pool: {:.3}s = {:.0}k/s | ratio {:.2}x",
            total, shard_wall.as_secs_f64(), total as f64 / shard_wall.as_secs_f64() / 1000.0,
            pool_wall.as_secs_f64(), total as f64 / pool_wall.as_secs_f64() / 1000.0,
            pool_wall.as_secs_f64() / shard_wall.as_secs_f64(),
        );
        println!(
            "[S1-CONTRACT] actors_per_mode={actors} warmup_per_actor={warmup} measured_per_actor={per_actor} measured_replies_per_mode={total} actors_retired={} facade_retired=true",
            actors * 2
        );
    });
}

// S2 keeps the original four long CPU tasks but verifies actual shard owners,
// completed results and probe progress inside each measured handler interval.
#[test]
#[ignore = "four calibrated long CPU tasks; descriptive latency, not a hard isolation SLA"]
fn s2_shard_isolation_under_overload() {
    sharded_isolation::run(8.0);
}

// ---------------------------------------------------------------------------
// S4: current sequential ask latency and throughput (not an allocation count)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "explicit sequential ask latency experiment; not an allocation count"]
fn s4_alloc_optimized_seq_ask() {
    run_s4(10_000, 2000);
}

fn run_s4(count: u64, warmup: u64) {
    assert!(count > 0, "empty S4 workload");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let (parrot, ts) = setup().await;
        let observation = Arc::new(Observation::default());
        let result = std::panic::AssertUnwindSafe(async {
        let a = ts
            .spawn_at::<BenchActor>(
                BenchActor {
                    observation: observation.clone(),
                },
                "/s4/echo",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        for _ in 0..warmup {
            assert_eq!(ask_echo(&a, 1).await, 1);
        } // warmup
        let mut lats = Vec::new();
        let t0 = Instant::now();
        for i in 0..count {
            let t = Instant::now();
            let v = ask_echo(&a, i).await;
            assert_eq!(v, i);
            lats.push(t.elapsed().as_micros());
        }
        let wall = t0.elapsed();
        let st = latency_stats(lats);
        assert_eq!(st.count, count);
        observation.verify_handled(warmup + count);
        retire_group(&ts, &[a], std::slice::from_ref(&observation)).await;
        (wall, st)
        })
        .catch_unwind()
        .await;
        let (wall, st) = finish(parrot, &ts, std::slice::from_ref(&observation), result).await;
        println!(
            "[S4] seq-ask-{count}: {:.3}s = {:.0}k/s (p50={:.0}µs p99={:.0}µs)",
            wall.as_secs_f64(),
            count as f64 / wall.as_secs_f64() / 1000.0,
            st.p50_us,
            st.p99_us
        );
        println!("[S4-CONTRACT] warmup={warmup} measured_replies={count} actors_retired=1 facade_retired=true");
    });
}

#[test]
fn small_s1_checks_both_scheduler_groups_and_retirement() {
    run_s1(2, 17, 3);
}

#[test]
fn small_s4_checks_sequential_work_and_retirement() {
    run_s4(17, 3);
}

#[test]
fn missing_or_duplicate_dispatch_is_rejected() {
    let stats = Observation::default();
    stats.initialized.store(1, Ordering::SeqCst);
    stats.handled.store(20, Ordering::SeqCst);
    stats.verify_handled(20);
    for invalid in [19, 21] {
        stats.handled.store(invalid, Ordering::SeqCst);
        assert!(std::panic::catch_unwind(|| stats.verify_handled(20)).is_err());
    }
}

#[test]
fn missing_or_duplicate_actor_disposal_is_rejected() {
    let stats = Observation::default();
    for invalid in [0, 2] {
        stats.dropped.store(invalid, Ordering::SeqCst);
        assert!(std::panic::catch_unwind(|| stats.verify_retired()).is_err());
    }
    stats.dropped.store(1, Ordering::SeqCst);
    stats.verify_retired();
}

#[test]
fn empty_experiments_are_rejected() {
    assert!(std::panic::catch_unwind(|| run_s1(0, 17, 3)).is_err());
    assert!(std::panic::catch_unwind(|| run_s1(2, 0, 3)).is_err());
    assert!(std::panic::catch_unwind(|| run_s4(0, 3)).is_err());
}
