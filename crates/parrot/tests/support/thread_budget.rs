use std::sync::{Arc, Condvar, Mutex};

pub(crate) struct ThreadBudget {
    released: Arc<(Mutex<bool>, Condvar)>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl ThreadBudget {
    pub(crate) fn exhaust() -> Self {
        let limit: usize = std::fs::read_to_string("/sys/fs/cgroup/pids.max")
            .expect("run in a disposable cgroup v2 container")
            .trim()
            .parse()
            .expect("container must have a finite pids.max");
        assert!(limit <= 128, "container pids.max must be <= 128");
        let mut budget = Self {
            released: Arc::new((Mutex::new(false), Condvar::new())),
            threads: Vec::new(),
        };
        for _ in 0..=limit {
            let released = budget.released.clone();
            match std::thread::Builder::new()
                .stack_size(128 * 1024)
                .spawn(move || {
                    let (lock, ready) = &*released;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                }) {
                Ok(thread) => budget.threads.push(thread),
                Err(error) => {
                    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                    return budget;
                }
            }
        }
        panic!("OS thread creation did not reach the cgroup limit");
    }
}

impl Drop for ThreadBudget {
    fn drop(&mut self) {
        let (lock, ready) = &*self.released;
        *lock.lock().unwrap() = true;
        ready.notify_all();
        for thread in self.threads.drain(..) {
            thread.join().unwrap();
        }
    }
}
