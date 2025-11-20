//! Request tracing must finish independently of retained shard worker lifetimes.
use parrot_api::{
    actor::{Actor, ActorState},
    address::{ActorPath, ActorRef},
    context::ActorContext,
    errors::ActorError,
    system::ActorSystem,
    trace::{RecordingSnapshot, SpanKind, SpanRecord, SpanStatus, TraceContext, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_node::{
    configuration::NodeSettings,
    runtime::RunningNode,
    sharding::{ShardConfig, ShardFactory},
    NEcho, NEchoed, NGetTotal, NSlowEcho, NTotal,
};
use parrot_remote::{
    raft::Role,
    roles::directory::{
        network::{DirectoryTiming, NetworkDirectory},
        ownership::{OwnershipClusterClient, OwnershipOutcome, OwnershipStatus},
    },
    LocalLookup, NodeAddr, RemoteConfig,
};
use std::{
    any::Any,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

const WAIT: Duration = Duration::from_secs(20);

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}

#[derive(Clone)]
struct Observation {
    key: String,
    phase: &'static str,
    value: u64,
    path: String,
    context: Option<TraceContext>,
}
struct Events {
    observations: Mutex<Vec<Observation>>,
    created: AtomicUsize,
    dropped: AtomicUsize,
    entered: Notify,
    work: Semaphore,
}
impl Events {
    fn observe(&self, key: &str, phase: &'static str, value: u64, path: &str) {
        self.observations.lock().unwrap().push(Observation {
            key: key.into(),
            phase,
            value,
            path: path.into(),
            context: TraceContext::current(),
        });
    }
    fn find(&self, key: &str, phase: &'static str, value: u64) -> Observation {
        let observations = self.observations.lock().unwrap();
        let matches: Vec<_> = observations
            .iter()
            .filter(|item| item.key == key && item.phase == phase && item.value == value)
            .collect();
        assert_eq!(matches.len(), 1, "one actual {key} {phase} {value}");
        matches[0].clone()
    }
}
struct Release(Arc<Events>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.work.add_permits(16);
    }
}
struct Probe {
    key: String,
    generation: u64,
    events: Arc<Events>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.events.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Probe {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.events
                .observe(&self.key, "init", self.generation, ctx.path().path());
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = if let Some(NEcho(value)) = message.downcast_ref::<NEcho>() {
                *value
            } else if let Some(NSlowEcho(value)) = message.downcast_ref::<NSlowEcho>() {
                self.events
                    .observe(&self.key, "handle", *value, ctx.path().path());
                self.events.entered.notify_one();
                self.events.work.acquire().await.unwrap().forget();
                return Ok(Box::new(NEchoed(*value)) as BoxedMessage);
            } else if message.is::<NGetTotal>() {
                return Ok(Box::new(NTotal(self.generation)) as BoxedMessage);
            } else {
                return Err(ActorError::MessageHandlingError(
                    "unsupported trace probe message".into(),
                ));
            };
            self.events
                .observe(&self.key, "handle", value, ctx.path().path());
            Ok(Box::new(NEchoed(value)) as BoxedMessage)
        })
    }
    fn before_stop<'a>(
        &'a mut self,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.events
                .observe(&self.key, "stop", self.generation, ctx.path().path());
            Ok(())
        })
    }
}

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("trace contract deadline")
}
async fn echo(actor: &dyn ActorRef, value: u64) {
    let response = bounded(actor.send(Box::new(NEcho(value)))).await.unwrap();
    assert_eq!(*response.downcast::<NEchoed>().unwrap(), NEchoed(value));
}
async fn generation(actor: &dyn ActorRef) -> u64 {
    bounded(actor.send(Box::new(NGetTotal)))
        .await
        .unwrap()
        .downcast::<NTotal>()
        .unwrap()
        .0
}
fn completed(recorder: &TraceRecorder) -> RecordingSnapshot {
    recorder.close();
    let snapshot = recorder.snapshot();
    assert!(
        snapshot.terminal,
        "completed shard request kept a runtime span alive: {:?}",
        snapshot.counts
    );
    assert_eq!(snapshot.counts.started, snapshot.counts.completed);
    assert_eq!(snapshot.counts.overwritten, 0);
    assert_eq!(snapshot.counts.rejected_after_close, 0);
    assert!(!snapshot.counts.counters_overflowed);
    snapshot
}
fn recorded<'a>(snapshot: &'a RecordingSnapshot, observation: &Observation) -> &'a SpanRecord {
    let context = observation
        .context
        .as_ref()
        .expect("business trace retained");
    let records: Vec<_> = snapshot
        .records
        .iter()
        .filter(|record| {
            record.trace_id == context.trace_id() && record.span_id == context.span_id()
        })
        .collect();
    assert_eq!(records.len(), 1, "one actual business invocation");
    let record = records[0];
    assert_eq!(record.target, observation.path);
    assert_eq!(record.status, SpanStatus::Ok);
    record
}
fn parent<'a>(snapshot: &'a RecordingSnapshot, child: &SpanRecord) -> &'a SpanRecord {
    snapshot
        .records
        .iter()
        .find(|record| {
            record.trace_id == child.trace_id && Some(record.span_id) == child.parent_span_id
        })
        .expect("actual admitted dispatch parent must be recorded")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_recordings_finish_while_shard_entities_remain_owned() {
    let data = tempfile::tempdir().unwrap();
    let mut directory = bounded(NetworkDirectory::start_with_timing(
        data.path(),
        "shard-trace",
        RemoteConfig::mem("trace-authority"),
        vec![],
        Arc::new(Empty),
        DirectoryTiming {
            election_ms: 100,
            heartbeat_ms: 20,
            rpc_timeout_ms: 500,
        },
    ))
    .await
    .unwrap();
    bounded(async {
        while directory.handle().observation().role != Role::Leader {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let mut settings = NodeSettings::from_environment(|key| match key {
        "PARROT_NODE_ID" => Some("trace-host".into()),
        "PARROT_SCHEME" => Some("mem".into()),
        "PARROT_ACTORS" => Some("".into()),
        _ => None,
    })
    .unwrap();
    settings.resolved.thread.default_mailbox_capacity = 1;
    let node = bounded(RunningNode::start(&settings)).await.unwrap();
    let events = Arc::new(Events {
        observations: Mutex::new(Vec::new()),
        created: AtomicUsize::new(0),
        dropped: AtomicUsize::new(0),
        entered: Notify::new(),
        work: Semaphore::new(0),
    });
    let _release = Release(events.clone());
    let captured = events.clone();
    let factory: ShardFactory = Arc::new(move |entity| {
        captured.created.fetch_add(1, Ordering::SeqCst);
        captured.observe(&entity.key, "factory", entity.generation, &entity.resource);
        Ok(Box::new(Probe {
            key: entity.key,
            generation: entity.generation,
            events: captured.clone(),
        }))
    });
    let routes = vec![NodeAddr::mem("trace-authority")];
    let router = bounded(node.install_shards(
        ShardConfig::new(
            "requests",
            vec![NodeAddr::mem("trace-host")],
            "shard-trace",
            routes.clone(),
        ),
        factory,
    ))
    .await
    .unwrap();
    let lookup = |key: &str| ActorPath::placeholder(router.path(key).unwrap());
    let first = ActorSystem::get_actor(node.facade.as_ref(), &lookup("first"))
        .await
        .unwrap();
    let healthy = ActorSystem::get_actor(node.facade.as_ref(), &lookup("healthy"))
        .await
        .unwrap();

    let initial = TraceRecorder::new(128).unwrap();
    let initial_root = initial.root();
    let ambient = TraceRecorder::new(128).unwrap();
    let ambient_root = ambient.root();
    let mut request = initial_root.in_scope(|| first.send(Box::new(NSlowEcho(801))));
    bounded(ambient_root.scope(async {
        tokio::select! {
            _ = events.entered.notified() => {},
            result = &mut request => panic!("blocked business completed early: {result:?}"),
        }
        drop(request);
    }))
    .await;
    // Completing another entity also orders this observation after Host dispatch.
    echo(healthy.as_ref(), 802).await;
    initial.close();
    let blocked = initial.snapshot();
    assert!(
        !blocked.terminal,
        "closing a recorder must not cancel or hide accepted business work"
    );
    events.work.add_permits(1);
    // A real same-entity FIFO barrier completes after the cancelled ASK's work.
    echo(first.as_ref(), 803).await;
    let finished = completed(&initial);
    assert_eq!(
        blocked.counts.in_flight, 1,
        "only actual business is pending"
    );
    assert_eq!(events.created.load(Ordering::SeqCst), 2);
    assert_eq!(events.dropped.load(Ordering::SeqCst), 0);
    let original_generation = generation(first.as_ref()).await;
    let handled = events.find("first", "handle", 801);
    let business = recorded(&finished, &handled);
    assert_eq!(business.kind, SpanKind::DynamicHandler);
    assert_eq!(business.trace_id, initial_root.trace_id());
    let dispatch = parent(&finished, business);
    assert_eq!(dispatch.kind, SpanKind::DynamicHandler);
    assert_eq!(dispatch.parent_span_id, Some(initial_root.span_id()));
    let init = recorded(
        &finished,
        &events.find("first", "init", original_generation),
    );
    assert_eq!(init.kind, SpanKind::ActorInit);
    assert_eq!(init.parent_span_id, Some(dispatch.span_id));
    assert_eq!(
        events
            .find("first", "factory", original_generation)
            .context
            .unwrap()
            .span_id(),
        dispatch.span_id
    );
    let authority =
        OwnershipClusterClient::new(node.remote.clone(), "shard-trace", routes).unwrap();
    assert!(matches!(
        bounded(authority.observe(&router.path("first").unwrap(), WAIT)).await.unwrap(),
        OwnershipStatus::Applied { outcome: OwnershipOutcome::Observed(Some(record)), .. }
            if record.holder.as_deref() == Some("trace-host") && record.generation == original_generation
    ));

    let later = TraceRecorder::new(128).unwrap();
    let later_root = later.root();
    let captured = later_root.in_scope(|| first.send(Box::new(NEcho(804))));
    let reply = bounded(ambient_root.scope(captured)).await.unwrap();
    assert_eq!(*reply.downcast::<NEchoed>().unwrap(), NEchoed(804));
    echo(first.as_ref(), 805).await;
    let finished_later = completed(&later);
    let business = recorded(&finished_later, &events.find("first", "handle", 804));
    assert_eq!(business.trace_id, later_root.trace_id());
    assert_eq!(
        parent(&finished_later, business).parent_span_id,
        Some(later_root.span_id())
    );
    assert_eq!(generation(first.as_ref()).await, original_generation);

    let none = first.send(Box::new(NEcho(806)));
    bounded(ambient_root.scope(none)).await.unwrap();
    assert!(events.find("first", "handle", 806).context.is_none());
    let identity = TraceContext::root();
    let unrecorded = identity.in_scope(|| first.send(Box::new(NEcho(807))));
    bounded(ambient_root.scope(unrecorded)).await.unwrap();
    assert_eq!(
        events
            .find("first", "handle", 807)
            .context
            .unwrap()
            .trace_id(),
        identity.trace_id()
    );
    assert_eq!(completed(&ambient).counts.started, 0);

    let stop = TraceRecorder::new(128).unwrap();
    let stop_root = stop.root();
    bounded(stop_root.scope(async { router.passivate("first").await }))
        .await
        .unwrap();
    echo(healthy.as_ref(), 808).await;
    let stopped = completed(&stop);
    let hook = recorded(&stopped, &events.find("first", "stop", original_generation));
    assert_eq!(hook.kind, SpanKind::ActorBeforeStop);
    assert_eq!(hook.trace_id, stop_root.trace_id());
    assert_eq!(
        parent(&stopped, hook).parent_span_id,
        Some(stop_root.span_id())
    );
    assert_eq!(events.dropped.load(Ordering::SeqCst), 1);

    let fresh = TraceRecorder::new(128).unwrap();
    let fresh_root = fresh.root();
    bounded(fresh_root.scope(async { echo(first.as_ref(), 809).await })).await;
    echo(first.as_ref(), 810).await;
    let recreated = completed(&fresh);
    let next = generation(first.as_ref()).await;
    assert!(next > original_generation);
    let new_handler = recorded(&recreated, &events.find("first", "handle", 809));
    let new_init = recorded(&recreated, &events.find("first", "init", next));
    assert_eq!(new_handler.trace_id, fresh_root.trace_id());
    assert_eq!(new_init.parent_span_id, new_handler.parent_span_id);
    assert_ne!(new_handler.target, handled.path);
    assert_eq!(events.created.load(Ordering::SeqCst), 3);
    bounded(router.close()).await.unwrap();
    assert_eq!(events.dropped.load(Ordering::SeqCst), 3);
    for recorder in [&initial, &later, &ambient, &stop, &fresh] {
        completed(recorder);
    }
    bounded(node.shutdown()).await.unwrap();
    bounded(directory.close()).await.unwrap();
}
