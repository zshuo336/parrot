use super::*;

pub struct StartGate {
    entered: tokio::sync::oneshot::Sender<()>,
    released: std::sync::mpsc::Receiver<()>,
}
impl StartGate {
    pub(super) fn wait(self) {
        self.entered.send(()).expect("gate observer disappeared");
        match self.released.recv_timeout(Duration::from_secs(10)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            Err(error) => panic!("gate was not released: {error}"),
        }
    }
}
pub(super) fn start_gate() -> (
    StartGate,
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, receipt) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    (StartGate { entered, released }, receipt, release)
}

pub struct WorkResult {
    pub(super) value: u64,
    pub(super) started: Instant,
    pub(super) completed: Instant,
}
#[derive(Message)]
#[rtype(result = "WorkResult")]
pub(super) struct GatedCpu {
    pub(super) gate: StartGate,
    pub(super) iters: u64,
    pub(super) salt: u64,
}
impl Handler<GatedCpu> for RawBenchActor {
    type Result = MessageResult<GatedCpu>;
    fn handle(&mut self, m: GatedCpu, _: &mut Self::Context) -> Self::Result {
        m.gate.wait();
        self.ops.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let value = burn_cpu(m.iters, m.salt);
        let completed = Instant::now();
        self.cpu_sink.fetch_add(value, Ordering::Relaxed);
        self.ops.fetch_add(1, Ordering::Relaxed);
        MessageResult(WorkResult {
            value,
            started,
            completed,
        })
    }
}

pub(super) struct ChunkComparison {
    pub(super) elapsed: Duration,
    pub(super) solid: Duration,
    pub(super) chunked: Duration,
    pub(super) solid_probe: Duration,
    pub(super) chunk_probe: Duration,
    pub(super) solid_value: u64,
    pub(super) chunk_value: u64,
}
pub(super) async fn compare_chunked(pool: &ArbiterPool, total: u64, chunk: u64) -> ChunkComparison {
    let expected_solid_probe = oracle::reference(1000, 1);
    let expected_chunk_probe = oracle::reference(1000, 2);
    let experiment_start = Instant::now();
    let solid = spawn_in_pool(
        pool,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    );
    let life = pool.lifetime();
    let chunked: Addr<RawChunkActor> =
        actix::Actor::start_in_arbiter(&pool.next_arbiter(), move |_| RawChunkActor {
            _life: life,
        });
    let (gate, entered, release) = start_gate();
    let solid_request = solid.send(GatedCpu {
        gate,
        iters: total,
        salt: 3,
    });
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    let probe_start = Instant::now();
    let probe = solid.send(TinyTask { salt: 1 });
    drop(release);
    let value = probe.timeout(REQUEST_TIMEOUT).await.unwrap();
    let probe_completed = Instant::now();
    let solid_probe = probe_completed.duration_since(probe_start);
    assert_eq!(value, expected_solid_probe);
    let solid_work = solid_request.timeout(REQUEST_TIMEOUT).await.unwrap();
    assert!(
        probe_start <= solid_work.started,
        "probe must be queued before computation"
    );
    assert!(probe_completed >= solid_work.completed);

    let (gate, entered, release) = start_gate();
    let chunk_request = chunked.send(ChunkedLong {
        gate,
        total,
        chunk,
        salt: 3,
    });
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    let probe_start = Instant::now();
    let probe = chunked.send(TinyTask { salt: 2 });
    drop(release);
    let value = probe.timeout(REQUEST_TIMEOUT).await.unwrap();
    let probe_completed = Instant::now();
    let chunk_probe = probe_completed.duration_since(probe_start);
    assert_eq!(value, expected_chunk_probe);
    let chunk_work = chunk_request.timeout(REQUEST_TIMEOUT).await.unwrap();
    assert!(
        probe_start <= chunk_work.started,
        "probe must be queued before computation"
    );
    assert!(
        probe_completed >= chunk_work.completed,
        "atomic handler must not process the probe before long work completes"
    );
    ChunkComparison {
        elapsed: experiment_start.elapsed(),
        solid: solid_work.completed.duration_since(solid_work.started),
        chunked: chunk_work.completed.duration_since(chunk_work.started),
        solid_probe,
        chunk_probe,
        solid_value: solid_work.value,
        chunk_value: chunk_work.value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(correctness: bool) -> BenchResult {
        BenchResult {
            name: "receipt".into(),
            engine: "actix-raw",
            total_messages: 1,
            elapsed: Duration::from_secs(4),
            latencies: latency_stats(vec![2, 3, 900]),
            correctness,
            note: "completion evidence".into(),
        }
    }
    #[test]
    fn report_rejects_empty_and_failed_runs_even_after_rendering() {
        assert!(Report::new().verify().is_err());
        let report = Report::new();
        report.push(row(false));
        report.push(row(true));
        assert!(report.dump_markdown().contains("completion evidence"));
        assert!(report.verify().unwrap_err().contains("receipt"));
        assert!(report.verify().is_err());
    }
    #[test]
    fn unmeasured_times_are_na_and_small_nonzero_rates_survive_rounding() {
        let r = row(true);
        assert_eq!(r.msg_per_sec(), Some(0.25));
        let report = Report::new();
        report.push(r);
        assert!(report.dump_markdown().contains("0.25"));
        let mut r = row(true);
        r.elapsed = Duration::ZERO;
        r.latencies = latency_stats(vec![]);
        assert_eq!(r.msg_per_sec(), None);
        let report = Report::new();
        report.push(r);
        assert!(report.dump_markdown().contains("n/a"));
        report.verify().unwrap();
    }
    #[test]
    fn percentiles_use_only_actual_samples_and_keep_tails() {
        let mut v = vec![1; 98];
        v.extend([500, 1000]);
        let s = latency_stats(v);
        assert_eq!((s.count, s.p50_us, s.p99_us, s.max_us), (100, 1, 500, 1000));
        assert_eq!(latency_stats(vec![]).count, 0);
    }
    #[test]
    fn fixed_work_matches_independent_literal_vectors_and_never_stops_at_42() {
        // Python arbitrary-precision recurrence, masked after each iteration.
        assert_eq!(burn_cpu(107, 3), 4046730678074085073);
        assert_eq!(burn_cpu(2, 3639755004476521255), 10481999410520546992);
        assert_eq!(burn_cpu(0, 0), 1);
    }
    #[test]
    fn independent_reference_matches_literal_cases_and_kernel() {
        assert_eq!(oracle::reference(107, 3), 4046730678074085073);
        assert_eq!(
            oracle::reference(2, 3639755004476521255),
            10481999410520546992
        );
        for salt in [0, 1, 3, u64::MAX] {
            for n in [0, 1, 2, 107, 10_000] {
                assert_eq!(oracle::reference(n, salt), burn_cpu(n, salt));
            }
        }
    }
    #[actix::test]
    async fn fire_and_forget_is_verified_by_business_checksum_after_mailbox_barrier() {
        let mut pool = ArbiterPool::new(1);
        let ops = Arc::new(AtomicU64::new(0));
        let actor = spawn_in_pool(&pool, ops.clone(), Arc::new(AtomicU64::new(0)));
        for v in 0..1000 {
            actor.do_send(Echo { v });
        }
        assert_eq!(
            actor
                .send(GetEchoSum)
                .timeout(REQUEST_TIMEOUT)
                .await
                .unwrap(),
            499500
        );
        assert_eq!(ops.load(Ordering::Acquire), 1000);
        drop(actor);
        pool.drain().await;
        assert!(pool.shutdown());
    }
    #[actix::test]
    async fn io_handler_replies_and_retires_its_owner() {
        let mut pool = ArbiterPool::new(1);
        let done = Arc::new(AtomicU64::new(0));
        let done_actor = done.clone();
        let life = pool.lifetime();
        let actor: Addr<RawIoActor> =
            actix::Actor::start_in_arbiter(&pool.next_arbiter(), move |_| RawIoActor {
                _life: life,
                done: done_actor,
            });
        let first = actor.send(Echo { v: 1 });
        let second = actor.send(Echo { v: 1 });
        assert_eq!(first.await.unwrap(), 1);
        assert_eq!(second.await.unwrap(), 1);
        assert_eq!(done.load(Ordering::Relaxed), 2);
        drop(actor);
        pool.drain().await;
        assert!(pool.shutdown());
    }
    #[test]
    fn split_ranges_preserve_all_iterations_including_short_final_chunk() {
        for salt in [0, 3, u64::MAX] {
            for chunk in [1, 8, 107, 200] {
                let mut state = salt | 1;
                let mut done = 0;
                while done < 107 {
                    let take = chunk.min(107 - done);
                    state = burn_cpu_range(done, take, state);
                    done += take;
                }
                assert_eq!(state, burn_cpu(107, salt));
            }
        }
    }
    #[test]
    fn probe_window_retains_requests_stalled_past_the_entire_interval() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let samples = [
            ProbeSample {
                started: ms(0),
                completed: ms(5),
            },
            ProbeSample {
                started: ms(9),
                completed: ms(31),
            },
            ProbeSample {
                started: ms(12),
                completed: ms(14),
            },
        ];
        let w = probe_window(&samples, ms(10), ms(30)).unwrap();
        assert_eq!(
            (w.baseline.count, w.overlapping.count, w.completed),
            (1, 2, 1)
        );
        assert_eq!(w.overlapping.max_us, 22000);
        assert!(probe_window(&[], ms(30), ms(10)).is_err());
        assert!(probe_window(
            &[ProbeSample {
                started: ms(5),
                completed: ms(4)
            }],
            ms(0),
            ms(10)
        )
        .is_err());
    }
    #[actix::test]
    async fn real_atomic_chunk_actor_uses_same_computation_and_serializes_probe() {
        let mut pool = ArbiterPool::new(1);
        for (total, chunk) in [(107, 8), (200_003, 8_000), (107, 200)] {
            let r = compare_chunked(&pool, total, chunk).await;
            assert_eq!(r.solid_value, burn_cpu(total, 3));
            assert_eq!(r.chunk_value, r.solid_value);
            pool.drain().await;
        }
        assert!(pool.shutdown());
    }
    #[actix::test]
    async fn timeout_requires_a_live_gated_handler_and_releases_it() {
        let mut pool = ArbiterPool::new(1);
        let actor = spawn_in_pool(
            &pool,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );
        let (gate, entered, release) = start_gate();
        let work = actor.send(GatedCpu {
            gate,
            iters: 107,
            salt: 3,
        });
        tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .unwrap()
            .unwrap();
        let reply = actor
            .send(Echo { v: 1 })
            .timeout(Duration::from_millis(1))
            .await;
        drop(release);
        assert_eq!(work.await.unwrap().value, 4046730678074085073);
        assert!(matches!(reply, Err(MailboxError::Timeout)));
        assert_eq!(actor.send(Echo { v: 77 }).await.unwrap(), 77);
        drop(actor);
        pool.drain().await;
        assert!(pool.shutdown());
    }
    #[actix::test]
    async fn checked_handlers_return_exact_results_and_stop_retires_owner() {
        let mut pool = ArbiterPool::new(1);
        let ops = Arc::new(AtomicU64::new(0));
        let sink = Arc::new(AtomicU64::new(0));
        let actor = spawn_in_pool(&pool, ops.clone(), sink.clone());
        let expected = burn_cpu(107, 3);
        assert_eq!(
            actor
                .send(CpuTask {
                    iters: 107,
                    salt: 3
                })
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            actor
                .send(MediumCpu {
                    iters: 107,
                    salt: 3
                })
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            actor
                .send(MinuteCpu {
                    iters: 107,
                    salt: 3
                })
                .await
                .unwrap(),
            expected
        );
        assert_eq!(sink.load(Ordering::Relaxed), expected.wrapping_mul(3));
        assert_eq!(ops.load(Ordering::Relaxed), 6);
        actor.send(StopNow).await.unwrap();
        assert!(matches!(
            actor.send(Echo { v: 1 }).await,
            Err(MailboxError::Closed)
        ));
        pool.drain().await;
        assert!(pool.shutdown());
    }
    #[actix::test]
    async fn dropped_startup_and_active_addresses_leave_no_actor_or_worker() {
        let mut pool = ArbiterPool::new(2);
        for _ in 0..100 {
            drop(spawn_in_pool(
                &pool,
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicU64::new(0)),
            ));
        }
        pool.drain().await;
        assert!(pool.shutdown());
        assert!(pool.arbiters.is_empty());
    }
}
