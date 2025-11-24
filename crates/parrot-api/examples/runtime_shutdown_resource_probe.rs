//! Linux-only validation probe; requires an isolated cgroup with 32..=96 PIDs.
//! It exhausts that cgroup's thread allowance and must never run on the host.
#[cfg(target_os = "linux")]
fn main() {
    use parrot_api::runtime::{ActorRuntime, RuntimeConfig, TokioActorRuntime};
    use std::{
        sync::{Arc, Condvar, Mutex},
        thread,
        time::Duration,
    };

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
        gate: Arc<(Mutex<bool>, Condvar)>,
        threads: Vec<thread::JoinHandle<()>>,
    }

    impl Exhausted {
        fn new(limit: usize) -> Self {
            let mut owner = Self {
                gate: Arc::new((Mutex::new(false), Condvar::new())),
                threads: Vec::new(),
            };
            for _ in 0..=limit {
                let gate = owner.gate.clone();
                match thread::Builder::new()
                    .stack_size(128 * 1024)
                    .spawn(move || {
                        let (lock, changed) = &*gate;
                        let _open = changed
                            .wait_while(lock.lock().unwrap(), |open| !*open)
                            .unwrap();
                    }) {
                    Ok(thread) => owner.threads.push(thread),
                    Err(error) => {
                        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                        assert!(!owner.threads.is_empty());
                        return owner;
                    }
                }
            }
            panic!("isolated cgroup did not enforce the declared PID limit");
        }
    }

    impl Drop for Exhausted {
        fn drop(&mut self) {
            *self.gate.0.lock().unwrap() = true;
            self.gate.1.notify_all();
            for thread in self.threads.drain(..) {
                thread.join().unwrap();
            }
        }
    }

    assert_eq!(
        std::env::args().nth(1).as_deref(),
        Some("--isolated-pid-probe")
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
    println!("PID_EXHAUSTION_OK: cold startup rollback, warm startup rollback, actor/I/O OS join; one process join service retained");
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("requires an isolated Linux cgroup v2 with 32..=96 PIDs");
    std::process::exit(2);
}
