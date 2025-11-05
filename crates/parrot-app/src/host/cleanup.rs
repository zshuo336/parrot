use super::references::{OwnedReference, OwnedReferences};
use crate::{cleanup::RetainedActor, DeployError};
use futures::FutureExt;
use parrot_api::{
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

pub(super) const MAX_CLEANUP_RESERVATIONS: usize = 4096;
type Outcome = Result<(), String>;
type Receipt = watch::Receiver<Option<Outcome>>;

pub(super) struct Registry {
    entries: Mutex<BTreeMap<u64, Arc<Entry>>>,
    next: AtomicU64,
    permits: Arc<Semaphore>,
    runtime: tokio::runtime::Handle,
}
impl Registry {
    pub(super) fn new(runtime: tokio::runtime::Handle) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::default(),
            next: AtomicU64::new(0),
            permits: Arc::new(Semaphore::new(MAX_CLEANUP_RESERVATIONS)),
            runtime,
        })
    }
    pub(super) fn reserve(self: &Arc<Self>, paths: &[String]) -> Result<Scope, DeployError> {
        if paths.len() > MAX_CLEANUP_RESERVATIONS {
            return Err(DeployError::Engine(
                "artifact cleanup ownership limit reached".into(),
            ));
        }
        let permit = self
            .permits
            .clone()
            .try_acquire_many_owned(paths.len() as u32)
            .map_err(|_| {
                DeployError::Engine(
                    "artifact cleanup ownership limit reached; retry pending cleanup".into(),
                )
            })?;
        // Identity order follows admission and declared member order, not the
        // order in which reverse rollback happens to observe failures.
        let first_id = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(paths.len() as u64)
            })
            .map_err(|_| DeployError::Engine("cleanup identity space exhausted".into()))?;
        Ok(Scope {
            registry: self.clone(),
            paths: paths.to_vec(),
            reservation: Arc::new(permit),
            first_id,
        })
    }
    pub(super) fn pending(&self) -> Vec<RetainedActor> {
        let entries: Vec<_> = self.entries.lock().unwrap().values().cloned().collect();
        entries.into_iter().map(|entry| entry.reference()).collect()
    }
    pub(super) async fn retry(&self) -> Result<(), DeployError> {
        let mut errors = Vec::new();
        for actor in self.pending().into_iter().rev() {
            if let Err(error) = actor.stop().await {
                errors.push(format!("{}: {error}", actor.path));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(DeployError::Cleanup {
                source: Box::new(DeployError::Engine("cleanup retry failed".into())),
                retained: self.pending(),
                detail: errors.join("; "),
            })
        }
    }
}
pub(super) struct Scope {
    registry: Arc<Registry>,
    pub(super) paths: Vec<String>,
    reservation: Arc<OwnedSemaphorePermit>,
    first_id: u64,
}
impl Scope {
    pub(super) async fn rollback(
        &self,
        mut owned: OwnedReferences,
        source: DeployError,
    ) -> DeployError {
        let mut retained = Vec::new();
        let mut disposal_errors = Vec::new();
        while let Some(actor) = owned.pop() {
            let index = owned.len();
            let path = &self.paths[index];
            if let Err(error) = stop(&*actor).await {
                let id = self.first_id + index as u64;
                let entry = Arc::new(Entry {
                    id,
                    path: path.clone(),
                    actor,
                    registry: Arc::downgrade(&self.registry),
                    runtime: self.registry.runtime.clone(),
                    state: Mutex::new(State {
                        done: false,
                        error,
                        in_flight: None,
                        reservation: Some(self.reservation.clone()),
                    }),
                });
                self.registry
                    .entries
                    .lock()
                    .unwrap()
                    .insert(id, entry.clone());
                retained.push(entry.reference());
            } else if !actor.release() {
                disposal_errors.push(format!("release {path} reference panicked"));
            }
        }
        retained.reverse();
        if retained.is_empty() {
            if disposal_errors.is_empty() {
                source
            } else {
                DeployError::Engine(format!(
                    "{source}; rollback reference disposal: {}",
                    disposal_errors.join("; ")
                ))
            }
        } else {
            let detail = retained
                .iter()
                .map(|actor| format!("{}: {}", actor.path, actor.error))
                .chain(disposal_errors)
                .collect::<Vec<_>>()
                .join("; ");
            DeployError::Cleanup {
                source: Box::new(source),
                retained,
                detail,
            }
        }
    }
}
async fn stop(actor: &dyn ActorRef) -> Outcome {
    match std::panic::AssertUnwindSafe(async { actor.stop().await })
        .catch_unwind()
        .await
    {
        Ok(Ok(())) | Ok(Err(ActorError::ActorNotFound(_))) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(payload) => {
            crate::disposal::drop_catching(payload);
            Err("actor stop panicked".into())
        }
    }
}
struct State {
    done: bool,
    error: String,
    in_flight: Option<Receipt>,
    reservation: Option<Arc<OwnedSemaphorePermit>>,
}
struct Entry {
    id: u64,
    path: String,
    actor: OwnedReference,
    registry: Weak<Registry>,
    runtime: tokio::runtime::Handle,
    state: Mutex<State>,
}
impl Entry {
    fn reference(self: Arc<Self>) -> RetainedActor {
        let error = self.state.lock().unwrap().error.clone();
        RetainedActor {
            path: self.path.clone(),
            error,
            actor: Arc::new(CleanupRef(self)),
        }
    }
    async fn retry(self: &Arc<Self>) -> Outcome {
        let mut receipt = {
            let mut state = self.state.lock().unwrap();
            if state.done {
                return Ok(());
            }
            if let Some(receipt) = &state.in_flight {
                receipt.clone()
            } else {
                let (sender, receipt) = watch::channel(None);
                state.in_flight = Some(receipt.clone());
                let entry = self.clone();
                self.runtime.spawn(async move {
                    let outcome = stop(&*entry.actor).await;
                    {
                        let mut state = entry.state.lock().unwrap();
                        state.in_flight = None;
                        match &outcome {
                            Ok(()) => {
                                state.done = true;
                                state.reservation.take();
                            }
                            Err(error) => state.error = error.clone(),
                        }
                    }
                    if outcome.is_ok() {
                        if let Some(registry) = entry.registry.upgrade() {
                            registry.entries.lock().unwrap().remove(&entry.id);
                        }
                    }
                    sender.send_replace(Some(outcome));
                });
                receipt
            }
        };
        loop {
            let outcome = receipt.borrow().clone();
            if let Some(outcome) = outcome {
                return outcome;
            }
            if receipt.changed().await.is_err() {
                return Err("cleanup owner runtime stopped".into());
            }
        }
    }
}
struct CleanupRef(Arc<Entry>);
impl std::fmt::Debug for CleanupRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CleanupRef").field(&self.0.path).finish()
    }
}
impl ActorRef for CleanupRef {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.actor.send(message)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.actor.send_with_timeout(message, timeout)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.actor.deliver(message)
    }
    fn send_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.actor.send_with_options(message, options)
    }
    fn deliver_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.actor.deliver_with_options(message, options)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { self.0.retry().await.map_err(ActorError::InternalError) })
    }
    fn path(&self) -> String {
        self.0.path.clone()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async {
            if self.0.state.lock().unwrap().done {
                false
            } else {
                self.0.actor.is_alive().await
            }
        })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self(self.0.clone()))
    }
    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        self.0.actor.death_watch()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
