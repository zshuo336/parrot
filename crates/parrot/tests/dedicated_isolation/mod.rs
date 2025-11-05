//! Actual work, owner and interval evidence for the dedicated-thread experiment.
use super::engine_stress_common::{burn_cpu, oracle};
use anyhow::{Context, Result, anyhow, ensure};
use futures::FutureExt;
use parrot::thread::{
    SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    collections::HashSet,
    sync::Arc,
    thread::ThreadId,
    time::{Duration, Instant},
};

enum Request {
    Probe,
    Work {
        salt: usize,
        entered: tokio::sync::mpsc::UnboundedSender<()>,
    },
}
#[derive(Clone)]
struct Work {
    salt: usize,
    value: u64,
    owner: ThreadId,
    started: Instant,
    completed: Instant,
}
struct Probe {
    value: u64,
    owner: ThreadId,
}
#[derive(Clone)]
struct Sample {
    owner: ThreadId,
    started: Instant,
    completed: Instant,
}
struct Worker;
impl Actor for Worker {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let request = message
                .downcast::<Request>()
                .map_err(|_| ActorError::MessageHandlingError("dedicated request type".into()))?;
            let owner = std::thread::current().id();
            match *request {
                Request::Probe => Ok(Box::new(Probe {
                    value: burn_cpu(1000, 1),
                    owner,
                }) as BoxedMessage),
                Request::Work { salt, entered } => {
                    let started = Instant::now();
                    entered.send(()).map_err(|_| {
                        ActorError::MessageHandlingError("work observer closed".into())
                    })?;
                    let value = burn_cpu(4_000_000_000, salt as u64);
                    Ok(Box::new(Work {
                        salt,
                        value,
                        owner,
                        started,
                        completed: Instant::now(),
                    }) as BoxedMessage)
                }
            }
        })
    }
}
fn validate(
    expected: &[u64],
    owners: &[ThreadId],
    works: &[Work],
    probes: &[Sample],
) -> Result<()> {
    ensure!(
        expected.len() == 4 && owners.len() == 4 && works.len() == 4,
        "four complete dedicated results required"
    );
    ensure!(
        owners.iter().copied().collect::<HashSet<_>>().len() == 4,
        "dedicated actors share an owner"
    );
    ensure!(probes.len() == 100, "100 complete probe requests required");
    ensure!(
        probes
            .iter()
            .all(|p| !owners.contains(&p.owner) && p.started <= p.completed),
        "probe used dedicated owner or invalid interval"
    );
    let mut seen = HashSet::new();
    for work in works {
        ensure!(
            work.salt < 4 && seen.insert(work.salt),
            "duplicate or unknown work identity"
        );
        ensure!(work.value == expected[work.salt], "wrong heavy result");
        ensure!(work.owner == owners[work.salt], "dedicated owner changed");
        ensure!(work.started < work.completed, "empty work interval");
        ensure!(
            probes
                .iter()
                .any(|p| p.started >= work.started && p.completed < work.completed),
            "no complete probe inside heavy work interval"
        );
    }
    let latest_start = works.iter().map(|w| w.started).max().unwrap();
    let earliest_finish = works.iter().map(|w| w.completed).min().unwrap();
    ensure!(
        latest_start < earliest_finish,
        "four heavy intervals never overlapped"
    );
    Ok(())
}
async fn probe(
    actor: &parrot::thread::address::ThreadActorRef<Worker>,
    expected: u64,
) -> Result<Probe> {
    let reply = actor
        .ask_with_timeout(Box::new(Request::Probe), Duration::from_secs(30))
        .await?;
    let reply = reply
        .downcast::<Probe>()
        .map_err(|_| anyhow!("probe reply type"))?;
    ensure!(reply.value == expected, "wrong probe result");
    Ok(*reply)
}
async fn measure(
    system: &Arc<ThreadActorSystem>,
    expected: &[u64],
    probe_expected: u64,
) -> Result<()> {
    // Keep registration and probe preparation within the original eight-second window.
    let start = Instant::now();
    let mut actors = Vec::new();
    let mut owners = Vec::new();
    for i in 0..4 {
        let actor = system
            .spawn_at(
                Worker,
                &format!("/a1/d{i}"),
                None,
                ThreadActorConfig {
                    scheduling_mode: Some(SchedulingMode::DedicatedThread),
                    ..Default::default()
                },
            )
            .await?;
        owners.push(probe(&actor, probe_expected).await?.owner);
        actors.push(actor);
    }
    let short = system
        .spawn_at(Worker, "/a1/short", None, Default::default())
        .await?;
    let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
    let mut jobs = Vec::new();
    for (salt, actor) in actors.into_iter().enumerate() {
        let entered = entered.clone();
        jobs.push(tokio::spawn(async move {
            let reply = actor
                .ask_with_timeout(
                    Box::new(Request::Work { salt, entered }),
                    Duration::from_secs(120),
                )
                .await?;
            reply
                .downcast::<Work>()
                .map(|work| *work)
                .map_err(|_| anyhow!("heavy reply type"))
        }));
    }
    drop(entered);
    let collect = async {
        // Await every job before propagating even the first business/join failure.
        futures::future::join_all(jobs)
            .await
            .into_iter()
            .map(|r| r.map_err(anyhow::Error::from).and_then(|r| r))
            .collect::<Result<Vec<_>>>()
    };
    let observe = async {
        tokio::time::timeout(Duration::from_secs(30), async {
            for _ in 0..4 {
                entries.recv().await.context("heavy work never entered")?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let mut probes = Vec::new();
        for _ in 0..100 {
            let started = Instant::now();
            let result = probe(&short, probe_expected).await?;
            probes.push(Sample {
                owner: result.owner,
                started,
                completed: Instant::now(),
            });
        }
        Ok::<_, anyhow::Error>(probes)
    };
    let (works, probes) = tokio::join!(collect, observe);
    let total = start.elapsed();
    let works = works?;
    let probes = probes?;
    validate(expected, &owners, &works, &probes)?;
    let max_latency = probes
        .iter()
        .map(|p| p.completed.duration_since(p.started))
        .max()
        .unwrap();
    println!(
        "[A1] verified four distinct owners, 4×4000000000 iterations; total={:.3}s limit=8s; 100 probes max={:.3}ms limit=500ms; hard_isolation=false",
        total.as_secs_f64(),
        max_latency.as_secs_f64() * 1000.0
    );
    ensure!(
        total < Duration::from_secs(8),
        "dedicated work exceeded original 8s threshold: {total:?}"
    );
    ensure!(
        max_latency < Duration::from_millis(500),
        "probe exceeded original 500ms threshold: {max_latency:?}"
    );
    Ok(())
}
pub(super) fn run() {
    if cfg!(debug_assertions) {
        panic!(
            "A1 performance gate requires --release; ordinary validation contracts run in both profiles"
        );
    }
    // Independent reference computations are joined before starting the timer/runtime.
    let reference_jobs: Vec<_> = (0..4)
        .map(|salt| std::thread::spawn(move || oracle::reference(4_000_000_000, salt)))
        .collect();
    let results: Vec<_> = reference_jobs.into_iter().map(|job| job.join()).collect();
    let expected: Vec<_> = results
        .into_iter()
        .map(|r| r.expect("reference worker failed"))
        .collect();
    let probe_expected = oracle::reference(1000, 1);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 2,
            shared_burst_workers_max: 0,
            ..Default::default()
        });
        let result = std::panic::AssertUnwindSafe(measure(&system, &expected, probe_expected))
            .catch_unwind()
            .await;
        let shutdown = system.shutdown_internal().await;
        let result = match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        };
        // Both receipts are collected, including when measurement failed.
        shutdown.expect("dedicated experiment system retirement");
        result.unwrap();
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Vec<u64>, Vec<ThreadId>, Vec<Work>, Vec<Sample>) {
        let owners: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| std::thread::current().id())
                    .join()
                    .unwrap()
            })
            .collect();
        let start = Instant::now();
        let expected = vec![7, 7, 11, 11]; // Equal values cannot prove distinct work identities.
        let work = owners
            .iter()
            .enumerate()
            .map(|(salt, &owner)| Work {
                salt,
                value: expected[salt],
                owner,
                started: start,
                completed: start + Duration::from_secs(2),
            })
            .collect();
        let probes = (0..100)
            .map(|_| Sample {
                owner: std::thread::current().id(),
                started: start + Duration::from_millis(10),
                completed: start + Duration::from_millis(11),
            })
            .collect();
        (expected, owners, work, probes)
    }
    #[test]
    fn complete_distinct_work_with_overlapping_probes_is_valid() {
        let (e, o, w, p) = fixture();
        validate(&e, &o, &w, &p).unwrap();
    }
    #[test]
    fn missing_or_wrong_heavy_results_are_rejected() {
        let (e, o, mut w, p) = fixture();
        w[0].value = 0;
        assert!(validate(&e, &o, &w, &p).is_err());
        w[0].value = e[0];
        w.pop();
        assert!(validate(&e, &o, &w, &p).is_err());
    }
    #[test]
    fn duplicate_identity_is_rejected_even_with_equal_values() {
        let (e, o, mut w, p) = fixture();
        w[1] = w[0].clone();
        assert!(validate(&e, &o, &w, &p).is_err());
    }
    #[test]
    fn shared_owner_is_rejected() {
        let (e, mut o, w, p) = fixture();
        o[1] = o[0];
        assert!(validate(&e, &o, &w, &p).is_err());
    }
    #[test]
    fn probes_outside_work_do_not_prove_isolation() {
        let (e, o, w, mut p) = fixture();
        for sample in &mut p {
            sample.started += Duration::from_secs(3);
            sample.completed += Duration::from_secs(3);
        }
        assert!(validate(&e, &o, &w, &p).is_err());
    }
    #[test]
    fn pairwise_probe_progress_does_not_prove_four_way_overlap() {
        let (e, o, mut w, mut p) = fixture();
        w[3].started += Duration::from_secs(3);
        w[3].completed += Duration::from_secs(3);
        for sample in &mut p[50..] {
            sample.started += Duration::from_secs(3);
            sample.completed += Duration::from_secs(3);
        }
        assert!(validate(&e, &o, &w, &p).is_err());
    }
}
