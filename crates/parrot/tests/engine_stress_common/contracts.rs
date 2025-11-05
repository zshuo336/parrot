use super::*;
use futures::StreamExt;
use parrot_api::address::ActorRef;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedMessage};
use std::sync::atomic::AtomicBool;

#[derive(Default)]
struct Owners {
    created: AtomicU64,
    destroyed: AtomicU64,
}
pub struct ActorOwner(Arc<Owners>);
impl Drop for ActorOwner {
    fn drop(&mut self) {
        self.0.destroyed.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Default)]
pub struct ActorScope {
    actors: Mutex<Vec<BoxedActorRef>>,
    owners: Arc<Owners>,
}
impl ActorScope {
    pub fn owner(&self) -> ActorOwner {
        self.owners.created.fetch_add(1, Ordering::SeqCst);
        ActorOwner(self.owners.clone())
    }
    pub fn track(&self, actor: &dyn ActorRef) {
        self.actors.lock().unwrap().push(actor.clone_boxed());
    }
    pub fn counts(&self) -> (u64, u64) {
        (
            self.owners.created.load(Ordering::SeqCst),
            self.owners.destroyed.load(Ordering::SeqCst),
        )
    }
    pub async fn cleanup(&self) -> anyhow::Result<()> {
        let actors = std::mem::take(&mut *self.actors.lock().unwrap());
        let results: Vec<_> = futures::stream::iter(actors.into_iter().map(retire_actor))
            .buffer_unordered(64)
            .collect()
            .await;
        // Retire every peer even when one stop reports a failure.
        let errors: Vec<_> = results
            .into_iter()
            .filter_map(Result::err)
            .map(|e| e.to_string())
            .collect();
        let retired = wait_until(
            || {
                let (created, destroyed) = self.counts();
                created == destroyed
            },
            Duration::from_secs(15),
            Duration::from_millis(1),
        )
        .await;
        anyhow::ensure!(
            errors.is_empty() && retired,
            "actor cleanup failed: {errors:?}; created/destroyed={:?}",
            self.counts()
        );
        Ok(())
    }
}

async fn retire_actor(actor: BoxedActorRef) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(15), actor.stop()).await??;
    anyhow::ensure!(
        !actor.is_alive().await,
        "{} remained alive after stop",
        actor.path()
    );
    Ok(())
}

/// Record receipt before downcasting or doing result validation.
pub async fn checked_u64(
    actor: &dyn ActorRef,
    message: BoxedMessage,
    expected: u64,
) -> anyhow::Result<u128> {
    let started = Instant::now();
    let value = actor
        .send_with_timeout(message, Some(STRESS_REQUEST_TIMEOUT))
        .await?;
    let completed = Instant::now();
    verify_u64(value, expected)?;
    Ok(completed.duration_since(started).as_micros())
}
pub fn verify_u64(value: BoxedMessage, expected: u64) -> anyhow::Result<()> {
    let value = value
        .downcast::<u64>()
        .map_err(|_| anyhow::anyhow!("wrong reply type; expected u64"))?;
    anyhow::ensure!(
        *value == expected,
        "wrong reply value: actual={}, expected={expected}",
        *value
    );
    Ok(())
}

pub struct GatedWork {
    gate: PauseForTimeout,
    cancelled: Arc<AtomicBool>,
    pub iterations: u64,
    pub salt: u64,
    pub chunk: Option<u64>,
}
impl GatedWork {
    fn enter(&self) -> ActorResult<bool> {
        self.gate.execute()?;
        Ok(!self.cancelled.load(Ordering::SeqCst))
    }
    pub fn solid(&self) -> ActorResult<Option<TimedWorkResult>> {
        if !self.enter()? {
            return Ok(None);
        }
        let started = Instant::now();
        let value = burn_cpu(self.iterations, self.salt);
        Ok(Some(TimedWorkResult {
            value,
            started,
            completed: Instant::now(),
        }))
    }
    pub async fn chunked(&self) -> ActorResult<Option<TimedWorkResult>> {
        if !self.enter()? {
            return Ok(None);
        }
        let chunk = self.chunk.unwrap_or(self.iterations).max(1);
        let started = Instant::now();
        let mut value = self.salt | 1;
        let mut done = 0;
        while done < self.iterations {
            let take = chunk.min(self.iterations - done);
            value = burn_cpu_range(done, take, value);
            done += take;
            tokio::task::yield_now().await;
        }
        Ok(Some(TimedWorkResult {
            value,
            started,
            completed: Instant::now(),
        }))
    }
}
pub struct ProbeMessage {
    pub salt: u64,
    reply: Mutex<Option<tokio::sync::oneshot::Sender<(u64, Instant)>>>,
}
impl ProbeMessage {
    pub fn execute(&self) -> u64 {
        let value = burn_cpu(1000, self.salt);
        let executed = Instant::now();
        if let Some(reply) = self.reply.lock().unwrap().take() {
            let _ = reply.send((value, executed));
        }
        value
    }
}

pub struct QueuedProbe {
    release: Option<std::sync::mpsc::Sender<()>>,
    work: Option<tokio::task::JoinHandle<ActorResult<BoxedMessage>>>,
    received: Option<tokio::sync::oneshot::Receiver<(u64, Instant)>>,
    cancelled: Arc<AtomicBool>,
    submitted: Instant,
    admitted: Instant,
    expected_probe: u64,
}
impl Drop for QueuedProbe {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.release.take();
        if let Some(work) = &self.work {
            work.abort();
        }
    }
}
pub struct BusyMeasurement {
    pub work: TimedWorkResult,
    pub probe: ProbeSample,
    pub admitted: Instant,
    pub handled: Instant,
}
impl QueuedProbe {
    /// `deliver` proves admission while the long handler is still gated. Merely
    /// polling an ask future or starting a task would not prove enqueueing.
    pub async fn start(
        actor: &dyn ActorRef,
        iterations: u64,
        salt: u64,
        chunk: Option<u64>,
        probe_salt: u64,
    ) -> anyhow::Result<Self> {
        let expected_probe = oracle::reference(1000, probe_salt);
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let message = GatedWork {
            gate: PauseForTimeout {
                entered: Mutex::new(Some(entered)),
                released: Mutex::new(released),
            },
            cancelled: cancelled.clone(),
            iterations,
            salt,
            chunk,
        };
        let worker = actor.clone_boxed();
        let work = tokio::spawn(async move {
            worker
                .send_with_timeout(Box::new(message), Some(STRESS_REQUEST_TIMEOUT))
                .await
        });
        let (reply, received) = tokio::sync::oneshot::channel();
        let now = Instant::now();
        let mut pending = Self {
            release: Some(release),
            work: Some(work),
            received: Some(received),
            cancelled,
            submitted: now,
            admitted: now,
            expected_probe,
        };
        let admission = async {
            tokio::time::timeout(Duration::from_secs(5), entry).await??;
            pending.submitted = Instant::now();
            tokio::time::timeout(
                Duration::from_secs(5),
                actor.deliver(Box::new(ProbeMessage {
                    salt: probe_salt,
                    reply: Mutex::new(Some(reply)),
                })),
            )
            .await??;
            pending.admitted = Instant::now();
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = admission {
            pending.cancelled.store(true, Ordering::SeqCst);
            pending.release.take();
            // Retain/join the submitted request even on setup failure.
            if let Some(work) = pending.work.as_mut() {
                let _ = work.await;
            }
            pending.work.take();
            return Err(error);
        }
        Ok(pending)
    }
    pub async fn finish(mut self) -> anyhow::Result<BusyMeasurement> {
        self.release.take();
        let reply =
            tokio::time::timeout(STRESS_REQUEST_TIMEOUT, self.received.take().unwrap()).await;
        let completed = Instant::now();
        let work = self.work.as_mut().unwrap().await;
        self.work.take();
        let (value, handled) = reply??;
        let work = *work??
            .downcast::<TimedWorkResult>()
            .map_err(|_| anyhow::anyhow!("wrong gated work result type"))?;
        anyhow::ensure!(value == self.expected_probe, "wrong queued probe result");
        anyhow::ensure!(
            self.admitted <= work.started,
            "probe was not admitted before CPU work"
        );
        anyhow::ensure!(
            handled >= work.completed,
            "same actor processed probe before its long work finished"
        );
        Ok(BusyMeasurement {
            work,
            probe: ProbeSample {
                started: self.submitted,
                completed,
            },
            admitted: self.admitted,
            handled,
        })
    }
}

pub async fn mixed_workload(
    engine: &'static str,
    long_refs: Vec<BoxedActorRef>,
    backup_refs: Vec<BoxedActorRef>,
    probe: BoxedActorRef,
    expected: Arc<std::collections::HashMap<(u64, u64), u64>>,
) -> anyhow::Result<BenchResult> {
    anyhow::ensure!(
        long_refs.len() == 8 && backup_refs.len() == 4,
        "mixed workload actor count"
    );
    let start = Instant::now();
    let mut work = tokio::task::JoinSet::new();
    for (i, actor) in long_refs.into_iter().enumerate() {
        let expected = expected.clone();
        work.spawn(async move {
            let salt = 0xD00D + i as u64;
            let latency = checked_u64(
                actor.as_ref(),
                Box::new(MinuteCpuTask {
                    iterations: 57_200_000_000,
                    salt,
                }),
                expected[&(57_200_000_000, salt)],
            )
            .await?;
            anyhow::Ok(vec![latency])
        });
    }
    for (i, actor) in backup_refs.into_iter().enumerate() {
        let expected = expected.clone();
        work.spawn(async move {
            let mut samples = Vec::new();
            for k in 0..10 {
                let salt = 0xBACC + i as u64 * 100 + k;
                samples.push(
                    checked_u64(
                        actor.as_ref(),
                        Box::new(MediumCpuTask {
                            iterations: 2_400_000_000,
                            salt,
                        }),
                        expected[&(2_400_000_000, salt)],
                    )
                    .await?,
                );
            }
            anyhow::Ok(samples)
        });
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let probe_expected = expected[&(1000, 0xE)];
    let probing = tokio::spawn(async move {
        let mut samples = Vec::new();
        while !flag.load(Ordering::Relaxed) {
            samples.push(
                checked_u64(
                    probe.as_ref(),
                    Box::new(TinyTask { salt: 0xE }),
                    probe_expected,
                )
                .await?,
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        anyhow::Ok(samples)
    });
    let mut samples = Vec::new();
    let mut errors = Vec::new();
    while let Some(result) = work.join_next().await {
        match result {
            Ok(Ok(values)) => samples.extend(values),
            other => errors.push(format!("{other:?}")),
        }
    }
    stop.store(true, Ordering::Relaxed);
    let probes = probing.await??;
    let elapsed = start.elapsed();
    let count = probes.len();
    Ok(BenchResult {
        unit: "messages",
        name: "mixed-minute-cpu-plus-incoming".into(),
        engine,
        total_messages: 48 + count as u64,
        elapsed,
        latencies: latency_stats(probes),
        correctness: errors.is_empty() && samples.len() == 48 && count > 0,
        note: format!(
            "48 verified work replies (8×57.2G + 40×2.4G iterations), plus {count} probes; latency samples are only probe round trips, recorded before validation; errors={errors:?}"
        ),
    })
}

pub async fn mixed_backlog(
    engine: &'static str,
    actor: BoxedActorRef,
    expected: Arc<std::collections::HashMap<(u64, u64), u64>>,
) -> anyhow::Result<BenchResult> {
    let start = Instant::now();
    // Queue the probe while only the gated long work is outstanding. Producers
    // start afterwards, so mailbox capacity cannot deadlock admission of the probe.
    let probe = QueuedProbe::start(actor.as_ref(), 30_000_000_000, 1, None, 5).await?;
    let mut tasks = tokio::task::JoinSet::new();
    for k in 0..6 {
        let actor = actor.clone_boxed();
        let expected = expected.clone();
        tasks.spawn(async move {
            checked_u64(
                actor.as_ref(),
                Box::new(MediumCpuTask {
                    iterations: 2_400_000_000,
                    salt: 100 + k,
                }),
                expected[&(2_400_000_000, 100 + k)],
            )
            .await
        });
    }
    for k in 0..4000 {
        let actor = actor.clone_boxed();
        let expected_value = expected[&(1000, k)];
        tasks.spawn(async move {
            checked_u64(
                actor.as_ref(),
                Box::new(TinyTask { salt: k }),
                expected_value,
            )
            .await
        });
    }
    let measured = probe.finish().await;
    let mut errors = Vec::new();
    let mut completed = 0;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(_)) => completed += 1,
            other => errors.push(format!("{other:?}")),
        }
    }
    let measured = measured?;
    anyhow::ensure!(
        measured.work.value == expected[&(30_000_000_000, 1)],
        "long backlog reply value"
    );
    Ok(BenchResult {
        unit: "messages",
        name: "mixed-same-actor-backlog-probe".into(),
        engine,
        total_messages: 4008,
        elapsed: start.elapsed(),
        latencies: latency_stats(vec![
            measured
                .probe
                .completed
                .duration_since(measured.probe.started)
                .as_micros(),
        ]),
        correctness: completed == 4006 && errors.is_empty(),
        note: format!(
            "probe admitted by deliver while long handler was gated; then 6×2.4G + 4000 tiny requests submitted; no claim that every producer precedes probe. Long CPU interval={:.3}s; errors={errors:?}",
            measured
                .work
                .completed
                .duration_since(measured.work.started)
                .as_secs_f64()
        ),
    })
}

pub async fn compare_chunked(
    engine: &'static str,
    solid: &dyn ActorRef,
    chunked: &dyn ActorRef,
    total: u64,
    chunk: u64,
    expected: u64,
) -> anyhow::Result<BenchResult> {
    let start = Instant::now();
    let solid = QueuedProbe::start(solid, total, 3, None, 1)
        .await?
        .finish()
        .await?;
    let chunked = QueuedProbe::start(chunked, total, 3, Some(chunk), 2)
        .await?
        .finish()
        .await?;
    anyhow::ensure!(
        solid.work.value == expected && chunked.work.value == expected,
        "fixed computation differs"
    );
    Ok(BenchResult {
        unit: "messages",
        name: "chunked-vs-solid-longrun".into(),
        engine,
        total_messages: 4,
        elapsed: start.elapsed(),
        latencies: latency_stats(vec![
            solid
                .probe
                .completed
                .duration_since(solid.probe.started)
                .as_micros(),
            chunked
                .probe
                .completed
                .duration_since(chunked.probe.started)
                .as_micros(),
        ]),
        correctness: true,
        note: format!(
            "same {total} iterations, chunk={chunk}; probes admitted via deliver before CPU starts and handled after completion; solid={:.3}s chunked={:.3}s. Yielding does not make this actor reentrant.",
            solid
                .work
                .completed
                .duration_since(solid.work.started)
                .as_secs_f64(),
            chunked
                .work
                .completed
                .duration_since(chunked.work.started)
                .as_secs_f64()
        ),
    })
}
