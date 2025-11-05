//! Admission for actual owned process groups, including startup and retirement.
use super::DeployError;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A host-owned budget shared by factory clones or explicitly across factories.
/// Counts guardian groups, not every descendant, CPU usage or process RSS.
/// A manifest cannot increase this host policy. Admission fails immediately;
/// cancelled startup and retirement continue consuming capacity until reaped.
pub struct GatewayProcessBudget {
    limit: usize,
    slots: Arc<Semaphore>,
    quarantined: AtomicUsize,
}

impl Default for GatewayProcessBudget {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_GROUPS).expect("valid default gateway process budget")
    }
}

impl GatewayProcessBudget {
    pub const DEFAULT_MAX_GROUPS: usize = 64;

    pub fn new(max_groups: usize) -> Result<Self, DeployError> {
        if max_groups == 0 || max_groups > Semaphore::MAX_PERMITS {
            return Err(DeployError::Config(format!(
                "gateway process group limit must be in 1..={}",
                Semaphore::MAX_PERMITS
            )));
        }
        Ok(Self {
            limit: max_groups,
            slots: Arc::new(Semaphore::new(max_groups)),
            quarantined: AtomicUsize::new(0),
        })
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
    /// Includes reserved startups, live groups, retirement and failed reaps.
    pub fn in_use(&self) -> usize {
        self.limit - self.slots.available_permits()
    }
    /// Capacity not reusable because retirement could not be proven. A failed
    /// reap must not make an unverified OS process disappear from accounting.
    pub fn quarantined(&self) -> usize {
        self.quarantined.load(Ordering::Acquire)
    }

    pub(super) fn reserve(self: &Arc<Self>) -> Result<Arc<Reservation>, DeployError> {
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            DeployError::Engine(format!(
                "gateway process group budget exhausted (in use {}, limit {})",
                self.in_use(),
                self.limit
            ))
        })?;
        Ok(Arc::new(Reservation {
            budget: self.clone(),
            permit: Some(permit),
            started: AtomicBool::new(false),
            retired: AtomicBool::new(false),
        }))
    }
}

pub(super) struct Reservation {
    budget: Arc<GatewayProcessBudget>,
    permit: Option<OwnedSemaphorePermit>,
    started: AtomicBool,
    retired: AtomicBool,
}
impl Reservation {
    pub(super) fn started(&self) {
        self.started.store(true, Ordering::Release);
    }
    pub(super) fn retired(&self) {
        self.retired.store(true, Ordering::Release);
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.started.load(Ordering::Acquire) && !self.retired.load(Ordering::Acquire) {
            self.budget.quarantined.fetch_add(1, Ordering::AcqRel);
            self.permit
                .take()
                .expect("reservation owns a permit")
                .forget();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_rejects_zero_and_unrepresentable_semaphore_capacity() {
        assert!(GatewayProcessBudget::new(0).is_err());
        assert!(GatewayProcessBudget::new(usize::MAX).is_err());
        assert_eq!(GatewayProcessBudget::default().limit(), 64);
    }

    #[test]
    fn pre_spawn_failure_releases_but_unproven_retirement_quarantines_capacity() {
        let budget = Arc::new(GatewayProcessBudget::new(1).unwrap());
        drop(budget.reserve().unwrap());
        assert_eq!(budget.in_use(), 0);
        let guard = budget.reserve().unwrap();
        guard.started();
        drop(guard);
        assert_eq!(budget.in_use(), 1);
        assert_eq!(budget.quarantined(), 1);
        assert!(budget.reserve().is_err());
    }

    #[test]
    fn concurrent_admission_never_overcommits_and_retirement_reuses_all_capacity() {
        let budget = Arc::new(GatewayProcessBudget::new(3).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(25));
        let mut workers = Vec::new();
        for _ in 0..24 {
            let budget = budget.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                let reservation = budget.reserve().ok();
                if let Some(guard) = &reservation {
                    guard.started();
                }
                barrier.wait();
                barrier.wait();
                let admitted = reservation.is_some();
                if let Some(guard) = reservation {
                    guard.retired();
                }
                admitted
            }));
        }
        barrier.wait();
        assert_eq!(budget.in_use(), 3);
        barrier.wait();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(|admitted| *admitted)
                .count(),
            3
        );
        assert_eq!(budget.in_use(), 0);
        assert_eq!(budget.quarantined(), 0);
        let reservations: Vec<_> = (0..3).map(|_| budget.reserve().unwrap()).collect();
        assert!(budget.reserve().is_err());
        drop(reservations);
        assert_eq!(budget.in_use(), 0);
    }
}
