//! Observe real shard owners and actual CPU work intervals before drawing any
//! conclusion about cross-shard progress. Timing is descriptive, not a hard SLA.
use super::{burn_cpu, cfg_sharded, oracle};
use anyhow::{Context, Result, anyhow, ensure};
use parrot::thread::{
    address::ThreadActorRef, config::ThreadActorSystemConfig, context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::ThreadId,
    time::{Duration, Instant},
};

enum Request {
    Probe(u64),
    Work {
        iterations: u64,
        salt: u64,
        entered: tokio::sync::mpsc::UnboundedSender<()>,
    },
}
#[derive(Clone)]
struct Work {
    salt: u64,
    value: u64,
    owner: ThreadId,
    started: Instant,
    completed: Instant,
}
struct Probe {
    value: u64,
    owner: ThreadId,
}
struct Worker;
impl Actor for Worker {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let request = message.downcast::<Request>().map_err(|_| {
                ActorError::MessageHandlingError("unknown isolation request".into())
            })?;
            let owner = std::thread::current().id();
            match *request {
                Request::Probe(value) => Ok(Box::new(Probe { value, owner }) as BoxedMessage),
                Request::Work {
                    iterations,
                    salt,
                    entered,
                } => {
                    let started = Instant::now();
                    entered.send(()).map_err(|_| {
                        ActorError::MessageHandlingError("work observer closed".into())
                    })?;
                    let value = burn_cpu(iterations, salt);
                    let completed = Instant::now();
                    Ok(Box::new(Work {
                        salt,
                        value,
                        owner,
                        started,
                        completed,
                    }) as BoxedMessage)
                }
            }
        })
    }
}

async fn probe(actor: &ThreadActorRef<Worker>, value: u64) -> Result<Probe> {
    let reply = actor
        .ask_with_timeout(Box::new(Request::Probe(value)), Duration::from_secs(10))
        .await?;
    let reply = reply
        .downcast::<Probe>()
        .map_err(|_| anyhow!("probe reply type"))?;
    ensure!(reply.value == value, "probe result changed");
    Ok(*reply)
}

#[derive(Clone)]
struct Sample {
    started: Instant,
    completed: Instant,
    owner: ThreadId,
}

fn validate(
    expected: &[u64],
    works: &[Work],
    samples: &[Sample],
    heavy_owner: ThreadId,
    probe_owner: ThreadId,
) -> Result<Vec<usize>> {
    ensure!(
        heavy_owner != probe_owner,
        "probe and load share an owner thread"
    );
    ensure!(
        !expected.is_empty() && works.len() == expected.len(),
        "missing heavy work results"
    );
    ensure!(
        samples
            .iter()
            .all(|s| s.owner == probe_owner && s.started <= s.completed),
        "probe owner or interval changed"
    );
    let mut seen = vec![false; expected.len()];
    let mut completions = Vec::new();
    for work in works {
        let index = usize::try_from(work.salt)?;
        ensure!(
            index < expected.len() && !seen[index],
            "duplicate or unknown work identity"
        );
        seen[index] = true;
        ensure!(work.value == expected[index], "wrong heavy work result");
        ensure!(work.owner == heavy_owner, "heavy owner changed");
        ensure!(work.started < work.completed, "empty heavy work interval");
        let inside = samples
            .iter()
            .filter(|s| s.started >= work.started && s.completed < work.completed)
            .count();
        ensure!(
            inside > 0,
            "no complete probe request inside heavy work interval"
        );
        completions.push(inside);
    }
    let mut ordered: Vec<_> = works.iter().collect();
    ordered.sort_unstable_by_key(|w| w.started);
    ensure!(
        ordered.windows(2).all(|w| w[0].completed <= w[1].started),
        "synchronous work on one owner overlapped"
    );
    Ok(completions)
}

async fn measure(system: &Arc<ThreadActorSystem>, iterations: u64, expected: &[u64]) -> Result<()> {
    let mut heavy = Vec::new();
    let mut owner = None;
    for salt in 0..expected.len() {
        let actor = system
            .spawn_at(
                Worker,
                &format!("/isolation/heavy/{salt}"),
                None,
                cfg_sharded("domain-heavy"),
            )
            .await?;
        let observed = probe(&actor, salt as u64).await?.owner;
        ensure!(
            owner.is_none_or(|id| id == observed),
            "same affinity key migrated"
        );
        owner = Some(observed);
        heavy.push(actor);
    }
    let heavy_owner = owner.context("no heavy actors")?;
    // Textually different keys can hash to the same shard. Observe the actual
    // owner, including the legacy key, and retire unsuitable candidates.
    let mut chosen = None;
    let keys = std::iter::once("domain-probe".to_owned())
        .chain((0..256).map(|n| format!("isolation-probe-{n}")));
    for (index, key) in keys.enumerate() {
        let path = format!("/isolation/probe/{index}");
        let actor = system
            .spawn_at(Worker, &path, None, cfg_sharded(&key))
            .await?;
        let observed = probe(&actor, index as u64).await?.owner;
        if observed != heavy_owner {
            chosen = Some((actor, observed, key));
            break;
        }
        system.stop_actor(&path).await?;
    }
    let (actor, probe_owner, key) = chosen.context(
        "no distinct shard found; this experiment requires at least two available shard threads",
    )?;
    ensure!(
        heavy_owner != probe_owner,
        "probe and load share an owner thread"
    );
    let mut baseline = Vec::new();
    for value in 0..100 {
        let started = Instant::now();
        let observed = probe(&actor, value).await?;
        ensure!(observed.owner == probe_owner, "baseline probe migrated");
        baseline.push(started.elapsed().as_micros());
    }

    let (entered, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut jobs = Vec::new();
    for (salt, worker) in heavy.into_iter().enumerate() {
        let entered = entered.clone();
        jobs.push(tokio::spawn(async move {
            let reply = worker
                .ask_with_timeout(
                    Box::new(Request::Work {
                        iterations,
                        salt: salt as u64,
                        entered,
                    }),
                    Duration::from_secs(180),
                )
                .await?;
            reply
                .downcast::<Work>()
                .map(|w| *w)
                .map_err(|_| anyhow!("heavy reply type"))
        }));
    }
    drop(entered);
    let done = AtomicBool::new(false);
    let collect = async {
        // Collect every join and business result before propagating errors.
        let results = futures::future::join_all(jobs).await;
        done.store(true, Ordering::Release);
        results
            .into_iter()
            .map(|r| r.map_err(anyhow::Error::from).and_then(|r| r))
            .collect::<Result<Vec<_>>>()
    };
    let observe = async {
        tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
            .await?
            .context("heavy work never entered")?;
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut samples = Vec::new();
        while !done.load(Ordering::Acquire) {
            ensure!(Instant::now() < deadline, "probe observation deadline");
            let started = Instant::now();
            let observed = probe(&actor, samples.len() as u64).await?;
            let completed = Instant::now();
            samples.push(Sample {
                started,
                completed,
                owner: observed.owner,
            });
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Ok::<_, anyhow::Error>(samples)
    };
    let (works, samples) = tokio::join!(collect, observe);
    let works = works?;
    let samples = samples?;
    let inside = validate(expected, &works, &samples, heavy_owner, probe_owner)?;
    let baseline = super::latency_stats(baseline);
    let loaded = super::latency_stats(
        samples
            .iter()
            .filter(|s| {
                works
                    .iter()
                    .any(|w| s.started < w.completed && s.completed > w.started)
            })
            .map(|s| s.completed.duration_since(s.started).as_micros())
            .collect(),
    );
    println!(
        "[S2 verified] heavy_owner={heavy_owner:?} probe_owner={probe_owner:?} probe_key={key} iterations={iterations} exact_results={} probes_inside_each_work={inside:?} baseline_p99_us={} overlapping_p99_us={} overlapping_max_us={}",
        works.len(),
        baseline.p99_us,
        loaded.p99_us,
        loaded.max_us
    );
    Ok(())
}

fn try_run(seconds: f64) -> Result<()> {
    // Calibration and independent expected outputs finish before the load
    // window; no calibration jobs compete with probes or masquerade as actors.
    let iterations = super::calibrated_iters(seconds);
    let expected: Vec<_> = (0..4)
        .map(|salt| oracle::reference(iterations, salt))
        .collect();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
        let measured = measure(&system, iterations, &expected).await;
        let retired = system.shutdown_internal().await;
        // Retirement is required on both success and a failed measurement.
        retired.context("isolation system retirement failed")?;
        println!("[S2 retirement] ok; measurement_ok={}", measured.is_ok());
        measured
    })
}

#[test]
fn actual_work_validates_progress_or_missing_topology() {
    let result = try_run(0.2);
    if std::thread::available_parallelism().unwrap().get() == 1 {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no distinct shard found")
        );
    } else {
        result.expect("actual CPU work must have exact results and cross-shard progress");
    }
}

pub(super) fn run(seconds: f64) {
    try_run(seconds).expect("invalid isolation measurement");
}

fn fixture() -> (Vec<u64>, Vec<Work>, Vec<Sample>, ThreadId, ThreadId) {
    let heavy = std::thread::current().id();
    let probe = std::thread::spawn(|| std::thread::current().id())
        .join()
        .unwrap();
    let start = Instant::now();
    (
        vec![42],
        vec![Work {
            salt: 0,
            value: 42,
            owner: heavy,
            started: start,
            completed: start + Duration::from_secs(1),
        }],
        vec![Sample {
            started: start + Duration::from_millis(10),
            completed: start + Duration::from_millis(20),
            owner: probe,
        }],
        heavy,
        probe,
    )
}

#[test]
fn measurement_requires_actual_progress_inside_the_work_window() {
    let (expected, works, samples, heavy, probe) = fixture();
    assert_eq!(
        validate(&expected, &works, &samples, heavy, probe).unwrap(),
        vec![1]
    );
    for after in [false, true] {
        let mut samples = samples.clone();
        let offset = if after {
            Duration::from_secs(2)
        } else {
            Duration::ZERO
        };
        samples[0].started = works[0].started + offset;
        samples[0].completed = samples[0].started;
        if !after {
            samples[0].started -= Duration::from_millis(1);
        }
        assert!(validate(&expected, &works, &samples, heavy, probe).is_err());
    }
    assert!(validate(&expected, &works, &[], heavy, probe).is_err());
}

#[test]
fn measurement_rejects_missing_wrong_duplicate_and_misrouted_work() {
    for fault in 0..6 {
        let (expected, mut works, samples, heavy, probe) = fixture();
        match fault {
            0 => works.clear(),
            1 => works[0].value = 0,
            2 => works[0].salt = 1,
            3 => works[0].owner = probe,
            4 => works[0].completed = works[0].started,
            _ => {
                works.push(works[0].clone());
            }
        }
        assert!(
            validate(&expected, &works, &samples, heavy, probe).is_err(),
            "fault {fault}"
        );
    }
}

#[test]
fn measurement_rejects_same_shard_and_probe_migration() {
    let (expected, works, mut samples, heavy, probe) = fixture();
    assert!(validate(&expected, &works, &samples, heavy, heavy).is_err());
    samples[0].owner = heavy;
    assert!(validate(&expected, &works, &samples, heavy, probe).is_err());
}

#[test]
fn duplicate_identity_is_rejected_even_with_valid_values_and_disjoint_intervals() {
    let (mut expected, mut works, mut samples, heavy, probe) = fixture();
    // Salts 0 and 1 really share a reference value because the recurrence starts
    // at salt | 1. Correct counts, values and timing cannot detect substitution.
    expected.push(expected[0]);
    let mut second = works[0].clone();
    second.salt = 1;
    second.started += Duration::from_secs(2);
    second.completed += Duration::from_secs(2);
    works.push(second);
    let mut second_probe = samples[0].clone();
    second_probe.started += Duration::from_secs(2);
    second_probe.completed += Duration::from_secs(2);
    samples.push(second_probe);
    assert_eq!(
        validate(&expected, &works, &samples, heavy, probe).unwrap(),
        vec![1, 1]
    );
    works[1].salt = 0;
    let error = validate(&expected, &works, &samples, heavy, probe).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("duplicate or unknown work identity")
    );
}
