//! Owned executors are built and retired outside every async executor.
use super::LoadBalancingStrategy;
use std::{
    future::Future,
    sync::{mpsc, Arc, Condvar, Mutex, OnceLock},
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::{runtime::Runtime, sync::Notify};

type Exit = Result<(), String>;
type OwnerJob = Box<dyn FnOnce() -> Exit + Send>;

#[cfg(test)]
#[path = "owned_runtime_contracts.rs"]
mod tests;

struct JoinJob {
    thread: JoinHandle<Exit>,
    receipt: Arc<Retirement>,
}

impl JoinJob {
    fn finish(self) {
        let result = self
            .thread
            .join()
            .map_err(|payload| crate::owned_input::panic_error(payload).to_string())
            .and_then(|result| result);
        *self.receipt.finished.lock().unwrap() = Some(result);
        // A custom waiter must not unwind the process-owned join service.
        let _ = crate::owned_input::call(|| self.receipt.changed.notify_waiters());
    }
}

struct JoinOwner {
    pending: Mutex<Vec<JoinJob>>,
    changed: Condvar,
}

struct SharedJoinOwner {
    owner: Arc<JoinOwner>,
    _thread: JoinHandle<()>,
}

impl JoinOwner {
    fn shared() -> Result<Arc<Self>, String> {
        // This process service outlives caller executors. Per-runtime owners,
        // Tokio workers and blocking helpers are joined before their receipt.
        static SHARED: OnceLock<Mutex<Option<SharedJoinOwner>>> = OnceLock::new();
        let mut shared = SHARED.get_or_init(|| Mutex::new(None)).lock().unwrap();
        if let Some(shared) = &*shared {
            return Ok(shared.owner.clone());
        }
        let owner = Arc::new(Self {
            pending: Mutex::new(Vec::new()),
            changed: Condvar::new(),
        });
        let worker = owner.clone();
        let thread = thread::Builder::new()
            .name("parrot-tokio-join-owner".into())
            .spawn(move || worker.run())
            .map_err(|error| format!("failed to start Tokio join owner: {error}"))?;
        *shared = Some(SharedJoinOwner {
            owner: owner.clone(),
            _thread: thread,
        });
        Ok(owner)
    }

    fn run(&self) {
        loop {
            let job = {
                let mut pending = self.pending.lock().unwrap();
                loop {
                    if let Some(index) = pending.iter().position(|job| job.thread.is_finished()) {
                        break pending.swap_remove(index);
                    }
                    pending = if pending.is_empty() {
                        self.changed.wait(pending).unwrap()
                    } else {
                        self.changed
                            .wait_timeout(pending, Duration::from_millis(1))
                            .unwrap()
                            .0
                    };
                }
            };
            // is_finished is only a scheduling hint: join, including TLS
            // destruction, must actually return before publishing completion.
            job.finish();
        }
    }

    fn enqueue(&self, job: JoinJob) {
        self.pending.lock().unwrap().push(job);
        self.changed.notify_one();
    }
}

struct Retirement {
    stop: Mutex<Option<mpsc::Sender<()>>>,
    thread: Mutex<Option<JoinHandle<Exit>>>,
    finished: Mutex<Option<Exit>>,
    changed: Notify,
    join_owner: Arc<JoinOwner>,
}

impl Retirement {
    fn request(self: &Arc<Self>) {
        // Disconnecting the channel is also the unpolled/cancelled startup
        // path. No new OS thread or caller-runtime task is needed at teardown.
        self.stop.lock().unwrap().take();
        let thread = self.thread.lock().unwrap().take();
        if let Some(thread) = thread {
            self.join_owner.enqueue(JoinJob {
                thread,
                receipt: self.clone(),
            });
        }
    }

    async fn wait(&self) -> Exit {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(result) = self.finished.lock().unwrap().clone() {
                return result;
            }
            changed.await;
        }
    }
}

pub(super) struct OwnedRuntime {
    handles: Vec<tokio::runtime::Handle>,
    retirements: Vec<Arc<Retirement>>,
}

impl OwnedRuntime {
    pub(super) async fn new(
        worker_threads: Option<usize>,
        io_threads: Option<usize>,
        strategy: LoadBalancingStrategy,
    ) -> Result<Self, String> {
        Self::new_with_builder(worker_threads, io_threads, strategy, |_, builder| {
            builder.build()
        })
        .await
    }

    async fn new_with_builder(
        worker_threads: Option<usize>,
        io_threads: Option<usize>,
        strategy: LoadBalancingStrategy,
        build: impl Fn(usize, &mut tokio::runtime::Builder) -> std::io::Result<Runtime>
            + Send
            + Sync
            + 'static,
    ) -> Result<Self, String> {
        let placement = !matches!(strategy, LoadBalancingStrategy::RoundRobin);
        let count = if placement {
            worker_threads.unwrap_or(2)
        } else {
            1
        };
        let build = Arc::new(build);
        let mut group = Self {
            handles: Vec::new(),
            retirements: Vec::new(),
        };
        for index in 0..count {
            let build = build.clone();
            let runtime = Self::start_with(move || {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.enable_all();
                if placement {
                    builder.worker_threads(1);
                } else if let Some(count) = worker_threads {
                    builder.worker_threads(count);
                }
                if let Some(count) = io_threads {
                    builder.max_blocking_threads(count);
                }
                build(index, &mut builder)
                    .map_err(|error| format!("failed to build owned Tokio runtime: {error}"))
            })
            .await;
            match runtime {
                Ok(mut runtime) => {
                    group.handles.append(&mut runtime.handles);
                    group.retirements.append(&mut runtime.retirements);
                }
                Err(error) => {
                    group.request_stop();
                    let cleanup = group.wait().await;
                    return Err(format!(
                        "{error}{}",
                        cleanup.err().map_or(String::new(), |e| format!("; {e}"))
                    ));
                }
            }
        }
        Ok(group)
    }

    pub(super) async fn new_io(io_threads: usize) -> Result<Self, String> {
        Self::start_with(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(io_threads)
                .max_blocking_threads(io_threads)
                .enable_all()
                .build()
                .map_err(|error| format!("failed to build owned Tokio I/O runtime: {error}"))
        })
        .await
    }

    async fn start_with(
        build: impl FnOnce() -> Result<Runtime, String> + Send + 'static,
    ) -> Result<Self, String> {
        Self::start_with_spawn(build, |job| {
            thread::Builder::new()
                .name("parrot-tokio-runtime-owner".into())
                .spawn(job)
        })
        .await
    }

    async fn start_with_spawn(
        build: impl FnOnce() -> Result<Runtime, String> + Send + 'static,
        spawn: impl FnOnce(OwnerJob) -> std::io::Result<JoinHandle<Exit>>,
    ) -> Result<Self, String> {
        let join_owner = JoinOwner::shared()?;
        let (stop, stopped) = mpsc::channel();
        let retirement = Arc::new(Retirement {
            stop: Mutex::new(Some(stop)),
            thread: Mutex::new(None),
            finished: Mutex::new(None),
            changed: Notify::new(),
            join_owner,
        });
        let mut owner = Self {
            handles: Vec::new(),
            retirements: vec![retirement.clone()],
        };
        let (started, startup) = tokio::sync::oneshot::channel();
        let thread = spawn(Box::new(move || {
            crate::owned_input::call(|| {
                let runtime = match build() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = started.send(Err(error));
                        return Ok(());
                    }
                };
                let handle = runtime.handle().clone();
                if started.send(Ok(handle)).is_ok() {
                    let _ = stopped.recv();
                }
                crate::owned_input::call(|| drop(runtime)).map_err(|error| error.to_string())
            })
            .map_err(|error| error.to_string())?
        }))
        .map_err(|error| format!("failed to start Tokio runtime owner: {error}"))?;
        *retirement.thread.lock().unwrap() = Some(thread);
        match startup.await {
            Ok(Ok(handle)) => {
                owner.handles.push(handle);
                Ok(owner)
            }
            result => {
                owner.request_stop();
                owner.wait().await?;
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => "Tokio runtime owner ended before startup completed".into(),
                })
            }
        }
    }

    pub(super) fn worker_count(&self) -> usize {
        self.handles.len()
    }

    pub(super) fn is_current(&self) -> bool {
        tokio::runtime::Handle::try_current().is_ok_and(|current| {
            self.handles
                .iter()
                .any(|handle| handle.id() == current.id())
        })
    }

    pub(super) fn spawn_on<F, T>(&self, worker: usize, future: F) -> tokio::task::JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.handles[worker % self.handles.len()].spawn(future)
    }

    pub(super) fn request_stop(&self) {
        // Every runtime starts teardown before any join is awaited. Worker
        // TLS can depend on another placement worker entering its own teardown.
        for retirement in &self.retirements {
            retirement.request();
        }
    }

    pub(super) async fn wait(&self) -> Exit {
        let mut result = Ok(());
        for retirement in &self.retirements {
            if let Err(error) = retirement.wait().await {
                if result.is_ok() {
                    result = Err(error);
                }
            }
        }
        result
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        self.request_stop();
    }
}
