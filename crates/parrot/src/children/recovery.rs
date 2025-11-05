//! Cohort barriers: all old generations retire, all outputs prepare, then all
//! replacements publish behind one activation gate. Abort never proves disposal.
use crate::constructor::ConstructionControl;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tokio::{sync::watch, time::Instant};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Retired = 0,
    Prepared = 1,
    Published = 2,
}
#[derive(Clone, Debug)]
enum Progress {
    Phase(u8),
    Active,
    Aborted(String),
}
struct State {
    next: HashMap<Uuid, u8>,
    finished: HashSet<Uuid>,
    phase: u8,
    committed: bool,
    error: Option<String>,
}
type Release = Box<dyn FnOnce(bool) + Send + 'static>;
pub(crate) struct Recovery {
    pub(crate) id: Uuid,
    pub(crate) deadline: Instant,
    pub(crate) control: Arc<ConstructionControl>,
    pub(crate) failure_context: Option<parrot_api::trace::TraceContext>,
    state: Mutex<State>,
    progress: watch::Sender<Progress>,
    release: Mutex<Option<Release>>,
    publications: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}
impl std::fmt::Debug for Recovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recovery")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
impl Recovery {
    pub(crate) fn new(
        id: Uuid,
        ids: impl IntoIterator<Item = Uuid>,
        deadline: Instant,
        release: Release,
    ) -> Arc<Self> {
        let next: HashMap<_, _> = ids.into_iter().map(|id| (id, 0)).collect();
        assert!(!next.is_empty(), "a recovery epoch requires members");
        Arc::new(Self {
            id,
            deadline,
            control: Arc::new(ConstructionControl::with_deadline(deadline)),
            failure_context: parrot_api::trace::TraceContext::current(),
            state: Mutex::new(State {
                next,
                finished: HashSet::new(),
                phase: 0,
                committed: false,
                error: None,
            }),
            progress: watch::channel(Progress::Phase(0)).0,
            release: Mutex::new(Some(release)),
            publications: Mutex::default(),
        })
    }
    pub(crate) fn on_activation(&self, publish: Box<dyn FnOnce() + Send>) {
        self.publications.lock().unwrap().push(publish);
    }
    fn release(&self, committed: bool) {
        let release = self.release.lock().unwrap().take();
        // Engine/topology callbacks run outside the epoch lock.
        if let Some(release) = release {
            release(committed);
        }
    }
    pub(crate) fn abort(&self, error: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        // The last valid publication acknowledgement commits the cohort. A
        // later explicit member stop belongs to the new live generation.
        if state.committed || state.error.is_some() {
            return;
        }
        let error = error.into();
        state.error = Some(error.clone());
        self.control.cancel();
        self.progress.send_replace(Progress::Aborted(error));
    }
    pub(crate) fn is_committed(&self) -> bool {
        self.state.lock().unwrap().committed
    }
    /// An arrival receipt is meaningful only after the engine has completed the
    /// corresponding actual disposal, preparation, or scheduler submission.
    pub(crate) async fn arrive(self: &Arc<Self>, member: Uuid, step: Step) -> Result<(), String> {
        let mut receipt = self.progress.subscribe();
        let mut commit = false;
        let invalid = {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = &state.error {
                return Err(error.clone());
            }
            if Instant::now() >= self.deadline {
                Some("cohort recovery deadline expired".to_owned())
            } else if state.phase != step as u8 || state.next.get(&member) != Some(&(step as u8)) {
                Some(format!(
                    "invalid or duplicate cohort arrival: member={member}, step={step:?}"
                ))
            } else {
                *state.next.get_mut(&member).unwrap() += 1;
                if state.next.values().all(|next| *next > state.phase) {
                    state.phase += 1;
                    if step == Step::Published {
                        state.committed = true;
                        commit = true;
                    } else {
                        self.progress.send_replace(Progress::Phase(state.phase));
                    }
                }
                None
            }
        };
        if let Some(error) = invalid {
            self.abort(error.clone());
            return Err(error);
        }
        if commit {
            // Clear old membership assignments and reopen child admission before
            // initialization can begin, so a new failure cannot join this epoch.
            for publish in std::mem::take(&mut *self.publications.lock().unwrap()) {
                publish();
            }
            self.release(true);
            self.progress.send_replace(Progress::Active);
        }
        let mut guard = ArrivalGuard {
            recovery: self.clone(),
            waiting: true,
        };
        loop {
            let progress = receipt.borrow_and_update().clone();
            match progress {
                Progress::Active => {
                    guard.waiting = false;
                    return Ok(());
                }
                Progress::Phase(phase) if phase > step as u8 => {
                    guard.waiting = false;
                    return Ok(());
                }
                Progress::Aborted(error) => {
                    guard.waiting = false;
                    return Err(error);
                }
                _ => {}
            }
            if self.is_committed() {
                receipt
                    .changed()
                    .await
                    .map_err(|_| "cohort activation owner disappeared".to_owned())?;
            } else {
                tokio::select! {
                    changed = receipt.changed() => { changed.map_err(|_| "cohort owner disappeared".to_owned())?; },
                    _ = tokio::time::sleep_until(self.deadline) => self.abort("cohort recovery deadline expired"),
                }
            }
        }
    }
    /// Called by the retained engine owner after an aborted member's real
    /// actor/output/capture/policy disposal. A public timeout is not this receipt.
    pub(crate) fn retired_after_abort(&self, member: Uuid) {
        let release = {
            let mut state = self.state.lock().unwrap();
            if state.committed {
                return;
            }
            assert!(state.error.is_some(), "abort must precede final retirement");
            assert!(
                state.next.contains_key(&member),
                "foreign retirement receipt"
            );
            state.finished.insert(member);
            state.finished.len() == state.next.len()
        };
        if release {
            self.release(false);
        }
    }
    /// Generation initialization/dispatch waits here. Scheduling alone never
    /// permits a member to execute before every sibling has published.
    pub(crate) async fn activation(&self) -> Result<(), String> {
        let mut receipt = self.progress.subscribe();
        loop {
            match receipt.borrow_and_update().clone() {
                Progress::Active => return Ok(()),
                Progress::Aborted(error) => return Err(error),
                Progress::Phase(_) => {}
            }
            receipt
                .changed()
                .await
                .map_err(|_| "cohort activation disappeared".to_owned())?;
        }
    }
}
struct ArrivalGuard {
    recovery: Arc<Recovery>,
    waiting: bool,
}
impl Drop for ArrivalGuard {
    fn drop(&mut self) {
        if self.waiting {
            self.recovery
                .abort("cohort participant cancelled its barrier wait");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    fn fixture() -> (Arc<Recovery>, [Uuid; 2], Arc<AtomicUsize>) {
        let ids = [Uuid::new_v4(), Uuid::new_v4()];
        let released = Arc::new(AtomicUsize::new(0));
        let count = released.clone();
        let recovery = Recovery::new(
            Uuid::new_v4(),
            ids,
            Instant::now() + Duration::from_secs(2),
            Box::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
            }),
        );
        (recovery, ids, released)
    }
    async fn barrier(recovery: &Arc<Recovery>, ids: [Uuid; 2], step: Step) {
        let (a, b) = tokio::join!(recovery.arrive(ids[0], step), recovery.arrive(ids[1], step));
        a.unwrap();
        b.unwrap();
    }
    #[tokio::test]
    async fn no_member_initializes_before_every_publication_and_admission_reopens_first() {
        let (recovery, ids, released) = fixture();
        barrier(&recovery, ids, Step::Retired).await;
        barrier(&recovery, ids, Step::Prepared).await;
        let first = recovery.arrive(ids[0], Step::Published);
        tokio::pin!(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut first)
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), recovery.activation())
                .await
                .is_err()
        );
        assert_eq!(released.load(Ordering::SeqCst), 0);
        let (a, b) = tokio::join!(&mut first, recovery.arrive(ids[1], Step::Published));
        a.unwrap();
        b.unwrap();
        recovery.activation().await.unwrap();
        assert_eq!(released.load(Ordering::SeqCst), 1);
        recovery.abort("late stop belongs to live generation");
        assert!(recovery.activation().await.is_ok());
    }
    #[tokio::test]
    async fn abort_retains_admission_until_every_real_disposal_receipt() {
        let (recovery, ids, released) = fixture();
        recovery.abort("constructor failed");
        assert!(recovery.activation().await.is_err());
        assert!(recovery.control.is_cancelled());
        recovery.retired_after_abort(ids[0]);
        recovery.retired_after_abort(ids[0]);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        recovery.retired_after_abort(ids[1]);
        recovery.retired_after_abort(ids[1]);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn duplicate_arrival_cannot_supply_a_missing_siblings_vote() {
        let (recovery, ids, released) = fixture();
        let first = recovery.arrive(ids[0], Step::Retired);
        tokio::pin!(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut first)
                .await
                .is_err()
        );
        assert!(recovery.arrive(ids[0], Step::Retired).await.is_err());
        assert!(first.await.is_err());
        assert!(!recovery.is_committed());
        assert_eq!(released.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn dropping_a_waiter_aborts_activation_without_faking_retirement() {
        let (recovery, ids, released) = fixture();
        let first = recovery.arrive(ids[0], Step::Retired);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), first)
                .await
                .is_err()
        );
        assert!(recovery.activation().await.is_err());
        assert_eq!(released.load(Ordering::SeqCst), 0);
    }
}
