use crate::actix::lifecycle::{ActorControl, Completion, Outcome, await_completion};
use crate::actix::{actor::ActixActor, context::ActixContext, reference::ActixActorRef};
use actix::prelude::ArbiterHandle;
use futures::FutureExt;
use parrot_api::{
    actor::Actor as ParrotActor,
    address::ActorRef,
    errors::ActorError,
    message::Message,
    system::{SystemError, SystemResources, SystemState, SystemStatus},
    types::{ActorResult, BoxedActorRef},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

struct ArbiterOwners {
    workers: Vec<ArbiterHandle>,
    owners: Mutex<Option<Vec<actix::Arbiter>>>,
}
impl Drop for ArbiterOwners {
    fn drop(&mut self) {
        for worker in &self.workers {
            worker.stop();
        }
        // Dropping the last pool must not join on an arbiter or async worker.
        // Explicit shutdown joins these owners before reporting success.
        if let Some(owners) = self.owners.get_mut().unwrap().take() {
            let _ = std::thread::Builder::new()
                .name("parrot-actix-retire".into())
                .spawn(move || {
                    for owner in owners {
                        let _ = owner.join();
                    }
                });
        }
    }
}

/// An owned group of Actix arbiters, shared by clones of one Parrot system.
#[derive(Clone)]
pub struct ArbiterPool {
    inner: Arc<ArbiterOwners>,
    next: Arc<AtomicUsize>,
}
impl ArbiterPool {
    /// Build workers within the current Actix System. Zero selects one worker.
    pub fn new(size: usize) -> Self {
        let owners: Vec<_> = (0..size.max(1)).map(|_| actix::Arbiter::new()).collect();
        Self {
            inner: Arc::new(ArbiterOwners {
                workers: owners.iter().map(actix::Arbiter::handle).collect(),
                owners: Mutex::new(Some(owners)),
            }),
            next: Arc::new(AtomicUsize::new(0)),
        }
    }
    pub fn size(&self) -> usize {
        self.inner.workers.len()
    }
    pub(crate) fn next_arbiter(&self) -> ArbiterHandle {
        self.inner.workers[self.next.fetch_add(1, Ordering::Relaxed) % self.size()].clone()
    }
    async fn stop_and_join(&self) -> ActorResult<()> {
        for worker in &self.inner.workers {
            worker.stop();
        }
        let owners = self.inner.owners.lock().unwrap().take();
        if let Some(owners) = owners {
            let (send, receive) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name("parrot-actix-retire".into())
                .spawn(move || {
                    let mut failed = false;
                    for owner in owners {
                        failed |= owner.join().is_err();
                    }
                    let _ = send.send(failed);
                })
                .map_err(|e| {
                    ActorError::InternalError(format!("cannot join Actix workers: {e}"))
                })?;
            if receive.await.unwrap_or(true) {
                return Err(ActorError::InternalError(
                    "Actix worker failed during shutdown".into(),
                ));
            }
        }
        Ok(())
    }
}
impl std::fmt::Debug for ArbiterPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArbiterPool")
            .field("workers", &self.size())
            .finish()
    }
}

/// Resource and deadline settings for managed Actix actors.
#[derive(Debug, Clone)]
pub struct ActixSystemSettings {
    pub arbiter_count: usize,
    pub mailbox_capacity: usize,
    /// Maximum outstanding timer tasks per actor. Zero disables scheduling.
    pub max_context_tasks: usize,
    pub ask_timeout: Duration,
    pub shutdown_timeout: Duration,
}
impl Default for ActixSystemSettings {
    fn default() -> Self {
        Self {
            arbiter_count: num_cpus::get(),
            mailbox_capacity: 1024,
            max_context_tasks: crate::context_tasks::DEFAULT_MAX_CONTEXT_TASKS,
            ask_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(5),
        }
    }
}

pub(crate) struct Registration {
    pub actor_ref: Arc<dyn ActorRef>,
    pub control: Arc<ActorControl>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.control
            .request_stop(tokio::time::Instant::now() + self.control.timeout);
    }
}
#[derive(Default)]
pub(crate) struct Registry {
    pub actors: HashMap<String, Registration>,
    pub shutting_down: bool,
    stopped: bool,
    shutdown: Option<Completion>,
}

/// A Parrot system owns its registry and arbiter pool, never the host Actix System.
#[derive(Clone)]
pub struct ActixActorSystem {
    pub(crate) registry: Arc<RwLock<Registry>>,
    pub(crate) settings: ActixSystemSettings,
    arbiters: ArbiterPool,
    runtime: tokio::runtime::Handle,
    started: Instant,
}
impl ActixActorSystem {
    pub async fn new() -> Result<Self, SystemError> {
        Self::with_settings(Default::default()).await
    }
    pub async fn with_arbiter_count(arbiter_count: usize) -> Result<Self, SystemError> {
        Self::with_settings(ActixSystemSettings {
            arbiter_count,
            ..Default::default()
        })
        .await
    }
    pub async fn with_settings(mut settings: ActixSystemSettings) -> Result<Self, SystemError> {
        if settings.mailbox_capacity == 0 {
            return Err(SystemError::Other(anyhow::anyhow!(
                "Actix mailbox capacity must be positive"
            )));
        }
        if actix::System::try_current().is_none() {
            return Err(SystemError::Other(anyhow::anyhow!(
                "ActixActorSystem requires a running Actix System"
            )));
        }
        settings.arbiter_count = settings.arbiter_count.max(1);
        // Construct on the host thread; later spawns may originate on foreign
        // Tokio threads where Actix's thread-local System is absent.
        let arbiters = ArbiterPool::new(settings.arbiter_count);
        Ok(Self {
            registry: Arc::new(RwLock::new(Registry::default())),
            settings,
            arbiters,
            runtime: tokio::runtime::Handle::current(),
            started: Instant::now(),
        })
    }
    pub fn arbiter_size(&self) -> usize {
        self.settings.arbiter_count
    }
    pub(crate) fn arbiter_pool(&self) -> ArbiterPool {
        self.arbiters.clone()
    }

    pub async fn spawn_root_typed<A>(
        &self,
        actor: A,
        _config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError>
    where
        A: ParrotActor<Context = ActixContext<ActixActor<A>>>
            + parrot_api::actor::ActixEngineExt<Context = ActixContext<ActixActor<A>>>
            + Unpin
            + 'static,
    {
        let mut registry = self.registry.write().unwrap();
        if registry.shutting_down {
            return Err(SystemError::ShuttingDown);
        }
        let name = std::any::type_name::<A>()
            .rsplit("::")
            .next()
            .unwrap_or("unknown");
        let path = format!("actix://{name}/{}", Uuid::new_v4().simple());
        let (control, finished) = ActorControl::with_task_limit(
            self.settings.shutdown_timeout,
            self.settings.max_context_tasks,
        );
        let outcome: crate::actix::actor::ActorOutcome = Default::default();
        let actor_base = ActixActor::managed(
            actor,
            path.clone(),
            control.clone(),
            outcome.clone(),
            self.settings.mailbox_capacity,
            self.settings.ask_timeout,
        );
        let (sender, receiver) = actix::dev::channel::channel(self.settings.mailbox_capacity);
        let addr = actix::Addr::new(sender);
        let actor_ref = Box::new(
            ActixActorRef::new(addr, path.clone())
                .with_control(control.clone(), self.settings.ask_timeout),
        ) as BoxedActorRef;
        let (send, result) = tokio::sync::oneshot::channel();
        let tasks = control.tasks.clone();
        if !self.arbiter_pool().next_arbiter().spawn_fn(move || {
            actix::spawn(async move {
                // Construct the !Send Actix context only after arriving on its owner.
                // Await and dispose the complete context future, including its mailbox,
                // before acknowledging stop to the system monitor.
                let execution = std::panic::AssertUnwindSafe(async move {
                    actix::Context::with_receiver(receiver)
                        .into_future(actor_base)
                        .await;
                })
                .catch_unwind()
                .await;
                let result = match execution {
                    Ok(()) => outcome.lock().unwrap().take().unwrap_or_else(|| {
                        Err(ActorError::InternalError(
                            "Actix actor ended without lifecycle completion".into(),
                        ))
                    }),
                    Err(payload) => Err(ActorError::Panic(crate::actix::lifecycle::panic_message(
                        payload,
                    ))),
                };
                tasks.close();
                let tasks_result = tasks.join().await;
                let _ = send.send(result.and(tasks_result));
            });
        }) {
            return Err(SystemError::ActorError(ActorError::InternalError(
                "Actix arbiter rejected actor execution".into(),
            )));
        }
        registry.actors.insert(
            path.clone(),
            Registration {
                actor_ref: Arc::from(actor_ref.clone_boxed()),
                control: control.clone(),
            },
        );
        self.monitor_actor(path, control, finished, result);
        Ok(actor_ref)
    }

    pub(crate) fn monitor_actor(
        &self,
        path: String,
        control: Arc<ActorControl>,
        finished: tokio::sync::watch::Sender<Option<Outcome>>,
        result: tokio::sync::oneshot::Receiver<ActorResult<()>>,
    ) {
        let registry = Arc::downgrade(&self.registry);
        let pool = self.arbiters.clone();
        self.runtime.spawn(async move {
            let _pool_owner = pool;
            let outcome = tokio::select! {
                biased;
                _ = control.expired() => Err(ActorError::TimeoutDetail(format!("Actix actor {path} stop exceeded configured deadline"))),
                result = result => result.unwrap_or_else(|_| Err(ActorError::InternalError(format!("Actix actor {path} execution was cancelled")))),
            };
            control.request_stop(tokio::time::Instant::now() + control.timeout);
            if let Some(registry) = registry.upgrade() {
                let mut registry = registry.write().unwrap();
                if registry.actors.get(&path).is_some_and(|entry| Arc::ptr_eq(&entry.control, &control)) {
                    registry.actors.remove(&path);
                }
            }
            finished.send_replace(Some(Outcome::from_result(outcome)));
        });
    }
    pub async fn get_actor(&self, path: &String) -> Option<BoxedActorRef> {
        self.registry
            .read()
            .unwrap()
            .actors
            .get(path)
            .map(|entry| entry.actor_ref.clone_boxed())
    }
    pub async fn stop_actor(&self, path: &str) -> ActorResult<()> {
        let actor = self
            .registry
            .read()
            .unwrap()
            .actors
            .get(path)
            .map(|entry| entry.actor_ref.clone())
            .ok_or_else(|| ActorError::ActorNotFound(path.into()))?;
        actor.stop().await
    }
    /// Complete mailbox admission for each recipient and propagate any delivery failure.
    pub async fn broadcast<M: Message + Clone + 'static>(&self, msg: M) -> Result<(), SystemError> {
        let actors: Vec<_> = {
            let registry = self.registry.read().unwrap();
            if registry.shutting_down {
                return Err(SystemError::ShuttingDown);
            }
            registry
                .actors
                .values()
                .map(|entry| entry.actor_ref.clone())
                .collect()
        };
        let results = futures::future::join_all(actors.into_iter().map(|actor| {
            let msg = msg.clone();
            async move { actor.deliver(Box::new(msg)).await }
        }))
        .await;
        for result in results {
            result?;
        }
        Ok(())
    }
    /// Close admission, stop all actors, and join only this system's workers.
    /// Concurrent callers share one owned operation; cancelling a caller does not cancel cleanup.
    pub async fn shutdown(self) -> Result<(), SystemError> {
        let completion = {
            let mut registry = self.registry.write().unwrap();
            if let Some(completion) = &registry.shutdown {
                completion.clone()
            } else {
                registry.shutting_down = true;
                let deadline = tokio::time::Instant::now() + self.settings.shutdown_timeout;
                let (finished, completion) = tokio::sync::watch::channel(None);
                registry.shutdown = Some(completion.clone());
                let actors: Vec<_> = registry
                    .actors
                    .values()
                    .map(|entry| {
                        entry.control.request_stop(deadline);
                        entry.actor_ref.clone()
                    })
                    .collect();
                let state = self.registry.clone();
                let pool = self.arbiters.clone();
                let mut execution = self.runtime.spawn(async move {
                    let results =
                        futures::future::join_all(actors.iter().map(|actor| actor.stop())).await;
                    let joined = pool.stop_and_join().await;
                    {
                        let mut state = state.write().unwrap();
                        state.actors.clear();
                        state.stopped = true;
                    }
                    joined?;
                    for result in results {
                        result?;
                    }
                    Ok(())
                });
                self.runtime.spawn(async move {
                    let result = tokio::select! {
                        biased;
                        result = &mut execution => result.unwrap_or_else(|e| Err(ActorError::InternalError(format!("Actix shutdown task failed: {e}")))),
                        _ = tokio::time::sleep_until(deadline) => Err(ActorError::TimeoutDetail("Actix system shutdown exceeded configured deadline".into())),
                    };
                    // A timed-out join continues owning its workers and actor cleanup.
                    finished.send_replace(Some(Outcome::from_result(result)));
                });
                completion
            }
        };
        await_completion(completion)
            .await
            .map_err(SystemError::from)
    }
    pub fn status(&self) -> SystemStatus {
        let registry = self.registry.read().unwrap();
        SystemStatus {
            state: if registry.stopped {
                SystemState::Stopped
            } else if registry.shutting_down {
                SystemState::ShuttingDown
            } else {
                SystemState::Running
            },
            active_actors: registry.actors.len(),
            uptime: self.started.elapsed(),
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: if registry.stopped {
                    0
                } else {
                    self.arbiters.size()
                },
            },
        }
    }
}
