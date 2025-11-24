//! Linux-only validation probe; requires an isolated cgroup with 32..=96 PIDs.
//! It exhausts that cgroup's thread allowance and must never run on the host.
#[cfg(target_os = "linux")]
fn main() {
    use parrot_api::runtime::{ActorRuntime, RuntimeConfig, TokioActorRuntime};
    use std::{sync::mpsc, thread, time::Duration};

    fn threads() -> usize {
        std::fs::read_dir("/proc/self/task").unwrap().count()
    }

    fn wait_threads(expected: usize, label: &str) {
        let started = std::time::Instant::now();
        let first = threads();
        loop {
            let actual = threads();
            if actual == expected {
                println!(
                    "THREAD_VIEW {label}: first={first} expected={expected} settled_us={}",
                    started.elapsed().as_micros()
                );
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{label}: expected {expected} threads after joins, still observed {actual}"
            );
            // pthread_join completes before Linux necessarily removes the
            // exiting task's /proc entry. Observe convergence, not one sample.
            thread::sleep(Duration::from_millis(1));
        }
    }

    struct Exhausted {
        threads: Vec<(thread::JoinHandle<()>, mpsc::Sender<()>)>,
    }

    impl Exhausted {
        fn new(limit: usize) -> Self {
            let mut owner = Self {
                threads: Vec::new(),
            };
            for _ in 0..=limit {
                let (release, gate) = mpsc::channel();
                match thread::Builder::new()
                    .stack_size(128 * 1024)
                    .spawn(move || {
                        let _ = gate.recv();
                    }) {
                    Ok(thread) => owner.threads.push((thread, release)),
                    Err(error) => {
                        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                        assert!(!owner.threads.is_empty());
                        return owner;
                    }
                }
            }
            panic!("isolated cgroup did not enforce the declared PID limit");
        }

        fn release(&mut self, count: usize) {
            for _ in 0..count {
                let (thread, release) = self.threads.pop().unwrap();
                release.send(()).unwrap();
                thread.join().unwrap();
            }
        }
    }

    impl Drop for Exhausted {
        fn drop(&mut self) {
            for (thread, release) in self.threads.drain(..) {
                let _ = release.send(());
                thread.join().unwrap();
            }
        }
    }

    let mode = std::env::args().nth(1).unwrap_or_default();
    assert!(
        matches!(
            mode.as_str(),
            "--isolated-pid-probe" | "--isolated-worker-startup-probe"
        ),
        "explicit isolated probe mode required"
    );
    let limit: usize = std::fs::read_to_string("/sys/fs/cgroup/pids.max")
        .expect("isolated cgroup v2 PID limit required")
        .trim()
        .parse()
        .unwrap();
    assert!(
        (32..=96).contains(&limit),
        "refusing to exhaust an unconstrained host"
    );
    let baseline = threads();
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    host.block_on(async {
        let config = RuntimeConfig {
            worker_threads: Some(1),
            io_threads: Some(1),
            ..Default::default()
        };
        if mode == "--isolated-worker-startup-probe" {
            let lane = std::env::args().nth(2).expect("actor or io lane required");
            let slots: usize = std::env::args().nth(3).unwrap().parse().unwrap();
            assert!(matches!(lane.as_str(), "actor" | "io"));
            assert!((1..=6).contains(&slots));
            // Initialize the process join service before reserving exact PID headroom.
            TokioActorRuntime::start(config).await.unwrap().shutdown().await.unwrap();
            wait_threads(baseline + 1, "warm-service");
            let mut exhausted = Exhausted::new(limit);
            exhausted.release(slots);
            let reserved = baseline + 1 + exhausted.threads.len();
            wait_threads(reserved, "reserved-worker-slots");
            let denied = std::fs::read_to_string("/sys/fs/cgroup/pids.events").unwrap();
            let startup = RuntimeConfig {
                worker_threads: Some(if lane == "actor" { 4 } else { 1 }),
                io_threads: (lane == "io").then_some(4),
                ..Default::default()
            };
            let result = tokio::time::timeout(Duration::from_secs(10), TokioActorRuntime::start(startup))
                .await.expect("worker startup did not finish within 10 seconds");
            println!("WORKER_STARTUP lane={lane} slots={slots} live={} reserved={reserved} result={result:?}", threads());
            println!("PIDS_EVENTS before={denied:?} after={:?}", std::fs::read_to_string("/sys/fs/cgroup/pids.events").unwrap());
            let failed = match result {
                Ok(runtime) => {
                    runtime.spawn(async {}).await.unwrap();
                    runtime.shutdown().await.unwrap();
                    false
                }
                Err(error) => {
                    assert!(matches!(error, parrot_api::errors::ActorError::InitializationError(_)), "{error}");
                    true
                }
            };
            wait_threads(reserved, "partial-worker-rollback");
            drop(exhausted);
            wait_threads(baseline + 1, "partial-worker-released");
            assert!(failed, "startup accepted fewer OS workers than configured: lane={lane} slots={slots}");
            // Recovery must still accept real actor and I/O work after resources return.
            let recovered = TokioActorRuntime::start(RuntimeConfig {
                worker_threads: Some(4), io_threads: Some(4), ..Default::default()
            }).await.unwrap();
            assert_eq!(recovered.spawn(async { 41 }).await.unwrap(), 41);
            assert_eq!(recovered.spawn_io(async { 42 }).await.unwrap(), 42);
            recovered.shutdown().await.unwrap();
            wait_threads(baseline + 1, "post-failure-recovery");
            println!("WORKER_STARTUP_EXHAUSTION_OK lane={lane} slots={slots}");
            return;
        }
        let cold = Exhausted::new(limit);
        let error = TokioActorRuntime::start(config.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("failed to start Tokio join owner"),
            "{error}"
        );
        drop(cold);
        wait_threads(baseline, "cold-rollback");

        let runtime = TokioActorRuntime::start(config.clone()).await.unwrap();
        runtime.spawn(async {}).await.unwrap();
        runtime.spawn_io(async {}).await.unwrap();
        let warm = Exhausted::new(limit);
        let error = TokioActorRuntime::start(config)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("failed to start Tokio runtime owner"),
            "{error}"
        );
        tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
            .await
            .unwrap()
            .unwrap();
        wait_threads(baseline + 1 + warm.threads.len(), "exhausted-shutdown");
        drop(warm);
        wait_threads(baseline + 1, "warm-rollback");
    });
    if mode == "--isolated-pid-probe" {
        println!("PID_EXHAUSTION_OK: cold startup rollback, warm startup rollback, actor/I/O OS join; one process join service retained");
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("requires an isolated Linux cgroup v2 with 32..=96 PIDs");
    std::process::exit(2);
}
