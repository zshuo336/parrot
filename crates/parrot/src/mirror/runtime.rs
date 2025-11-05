use super::*;
use futures::FutureExt;
use std::{
    collections::HashMap,
    panic::AssertUnwindSafe,
    sync::{
        Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
type Copiers = Arc<RwLock<HashMap<TypeId, MirrorCopier>>>;
#[derive(Default)]
struct Counts {
    attempted: AtomicU64,
    queued: AtomicU64,
    admitted: AtomicU64,
    copy_failed: AtomicU64,
    full: AtomicU64,
    missing_target: AtomicU64,
    delivery_failed: AtomicU64,
    cancelled: AtomicU64,
    disposal_failed: AtomicU64,
    worker_failed: AtomicU64,
}
impl Counts {
    fn snapshot(&self) -> MirrorStats {
        MirrorStats {
            attempted: self.attempted.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            admitted: self.admitted.load(Ordering::Relaxed),
            copy_failed: self.copy_failed.load(Ordering::Relaxed),
            full: self.full.load(Ordering::Relaxed),
            missing_target: self.missing_target.load(Ordering::Relaxed),
            delivery_failed: self.delivery_failed.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
            disposal_failed: self.disposal_failed.load(Ordering::Relaxed),
            worker_failed: self.worker_failed.load(Ordering::Relaxed),
        }
    }
}
// This owner never defers destruction: the enclosing job keeps its count and
// byte permits until disposal returns. An unpublished shadow copy must not
// unwind into a primary call, queue cleanup, or another owned destructor.
struct ShadowInput<T> {
    context: Option<parrot_api::trace::TraceContext>,
    value: Option<T>,
    counts: Arc<Counts>,
}
impl<T> ShadowInput<T> {
    fn new(value: T, counts: &Arc<Counts>) -> Self {
        Self {
            context: parrot_api::trace::TraceContext::current(),
            value: Some(value),
            counts: counts.clone(),
        }
    }
    fn value(&self) -> &T {
        self.value.as_ref().expect("owned shadow input")
    }
    fn value_mut(&mut self) -> &mut T {
        self.value.as_mut().expect("owned shadow input")
    }
    fn take(&mut self) -> T {
        self.value.take().expect("owned shadow input")
    }
}
impl<T> Drop for ShadowInput<T> {
    fn drop(&mut self) {
        parrot_api::trace::in_scope(self.context.clone(), || {
            if let Err(payload) =
                std::panic::catch_unwind(AssertUnwindSafe(|| drop(self.value.take())))
            {
                let _ = crate::owned_payload::panic_message(payload);
                self.counts.disposal_failed.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
}

struct Job {
    message: ShadowInput<BoxedMessage>,
    lookup: ShadowInput<Arc<dyn MirrorLookup>>,
    source_path: String,
    _slot: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
    counts: Arc<Counts>,
    finished: bool,
}
impl Drop for Job {
    fn drop(&mut self) {
        if !self.finished {
            self.counts.cancelled.fetch_add(1, Ordering::Relaxed);
        }
    }
}
fn invoke<T>(callback: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(AssertUnwindSafe(callback))
        .map_err(crate::owned_payload::panic_message)
}

// Construction, polling and destruction are distinct user-code boundaries.
// Acquire ownership before returning, so even an unpolled cancellation retires
// the borrowed delegate before its target. No task or unbounded cleanup queue.
fn resolve<'a, T: Send + 'a>(
    future: BoxedFuture<'a, T>,
    counts: &Arc<Counts>,
) -> impl std::future::Future<Output = Result<T, String>> + Send + 'a {
    let mut future = ShadowInput::new(future, counts);
    let counts = counts.clone();
    async move {
        let result =
            std::future::poll_fn(|cx| match invoke(|| future.value_mut().as_mut().poll(cx)) {
                Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
                Ok(std::task::Poll::Ready(result)) => std::task::Poll::Ready(Ok(result)),
                Err(error) => std::task::Poll::Ready(Err(error)),
            })
            .await;
        let mut result = ShadowInput::new(result, &counts);
        drop(future);
        result.take()
    }
}

async fn admit(job: &mut Job, path: &str) -> Result<bool, String> {
    let lookup = invoke(|| job.lookup.value().lookup(path))?;
    let Some(target) = resolve(lookup, &job.counts).await? else {
        return Ok(false);
    };
    let target = ShadowInput::new(target, &job.counts);
    if invoke(|| target.value().path())? == job.source_path {
        return Ok(false);
    }
    let delivered = {
        let future = invoke(|| target.value().deliver(job.message.take()))?;
        resolve(future, &job.counts).await?
    };
    // ActorError::Other can own arbitrary errors. Its formatting and destructor
    // are not trusted. Preserve actual mailbox admission even if cleanup fails.
    let delivered = ShadowInput::new(delivered, &job.counts);
    if delivered.value().is_ok() {
        Ok(true)
    } else {
        Err("shadow admission rejected".into())
    }
}

struct Worker {
    rx: Option<mpsc::Receiver<Job>>,
    done: watch::Sender<bool>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        // This also runs if the worker task was never polled or was aborted.
        // All queued jobs (and their permits) retire before completion is visible.
        if let Some(mut rx) = self.rx.take() {
            rx.close();
            drop(rx);
        }
        self.done.send_replace(true);
    }
}
struct Session {
    policy: MirrorPolicy,
    options: MirrorOptions,
    copiers: Copiers,
    counts: Arc<Counts>,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    tx: mpsc::Sender<Job>,
    closed: AtomicBool,
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        if let Some(task) = self.task.get_mut().unwrap().take() {
            task.abort();
        }
    }
}
#[derive(Clone)]
pub struct MirrorHandle(Arc<Session>);
impl MirrorHandle {
    fn start(
        policy: MirrorPolicy,
        options: MirrorOptions,
        copiers: Copiers,
    ) -> Result<Self, MirrorError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| MirrorError::NoRuntime)?;
        let (tx, rx) = mpsc::channel::<Job>(options.capacity);
        let (cancel, mut cancelled) = watch::channel(false);
        let (done_tx, done) = watch::channel(false);
        let mut worker = Worker {
            rx: Some(rx),
            done: done_tx,
        };
        let target_path = policy.mirror_path.clone();
        let counts = Arc::new(Counts::default());
        let worker_counts = counts.clone();
        let task = runtime.spawn(async move {
            let result = AssertUnwindSafe(async {
                loop {
                    let mut job = tokio::select! { biased;
                        _ = cancelled.changed() => break,
                        job = worker.rx.as_mut().expect("live worker").recv() => match job { Some(job) => job, None => break }
                    };
                    let context = job.message.context.clone();
                    let outcome = tokio::select! { biased;
                        _ = cancelled.changed() => break,
                        result = tokio::time::timeout(options.delivery_timeout, parrot_api::trace::scope(context, admit(&mut job, &target_path))) => result
                    };
                    job.finished = true;
                    if matches!(outcome, Ok(Ok(true))) {
                        job.counts.admitted.fetch_add(1, Ordering::Relaxed);
                    } else if matches!(outcome, Ok(Ok(false))) {
                        job.counts.missing_target.fetch_add(1, Ordering::Relaxed);
                    } else if job.counts.delivery_failed.fetch_add(1, Ordering::Relaxed) == 0 {
                        tracing::warn!("mirror admission failed, panicked or timed out; inspect MirrorHandle::stats");
                    }
                }
            }).catch_unwind().await;
            if let Err(payload) = result {
                let _ = crate::owned_payload::panic_message(payload);
                worker_counts.worker_failed.fetch_add(1, Ordering::Relaxed);
            }
            drop(worker);
        });
        Ok(Self(Arc::new(Session {
            policy,
            options,
            copiers,
            counts,
            slots: Arc::new(Semaphore::new(options.capacity)),
            bytes: Arc::new(Semaphore::new(options.max_pending_bytes)),
            tx,
            closed: AtomicBool::new(false),
            cancel,
            done,
            task: Mutex::new(Some(task)),
        })))
    }
    pub fn policy(&self) -> &MirrorPolicy {
        &self.0.policy
    }
    pub fn stats(&self) -> MirrorStats {
        self.0.counts.snapshot()
    }
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire) || self.is_finished()
    }
    pub fn is_finished(&self) -> bool {
        *self.0.done.borrow()
    }
    /// Stop accepting copies and cancel queued/in-flight shadow admissions.
    /// Already admitted messages belong to the shadow actor and are not rolled back.
    pub fn close(&self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.slots.close();
        self.0.bytes.close();
        self.0.cancel.send_replace(true);
    }
    /// Cancel and wait for this worker; does not stop the source or shadow actor.
    pub async fn shutdown(&self) {
        self.close();
        let mut done = self.0.done.clone();
        let _ = done.wait_for(|finished| *finished).await;
        let task = self.0.task.lock().unwrap().take();
        if let Some(task) = task
            && let Err(error) = task.await
        {
            let _ = crate::owned_payload::join_error(error);
            self.0.counts.worker_failed.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(super) fn offer(
        &self,
        message: &BoxedMessage,
        lookup: Arc<dyn MirrorLookup>,
        source_path: String,
    ) {
        let s = &self.0;
        let lookup = ShadowInput::new(lookup, &s.counts);
        s.counts.attempted.fetch_add(1, Ordering::Relaxed);
        if self.is_closed() {
            s.counts.cancelled.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Ok(slot) = s.slots.clone().try_acquire_owned() else {
            s.counts.full.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let copier = s
            .copiers
            .read()
            .unwrap()
            .get(&(**message).type_id())
            .cloned();
        let copied = std::panic::catch_unwind(AssertUnwindSafe(|| {
            // Keep the two copier captures outside the callback unwind scope.
            // A callback may replace its own registration, leaving this clone
            // as their final owner. Retire the captures independently below.
            let copier = copier.as_ref().ok_or("message type has no mirror copier")?;
            let size = (copier.measure)(message)?.max(1);
            if size > s.options.max_message_bytes {
                return Err("mirror message byte budget exceeded".into());
            }
            let bytes = s
                .bytes
                .clone()
                .try_acquire_many_owned(size as u32)
                .map_err(|_| "mirror pending byte budget exceeded")?;
            let copy = (copier.copy)(message)?;
            Ok::<_, String>((ShadowInput::new(copy, &s.counts), bytes))
        }))
        .map_err(crate::owned_payload::panic_message);
        if let Some(copier) = copier
            && copier.retire().is_err()
        {
            s.counts.disposal_failed.fetch_add(1, Ordering::Relaxed);
        }
        let (copy, bytes) = match copied {
            Ok(Ok(copied)) => copied,
            _ => {
                if s.counts.copy_failed.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::warn!(
                        "mirror copying unsupported, over budget, failed or panicked; inspect MirrorHandle::stats"
                    );
                }
                return;
            }
        };
        if self.is_closed() {
            s.counts.cancelled.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut job = Job {
            message: copy,
            lookup,
            source_path,
            _slot: slot,
            _bytes: bytes,
            counts: s.counts.clone(),
            finished: false,
        };
        // Slots include copying and in-flight work, so a live receiver always
        // has capacity here. A concurrent close is accounted as cancellation.
        s.counts.queued.fetch_add(1, Ordering::Relaxed);
        if let Err(error) = s.tx.try_send(job) {
            job = error.into_inner();
            job.finished = false;
            drop(job);
        }
    }
}
#[derive(Default)]
struct RegistryState {
    sessions: Vec<MirrorHandle>,
    closed: bool,
}
#[derive(Default)]
pub(crate) struct MirrorRegistry {
    state: Mutex<RegistryState>,
    copiers: Copiers,
}
impl MirrorRegistry {
    pub fn register_copier(&self, copier: MirrorCopier) -> Result<(), MirrorError> {
        let mut incoming = Some(copier);
        let result = {
            let state = self.state.lock().unwrap();
            if state.closed {
                Err(MirrorError::Closed)
            } else {
                let mut copiers = self.copiers.write().unwrap();
                let key = incoming.as_ref().expect("unconsumed copier").type_id;
                if copiers.len() >= 1024 && !copiers.contains_key(&key) {
                    Err(MirrorError::RegistryFull)
                } else {
                    Ok(copiers.insert(key, incoming.take().expect("one registration")))
                }
            }
        };
        match result {
            Ok(old) => {
                if let Some(old) = old {
                    old.retire().map_err(|reason| MirrorError::CopierDisposal {
                        committed: true,
                        reason,
                    })?;
                }
                Ok(())
            }
            Err(error) => {
                incoming
                    .expect("rejected copier")
                    .retire()
                    .map_err(|reason| MirrorError::CopierDisposal {
                        committed: false,
                        reason: format!("{error}: {reason}"),
                    })?;
                Err(error)
            }
        }
    }

    pub fn register(
        &self,
        policy: MirrorPolicy,
        options: MirrorOptions,
    ) -> Result<MirrorHandle, MirrorError> {
        policy.validate()?;
        options.validate()?;
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(MirrorError::Closed);
        }
        if self.copiers.read().unwrap().is_empty() {
            return Err(MirrorError::NoCopier);
        }
        state.sessions.retain(|handle| !handle.is_finished());
        if state.sessions.len() >= 64 {
            return Err(MirrorError::RegistryFull);
        }
        let handle = MirrorHandle::start(policy, options, self.copiers.clone())?;
        for previous in &state.sessions {
            if previous.policy().src_prefix == handle.policy().src_prefix {
                previous.close();
            }
        }
        state.sessions.push(handle.clone());
        Ok(handle)
    }
    pub fn remove(&self, prefix: &str) -> Option<MirrorHandle> {
        let state = self.state.lock().unwrap();
        let handle = state
            .sessions
            .iter()
            .find(|h| !h.is_closed() && h.policy().src_prefix == prefix)?
            .clone();
        handle.close();
        Some(handle)
    }
    pub fn find(&self, path: &str) -> Option<MirrorHandle> {
        self.state
            .lock()
            .unwrap()
            .sessions
            .iter()
            .filter(|h| {
                !h.is_closed()
                    && h.policy().mirror_path != path
                    && path.starts_with(&h.policy().src_prefix)
            })
            .max_by_key(|h| h.policy().src_prefix.len())
            .cloned()
    }
    pub async fn shutdown(&self) -> Result<(), String> {
        let sessions = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            for handle in &state.sessions {
                handle.close();
            }
            std::mem::take(&mut state.sessions)
        };
        for handle in sessions {
            handle.shutdown().await;
        }
        let copiers = std::mem::take(&mut *self.copiers.write().unwrap());
        // One retirement job per facade. A blocked destructor does not block
        // the facade deadline or cancel the remaining captures' ownership.
        tokio::task::spawn_blocking(move || {
            let errors: Vec<_> = copiers
                .into_values()
                .filter_map(|copier| copier.retire().err())
                .collect();
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        })
        .await
        .map_err(crate::owned_payload::join_error)?
    }
}
impl Drop for MirrorRegistry {
    fn drop(&mut self) {
        for handle in &self.state.get_mut().unwrap().sessions {
            handle.close();
        }
    }
}
