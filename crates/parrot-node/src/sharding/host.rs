use super::*;
use futures::{stream::FuturesUnordered, StreamExt};
use std::sync::atomic::AtomicUsize;
use tokio::sync::mpsc;

pub(super) struct Activity(Arc<AtomicUsize>);
impl Activity {
    fn new(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::AcqRel);
        Self(active)
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Queued {
    context: Option<trace::TraceContext>,
    command: EntityCommand,
}
enum EntityCommand {
    Work(Work),
    Retire(Reply),
    Exit(Reply),
}
impl EntityCommand {
    fn reject(self, error: ActorError) {
        match self {
            Self::Work(work) => work.complete(Err(error)),
            Self::Retire(reply) | Self::Exit(reply) => reply.complete(Err(error)),
        }
    }
}
#[derive(Clone)]
struct Cell {
    actor: Arc<Owned<BoxedActorRef>>,
    sender: mpsc::Sender<Queued>,
    idle: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
}
impl Cell {
    fn enqueue(&self, command: EntityCommand) {
        let queued = Queued {
            context: trace::TraceContext::current(),
            command,
        };
        if let Err(failure) = self.sender.try_send(queued) {
            match failure {
                mpsc::error::TrySendError::Full(queued) => queued
                    .command
                    .reject(error("shard entity queue capacity exhausted")),
                mpsc::error::TrySendError::Closed(queued) => {
                    queued.command.reject(ActorError::Stopped)
                }
            }
        }
    }
    async fn control(&self, close: bool, exit: bool) -> ActorResult<()> {
        if !self.exited.load(Ordering::Acquire) {
            let (sender, receiver) = oneshot::channel();
            let reply = Reply {
                sender,
                _control: None,
            };
            self.enqueue(if exit {
                EntityCommand::Exit(reply)
            } else {
                EntityCommand::Retire(reply)
            });
            let mut response = receiver.await.map_err(|_| ActorError::Stopped)?;
            let response = Owned::new(response.take()?);
            if !response.get().is::<()>() {
                return response.finish(Err(error("invalid entity retirement reply")));
            }
            response.finish(Ok(()))?;
        }
        if close || exit {
            stop_for_owned_cleanup(self.actor.get().as_ref()).await?;
        }
        Ok(())
    }
}
pub(super) struct Entry {
    cell: Cell,
    retiring: bool,
    bucket: usize,
}
pub(super) struct Host {
    pub(super) node: String,
    pub(super) prefix: String,
    pub(super) authority: OwnershipClusterClient,
    pub(super) factory: Owned<ShardFactory>,
    pub(super) placement: Arc<RwLock<Placement>>,
    pub(super) entities: HashMap<String, Entry>,
    pub(super) limit: usize,
    pub(super) timeout: Duration,
    pub(super) drained: Arc<AtomicBool>,
    pub(super) queue_capacity: usize,
    pub(super) last_control: Option<Owned<BoxedActorRef>>,
    pub(super) buckets: Vec<Owned<BoxedActorRef>>,
    pub(super) bucket_loads: Vec<usize>,
    pub(super) authority_slots: Arc<Semaphore>,
}
impl Host {
    async fn cell(&mut self, key: &str, ctx: &dyn ActorContext) -> ActorResult<&mut Entry> {
        if self
            .entities
            .get(key)
            .is_some_and(|entry| entry.cell.exited.load(Ordering::Acquire))
        {
            self.entities[key].cell.control(false, true).await?;
            let removed = self.entities.remove(key).unwrap();
            self.bucket_loads[removed.bucket] -= 1;
        }
        if !self.entities.contains_key(key) {
            if self.entities.len() >= self.limit {
                let idle = self.entities.iter().find_map(|(key, entry)| {
                    (entry.cell.idle.load(Ordering::Acquire)
                        && entry.cell.active.load(Ordering::Acquire) == 0)
                        .then(|| key.clone())
                });
                let idle = idle.ok_or_else(|| error("shard entity capacity exhausted"))?;
                self.entities[&idle].cell.control(false, true).await?;
                let removed = self.entities.remove(&idle).unwrap();
                self.bucket_loads[removed.bucket] -= 1;
            }
            let (sender, receiver) = mpsc::channel(self.queue_capacity);
            let idle = Arc::new(AtomicBool::new(false));
            let exited = Arc::new(AtomicBool::new(false));
            let actor = EntityWorker {
                key: key.into(),
                core: EntityHost {
                    node: self.node.clone(),
                    prefix: self.prefix.clone(),
                    authority: self.authority.clone(),
                    factory: Owned::new(self.factory.get().clone()),
                    entities: HashMap::new(),
                    limit: 1,
                    timeout: self.timeout,
                    authority_slots: self.authority_slots.clone(),
                },
                receiver: Some(receiver),
                idle: idle.clone(),
                exited: exited.clone(),
            };
            // Leave room for termination notifications in each real parent.
            // At most eight buckets cover the public 4096-entity upper bound.
            let bucket = match self.bucket_loads.iter().position(|load| *load < 512) {
                Some(bucket) => bucket,
                None => {
                    let spawner = ctx
                        .async_spawner()
                        .ok_or_else(|| error("shard child capability unavailable"))?;
                    let bucket = resolve(call(|| {
                        spawner.spawn(Box::new(Bucket), Box::new(()), None)
                    })?)
                    .await?;
                    self.buckets.push(Owned::new(bucket));
                    self.bucket_loads.push(0);
                    self.buckets.len() - 1
                }
            };
            let (sender_reply, receiver_reply) = oneshot::channel();
            let create = CreateCell {
                actor: Owned::new(Box::new(actor)),
                reply: sender_reply,
            };
            resolve(call(|| {
                self.buckets[bucket].get().deliver(Box::new(create))
            })?)
            .await?;
            let mut created = receiver_reply.await.map_err(|_| ActorError::Stopped)?;
            let actor = Owned::new(created.take()?);
            let cell = Cell {
                actor: Arc::new(actor),
                sender,
                idle,
                exited,
                active: Arc::new(AtomicUsize::new(0)),
            };
            self.entities.insert(
                key.into(),
                Entry {
                    cell: cell.clone(),
                    retiring: false,
                    bucket,
                },
            );
            self.bucket_loads[bucket] += 1;
            // The queue loop outlives this request; each item restores its own context.
            trace::scope(None, async {
                resolve(call(|| cell.actor.get().deliver(Box::new(())))?).await
            })
            .await?;
        }
        Ok(self.entities.get_mut(key).expect("admitted shard cell"))
    }
    async fn dispatch(&mut self, mut work: Work, ctx: &dyn ActorContext) {
        let result = async {
            if self
                .placement
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .ring
                .node(&work.key)
                != Some(&self.node)
            {
                return Err(error("stale shard destination"));
            }
            let entry = self.cell(&work.key, ctx).await?;
            if entry.retiring {
                return Err(error("shard generation is retiring"));
            }
            if entry.cell.exited.load(Ordering::Acquire) {
                return Err(ActorError::Stopped);
            }
            entry.retiring = false;
            entry.cell.idle.store(false, Ordering::Release);
            Ok(entry.cell.clone())
        }
        .await;
        match result {
            Ok(cell) => {
                work._activity = Some(Activity::new(cell.active.clone()));
                cell.enqueue(EntityCommand::Work(work));
            }
            Err(error) => work.complete(Err(error)),
        }
    }
    async fn drain(&mut self, close: bool, reply: Reply, ctx: &dyn ActorContext) {
        let mut reply = Some(reply);
        let result = async {
            if let Some(previous) = &self.last_control {
                stop_for_owned_cleanup(previous.get().as_ref()).await?;
            }
            self.last_control = None;
            let mut cells = Vec::new();
            let mut keys: Vec<_> = self.entities.keys().cloned().collect();
            keys.sort();
            for key in keys {
                if close
                    || self
                        .placement
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .ring
                        .node(&key)
                        != Some(&self.node)
                {
                    let entry = self.entities.get_mut(&key).unwrap();
                    entry.retiring = true;
                    cells.push(entry.cell.clone());
                }
            }
            let spawner = ctx
                .async_spawner()
                .ok_or_else(|| error("shard child capability unavailable"))?;
            let actor = Drain {
                cells,
                close,
                drained: self.drained.clone(),
                reply: reply.take(),
            };
            let actor = Owned::new(
                resolve(call(|| spawner.spawn(Box::new(actor), Box::new(()), None))?).await?,
            );
            self.last_control = Some(actor);
            resolve(call(|| {
                self.last_control
                    .as_ref()
                    .unwrap()
                    .get()
                    .deliver(Box::new(()))
            })?)
            .await
        }
        .await;
        if let Some(reply) = reply.take() {
            reply.complete(result.map(|()| Box::new(()) as BoxedMessage));
        }
    }
}

struct CreateCell {
    actor: Owned<RuntimeActor>,
    reply: oneshot::Sender<Owned<ActorResult<BoxedActorRef>>>,
}
struct Bucket;
impl Actor for Bucket {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let mut message = Owned::new(message);
        Box::pin(async move {
            let mut create = match message.take().downcast::<CreateCell>() {
                Ok(create) => create,
                Err(input) => {
                    return Owned::new(input).finish(Err(error("invalid shard bucket command")))
                }
            };
            let result = async {
                let spawner = ctx
                    .async_spawner()
                    .ok_or_else(|| error("shard child capability unavailable"))?;
                resolve(call(|| {
                    spawner.spawn(create.actor.take(), Box::new(()), None)
                })?)
                .await
            }
            .await;
            let result = create.actor.finish(result);
            drop(Owned::new(create.reply.send(Owned::new(result))));
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
impl Actor for Host {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { self.factory.retire() })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let mut message = Owned::new(message);
        Box::pin(async move {
            let command = match message.take().downcast::<Command>() {
                Ok(command) => *command,
                Err(input) => {
                    return Owned::new(input).finish(Err(error("invalid shard host command")))
                }
            };
            match command {
                Command::Work(work) => self.dispatch(work, ctx).await,
                Command::Passivate(key, reply) => {
                    if let Some(entry) = self
                        .entities
                        .get_mut(&key)
                        .filter(|entry| !entry.cell.exited.load(Ordering::Acquire))
                    {
                        entry.retiring = true;
                        entry.cell.enqueue(EntityCommand::Retire(reply));
                    } else {
                        reply.complete(Ok(Box::new(())));
                    }
                }
                Command::Rebalance(placement, reply) => {
                    *self.placement.write().unwrap_or_else(|e| e.into_inner()) = placement;
                    self.drain(false, reply, ctx).await;
                }
                Command::Close(reply) => self.drain(true, reply, ctx).await,
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}

struct EntityWorker {
    key: String,
    core: EntityHost,
    receiver: Option<mpsc::Receiver<Queued>>,
    idle: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
}
impl Actor for EntityWorker {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { self.core.factory.retire() })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let message = Owned::new(message);
        Box::pin(async move {
            message.finish(Ok(()))?;
            let mut receiver = self.receiver.take().ok_or(ActorError::Stopped)?;
            while let Some(queued) = receiver.recv().await {
                let exit = trace::scope(queued.context, async {
                    match queued.command {
                        EntityCommand::Work(mut work) => {
                            let result = self.core.work(&self.key, &mut work.message, ctx).await;
                            self.idle
                                .store(self.core.entities.is_empty(), Ordering::Release);
                            work.complete(result);
                            false
                        }
                        EntityCommand::Retire(reply) => {
                            let result = self.core.retire(&self.key).await;
                            // A new generation gets a fresh parent scope. Keeping
                            // this handler open would strand child notifications.
                            let exit = result.is_ok();
                            if exit {
                                self.exited.store(true, Ordering::Release);
                                self.idle.store(true, Ordering::Release);
                            }
                            reply.complete(result.map(|()| Box::new(()) as BoxedMessage));
                            exit
                        }
                        EntityCommand::Exit(reply) => {
                            if self.core.entities.is_empty() {
                                self.exited.store(true, Ordering::Release);
                                reply.complete(Ok(Box::new(())));
                                true
                            } else {
                                reply.complete(Err(error("cannot evict an owned shard cell")));
                                false
                            }
                        }
                    }
                })
                .await;
                if exit {
                    break;
                }
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}

struct Drain {
    cells: Vec<Cell>,
    close: bool,
    drained: Arc<AtomicBool>,
    reply: Option<Reply>,
}
impl Actor for Drain {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let message = Owned::new(message);
        Box::pin(async move {
            message.finish(Ok(()))?;
            let reply = self.reply.take().ok_or(ActorError::Stopped)?;
            let mut pending = FuturesUnordered::new();
            for cell in self.cells.drain(..) {
                let close = self.close;
                pending.push(trace::capture(
                    async move { cell.control(close, false).await },
                ));
            }
            let mut first = None;
            while let Some(result) = pending.next().await {
                if let Err(error) = result {
                    first.get_or_insert(error);
                }
            }
            if self.close && first.is_none() {
                self.drained.store(true, Ordering::Release);
            }
            reply.complete(first.map_or_else(|| Ok(Box::new(()) as BoxedMessage), Err));
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
