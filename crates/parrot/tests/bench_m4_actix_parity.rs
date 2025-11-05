//! M4 echo ask comparison, with one dedicated worker for each implementation.
//! Every warmup and measured response is checked in debug AND release.
//! The ordinary semantic test always runs; the ignored performance gate requires
//! release and keeps the original median paired latency ratio <= 1.15 target.
//! Samples alternate execution order; these local results do not certify an
//! uncontrolled host or broader lifecycle/supervision feature parity.

use actix::prelude::*;
use futures::FutureExt;
use parrot::actix::ActixActorSystem;
use parrot_api::ParrotTypedActor;
use parrot_api::message::Message;
use parrot_api::typed::TypedReceive;
use parrot_api::typed_channel::TypedActorRef;
use parrot_api::types::{ActorResult, BoxedFuture};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ============ 消息 ============
// 两套消息类型分开（actix Message derive 与 parrot Message trait 的
// impl 冲突不能共存于同一类型）。

/// 纯 actix 侧消息（actix derive）。
#[derive(Clone, Copy, Debug, MessageResponse)]
pub struct PureEcho(u64);

#[derive(Clone, Copy, Debug, Message)]
#[rtype(result = "PureEcho")]
pub struct PurePing(u64);

/// parrot 静态轨消息（parrot Message trait 手写 impl）。
#[derive(Clone, Copy, Debug)]
pub struct Ping(u64);

impl Message for Ping {
    type Result = u64;
}

// ============ 纯 actix 基准 actor ============

struct PureActixEcho;

impl Actor for PureActixEcho {
    type Context = Context<Self>;
}

impl Handler<PurePing> for PureActixEcho {
    type Result = PureEcho;
    fn handle(&mut self, msg: PurePing, _ctx: &mut Self::Context) -> PureEcho {
        PureEcho(msg.0)
    }
}

// ============ parrot 静态轨 actor ============

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Ping))]
pub struct TypedEcho;

impl TypedReceive<Ping> for TypedEcho {
    fn receive_typed<'a>(&'a mut self, msg: Ping) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(msg.0) })
    }
}

// Stop and join the baseline worker even if an assertion fails.
struct BaselineWorker(Option<Arbiter>);
impl Drop for BaselineWorker {
    fn drop(&mut self) {
        if let Some(worker) = self.0.take() {
            worker.stop();
            let result = worker.join();
            if !std::thread::panicking() {
                result.expect("baseline worker exited cleanly");
            }
        }
    }
}

async fn measure_pure(actor: &Addr<PureActixEcho>, count: u64) -> Duration {
    let start = Instant::now();
    for i in 0..count {
        let PureEcho(value) = actor.send(PurePing(i)).await.expect("pure response");
        assert_eq!(value, i, "pure echo changed the payload");
    }
    start.elapsed()
}

async fn measure_typed(actor: &TypedActorRef<TypedEcho, Ping>, count: u64) -> Duration {
    let start = Instant::now();
    for i in 0..count {
        let value = actor.ask(Ping(i)).await.expect("typed response");
        assert_eq!(value, i, "typed echo changed the payload");
    }
    start.elapsed()
}

#[derive(Debug)]
struct Sample {
    pure: Duration,
    typed: Duration,
}
impl Sample {
    fn ratio(&self) -> f64 {
        assert!(!self.pure.is_zero(), "baseline timer must resolve workload");
        self.typed.as_secs_f64() / self.pure.as_secs_f64()
    }
}

fn samples(count: u64, rounds: usize, warmup: u64) -> Vec<Sample> {
    assert!(count > 0 && rounds > 0 && rounds.is_multiple_of(2));
    let runner = System::new();
    let baseline = BaselineWorker(Some(runner.block_on(async { Arbiter::new() })));
    runner.block_on(async {
        // The old baseline ran on the producer's own arbiter while typed used
        // another worker. Both now cross the producer/worker thread boundary.
        let pure = PureActixEcho::start_in_arbiter(&baseline.0.as_ref().unwrap().handle(), |_| {
            PureActixEcho
        });
        let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
        let typed = system
            .spawn_typed::<TypedEcho, Ping>(TypedEcho, "/bench/typed")
            .await
            .unwrap();
        let result = std::panic::AssertUnwindSafe(async {
            let mut samples = Vec::with_capacity(rounds);
            for round in 0..rounds {
                let (pure_time, typed_time) = if round % 2 == 0 {
                    measure_pure(&pure, warmup).await;
                    measure_typed(&typed, warmup).await;
                    (
                        measure_pure(&pure, count).await,
                        measure_typed(&typed, count).await,
                    )
                } else {
                    measure_typed(&typed, warmup).await;
                    measure_pure(&pure, warmup).await;
                    let typed_time = measure_typed(&typed, count).await;
                    (measure_pure(&pure, count).await, typed_time)
                };
                samples.push(Sample {
                    pure: pure_time,
                    typed: typed_time,
                });
            }
            samples
        })
        .catch_unwind()
        .await;
        let stopped = typed.stop().await;
        let shutdown = system.as_ref().clone().shutdown().await;
        let samples = match result {
            Ok(samples) => samples,
            Err(payload) => std::panic::resume_unwind(payload),
        };
        stopped.expect("typed actor retirement");
        shutdown.expect("typed worker retirement");
        samples
    })
}

#[test]
fn m4_echo_semantics_in_every_profile() {
    let samples = samples(128, 2, 32);
    assert_eq!(samples.len(), 2);
    assert!(samples.iter().all(|sample| sample.ratio().is_finite()));
}

#[cfg(debug_assertions)]
#[test]
#[ignore = "explicit release performance gate: make check-m4-perf"]
fn m4_static_track_matches_pure_actix() {
    panic!(
        "performance gate requires cargo test --release; use m4_echo_semantics_in_every_profile for debug correctness"
    );
}

#[cfg(not(debug_assertions))]
#[test]
#[ignore = "explicit release performance gate: make check-m4-perf"]
fn m4_static_track_matches_pure_actix() {
    const MESSAGES: u64 = 50_000;
    let samples = samples(MESSAGES, 8, 1_000);
    let mut ratios = Vec::with_capacity(samples.len());
    for (index, sample) in samples.iter().enumerate() {
        let ratio = sample.ratio();
        println!(
            "pair={index} first={} messages={MESSAGES} pure_ns_per_ask={:.1} typed_ns_per_ask={:.1} ratio={ratio:.3}",
            if index % 2 == 0 { "pure" } else { "typed" },
            sample.pure.as_nanos() as f64 / MESSAGES as f64,
            sample.typed.as_nanos() as f64 / MESSAGES as f64
        );
        ratios.push(ratio);
    }
    ratios.sort_by(f64::total_cmp);
    let middle = ratios.len() / 2;
    let median = (ratios[middle - 1] + ratios[middle]) / 2.0;
    println!("median_paired_ratio={median:.3} limit=1.15");
    assert!(
        median <= 1.15,
        "typed track must match pure actix: median paired ratio={median:.3}, limit=1.15; samples={samples:?}"
    );
}
