//! One generation-owned idle clock, reset at serial actor dispatch boundaries.
use crate::context_tasks::{ContextTasks, OwnedTask};
use parrot_api::types::{ActorResult, BoxedFuture};
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

/// Private mailbox token. Only a still-current idle interval becomes a public
/// LifecycleEvent::ReceiveTimeout at dispatch; stale queued tokens are ignored.
#[derive(Debug)]
pub(crate) struct IdleSignal(u64);

pub(crate) trait IdleRecipient: Send + Sync {
    /// Use a weak endpoint and bounded, cancellation-safe mailbox admission.
    fn deliver(&self, signal: IdleSignal) -> BoxedFuture<'_, ActorResult<()>>;
}

#[derive(Debug, Default)]
struct State {
    timeout: Option<Duration>,
    epoch: u64,
    active: usize,
    ready: bool,
    closed: bool,
    worker_started: bool,
    deadline: Option<Instant>,
    pending: Option<u64>,
}
impl State {
    fn reset(&mut self) {
        self.pending = None;
        self.deadline = None;
        if let Some(next) = self.epoch.checked_add(1) {
            self.epoch = next;
        } else {
            self.closed = true;
            self.timeout = None;
        }
        if !self.closed && self.ready && self.active == 0 {
            self.deadline = self
                .timeout
                .and_then(|timeout| Instant::now().checked_add(timeout));
        }
    }
}

pub(crate) struct ReceiveTimeout {
    state: Mutex<State>,
    changed: Notify,
    recipient: OnceLock<Arc<dyn IdleRecipient>>,
}
impl std::fmt::Debug for ReceiveTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiveTimeout")
            .field("state", &self.state.lock().unwrap())
            .finish_non_exhaustive()
    }
}
impl ReceiveTimeout {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            changed: Notify::new(),
            recipient: OnceLock::new(),
        })
    }
    pub(crate) fn bind(&self, recipient: Arc<dyn IdleRecipient>) {
        assert!(
            self.recipient.set(recipient).is_ok(),
            "idle recipient installed twice"
        );
    }
    pub(crate) fn get(&self) -> Option<Duration> {
        self.state.lock().unwrap().timeout
    }
    pub(crate) fn activate(&self) {
        let mut state = self.state.lock().unwrap();
        state.ready = true;
        state.reset();
        drop(state);
        self.changed.notify_waiters();
    }
    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.timeout = None;
        state.reset();
        drop(state);
        self.changed.notify_waiters();
    }

    /// The public setter cannot return an error. Zero disables the clock;
    /// unrepresentable deadlines are explicitly disabled and diagnosed.
    pub(crate) fn configure(
        self: &Arc<Self>,
        timeout: Option<Duration>,
        tasks: &ContextTasks,
    ) -> Option<OwnedTask> {
        let timeout = timeout.filter(|duration| {
            if duration.is_zero() {
                return false;
            }
            if Instant::now().checked_add(*duration).is_none() {
                tracing::warn!(?duration, "receive timeout exceeds clock range; disabled");
                return false;
            }
            true
        });
        let start = {
            let mut state = self.state.lock().unwrap();
            state.timeout = timeout.filter(|_| !state.closed);
            state.reset();
            let start =
                state.timeout.is_some() && !state.worker_started && self.recipient.get().is_some();
            state.worker_started |= start;
            start
        };
        self.changed.notify_waiters();
        if !start {
            return None;
        }
        match tasks.idle_worker(self.clone().run()) {
            Ok(task) => Some(task),
            Err(error) => {
                self.close();
                tracing::warn!(%error, "receive timeout could not start; disabled");
                None
            }
        }
    }

    /// An owned guard follows the entire handler future, including cancellation.
    pub(crate) fn enter(self: &Arc<Self>) -> Activity {
        let mut state = self.state.lock().unwrap();
        state.active += 1;
        state.reset();
        drop(state);
        self.changed.notify_waiters();
        Activity(self.clone())
    }
    pub(crate) fn claim(self: &Arc<Self>, signal: IdleSignal) -> Option<Activity> {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.active != 0 || state.pending != Some(signal.0) {
            return None;
        }
        state.active += 1;
        state.reset();
        drop(state);
        self.changed.notify_waiters();
        Some(Activity(self.clone()))
    }

    async fn run(self: Arc<Self>) -> ActorResult<()> {
        let recipient = self.recipient.get().expect("bound idle clock").clone();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (deadline, epoch, closed) = {
                let state = self.state.lock().unwrap();
                (state.deadline, state.epoch, state.closed)
            };
            if closed {
                return Ok(());
            }
            let Some(deadline) = deadline else {
                changed.await;
                continue;
            };
            tokio::select! {
                biased;
                _ = &mut changed => continue,
                _ = tokio::time::sleep_until(deadline) => {},
            }
            {
                let mut state = self.state.lock().unwrap();
                if state.closed || state.epoch != epoch || state.deadline != Some(deadline) {
                    continue;
                }
                state.deadline = None;
                state.pending = Some(epoch);
            }
            // New activity/configuration cancels capacity waits. Once admitted,
            // the private token is still validated before invoking user code.
            tokio::select! {
                biased;
                _ = &mut changed => {},
                result = recipient.deliver(IdleSignal(epoch)) => result?,
            }
        }
    }
}

pub(crate) struct Activity(Arc<ReceiveTimeout>);
impl Drop for Activity {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.active -= 1;
        state.reset();
        drop(state);
        self.0.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Inbox(tokio::sync::mpsc::Sender<IdleSignal>);
    impl IdleRecipient for Inbox {
        fn deliver(&self, signal: IdleSignal) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async move {
                self.0
                    .send(signal)
                    .await
                    .map_err(|_| parrot_api::errors::ActorError::Stopped)
            })
        }
    }
    async fn advance(duration: Duration) {
        tokio::time::advance(duration).await;
        tokio::task::yield_now().await;
    }
    #[tokio::test(start_paused = true)]
    async fn reconfiguration_cancels_pending_capacity_waits_and_reuses_worker() {
        let tasks = ContextTasks::new(0);
        let clock = ReceiveTimeout::new();
        let (send, mut events) = tokio::sync::mpsc::channel(1);
        send.try_send(IdleSignal(0)).unwrap();
        clock.bind(Arc::new(Inbox(send)));
        let worker = tokio::spawn(
            clock
                .configure(Some(Duration::from_secs(1)), &tasks)
                .unwrap(),
        );
        clock.activate();
        tokio::task::yield_now().await;
        advance(Duration::from_secs(1)).await;
        assert!(clock.state.lock().unwrap().pending.is_some());
        clock.configure(None, &tasks);
        assert_eq!(events.try_recv().unwrap().0, 0);
        tokio::task::yield_now().await;
        assert!(
            events.try_recv().is_err(),
            "disabled capacity wait must be cancelled"
        );
        assert!(
            clock
                .configure(Some(Duration::from_secs(2)), &tasks)
                .is_none()
        );
        advance(Duration::from_secs(2)).await;
        assert!(clock.claim(events.try_recv().unwrap()).is_some());
        clock.close();
        tasks.close();
        tasks.join().await.unwrap();
        worker.await.unwrap().unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn activity_pauses_idle_time_and_invalidates_queued_notifications() {
        let tasks = ContextTasks::new(0);
        let clock = ReceiveTimeout::new();
        let (send, mut events) = tokio::sync::mpsc::channel(2);
        clock.bind(Arc::new(Inbox(send)));
        let worker = tokio::spawn(
            clock
                .configure(Some(Duration::from_secs(10)), &tasks)
                .unwrap(),
        );
        clock.activate();
        tokio::task::yield_now().await;
        advance(Duration::from_secs(9)).await;
        assert!(events.try_recv().is_err());
        let work = clock.enter();
        assert!(
            clock
                .configure(Some(Duration::from_secs(20)), &tasks)
                .is_none()
        );
        advance(Duration::from_secs(100)).await;
        assert!(
            events.try_recv().is_err(),
            "time spent in a handler is not idle"
        );
        drop(work);
        advance(Duration::from_secs(19)).await;
        assert!(events.try_recv().is_err());
        advance(Duration::from_secs(1)).await;
        let obsolete = events.try_recv().unwrap();
        drop(clock.enter());
        assert!(
            clock.claim(obsolete).is_none(),
            "new activity invalidates an already queued tick"
        );
        advance(Duration::from_secs(20)).await;
        let handler = clock.claim(events.try_recv().unwrap()).unwrap();
        advance(Duration::from_secs(100)).await;
        assert!(events.try_recv().is_err());
        drop(handler);
        advance(Duration::from_secs(20)).await;
        assert!(
            clock.claim(events.try_recv().unwrap()).is_some(),
            "idle repeats after the timeout handler completes"
        );
        tasks.close();
        tasks.join().await.unwrap();
        worker.await.unwrap().unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn disabled_clocks_cancel_tokens_and_never_accumulate_catchup_ticks() {
        let tasks = ContextTasks::new(0);
        let clock = ReceiveTimeout::new();
        let (send, mut events) = tokio::sync::mpsc::channel(2);
        clock.bind(Arc::new(Inbox(send)));
        assert!(clock.configure(Some(Duration::ZERO), &tasks).is_none());
        assert_eq!(clock.get(), None);
        let worker = tokio::spawn(
            clock
                .configure(Some(Duration::from_secs(1)), &tasks)
                .unwrap(),
        );
        clock.activate();
        tokio::task::yield_now().await;
        advance(Duration::from_secs(3600)).await;
        let obsolete = events.try_recv().unwrap();
        advance(Duration::from_secs(3600)).await;
        assert!(
            events.try_recv().is_err(),
            "one pending tick cannot turn into a backlog"
        );
        assert!(clock.configure(None, &tasks).is_none());
        assert!(clock.claim(obsolete).is_none());
        advance(Duration::from_secs(3600)).await;
        assert!(events.try_recv().is_err());
        for _ in 0..1000 {
            assert!(
                clock
                    .configure(Some(Duration::from_secs(1)), &tasks)
                    .is_none(),
                "reconfiguration must reuse one worker"
            );
        }
        clock.configure(Some(Duration::MAX), &tasks);
        assert_eq!(clock.get(), None);
        clock.close();
        clock.configure(Some(Duration::from_secs(1)), &tasks);
        assert_eq!(clock.get(), None, "a closed generation cannot be rearmed");
        tasks.close();
        tasks.join().await.unwrap();
        worker.await.unwrap().unwrap();
    }
}
