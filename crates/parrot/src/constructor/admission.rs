//! Invocation ownership includes staged outputs, independently of worker use.
use super::{ConstructionControl, ConstructionError};
use std::sync::{Arc, Mutex};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};

#[cfg(test)]
mod contracts;

pub(crate) const SYSTEM_CAPACITY: usize = 1024;

pub(crate) struct Admission {
    slots: Arc<Semaphore>,
    capacity: usize,
}
impl Admission {
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(capacity)),
            capacity,
        })
    }

    #[cfg(test)]
    pub(crate) fn available_permits(&self) -> usize {
        self.slots.available_permits()
    }

    async fn acquire_amount<E>(
        &self,
        amount: usize,
        control: &ConstructionControl,
    ) -> Result<Arc<OwnedSemaphorePermit>, ConstructionError<E>> {
        if control.is_cancelled() {
            return Err(ConstructionError::Cancelled);
        }
        let amount = u32::try_from(amount)
            .ok()
            .filter(|_| amount <= self.capacity)
            .ok_or_else(|| {
                ConstructionError::WorkerFailed(
                    "recovery group exceeds shared factory invocation capacity".into(),
                )
            })?;
        if crate::owned_payload::DisposalScope::active() {
            return self
                .slots
                .clone()
                .try_acquire_many_owned(amount)
                .map(Arc::new)
                .map_err(|_| {
                    ConstructionError::WorkerFailed(
                        "cleanup capacity unavailable during reentrant construction".into(),
                    )
                });
        }
        tokio::select! {
            biased;
            _ = control.cancelled() => Err(ConstructionError::Cancelled),
            permit = self.slots.clone().acquire_many_owned(amount) => permit
                .map(Arc::new)
                .map_err(|error| ConstructionError::WorkerFailed(error.to_string())),
        }
    }

    pub(super) async fn acquire<E>(
        &self,
        control: &ConstructionControl,
    ) -> Result<Arc<OwnedSemaphorePermit>, ConstructionError<E>> {
        match &control.admission {
            Some(member) => member.acquire(self, control).await,
            None => self.acquire_amount(1, control).await,
        }
    }
}

type Reservation = Arc<OnceCell<Arc<OwnedSemaphorePermit>>>;

pub(crate) struct Batch {
    members: usize,
    reservations: Mutex<Vec<(Arc<Semaphore>, Reservation)>>,
}
impl Batch {
    pub(crate) fn new(members: usize) -> Arc<Self> {
        assert!(members > 0, "a construction batch requires members");
        Arc::new(Self {
            members,
            reservations: Mutex::default(),
        })
    }

    pub(crate) fn release(&self) {
        let reservations = std::mem::take(&mut *self.reservations.lock().unwrap());
        drop(reservations);
    }

    fn reservation(&self, admission: &Admission) -> Reservation {
        // Reserving the whole cohort avoids holding staged siblings while waiting
        // for another member's invocation slot at the preparation barrier.
        let mut reservations = self.reservations.lock().unwrap();
        if let Some((_, cell)) = reservations
            .iter()
            .find(|(slots, _)| Arc::ptr_eq(slots, &admission.slots))
        {
            return cell.clone();
        }
        let cell = Arc::new(OnceCell::new());
        reservations.push((admission.slots.clone(), cell.clone()));
        cell
    }
}

pub(super) struct Member {
    batch: Arc<Batch>,
    claimed: std::sync::atomic::AtomicBool,
}
impl Member {
    pub(super) fn new(batch: Arc<Batch>) -> Arc<Self> {
        Arc::new(Self {
            batch,
            claimed: false.into(),
        })
    }

    async fn acquire<E>(
        &self,
        admission: &Admission,
        control: &ConstructionControl,
    ) -> Result<Arc<OwnedSemaphorePermit>, ConstructionError<E>> {
        if self.claimed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Err(ConstructionError::WorkerFailed(
                "recovery member already claimed its factory invocation".into(),
            ));
        }
        let cell = self.batch.reservation(admission);
        if crate::owned_payload::DisposalScope::active() {
            if control.is_cancelled() {
                return Err(ConstructionError::Cancelled);
            }
            if let Some(permit) = cell.get() {
                return Ok(permit.clone());
            }
            let permit = admission
                .acquire_amount(self.batch.members, control)
                .await?;
            match cell.set(permit.clone()) {
                Ok(()) => return Ok(permit),
                Err(_) => {
                    return cell.get().cloned().ok_or_else(|| {
                        ConstructionError::WorkerFailed(
                            "recovery reservation unavailable during reentrant construction".into(),
                        )
                    });
                }
            }
        }
        tokio::select! {
            biased;
            _ = control.cancelled() => Err(ConstructionError::Cancelled),
            permit = cell.get_or_try_init(|| admission.acquire_amount(self.batch.members, control)) => permit.cloned(),
        }
    }
}
