//! Actual Wire transports and Thread actors: primary replies, shadow execution,
//! causal parent identity, and owned shutdown must agree end to end.
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorPath, ActorRef},
    system::ActorSystem,
    trace::{self, TraceContext, TraceRecorder},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
    mirror::{WireMirrorOptions, WireMirrorPolicy},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Work(u64);
impl parrot_api::Message for Work {
    type Result = ();
}
#[derive(Clone)]
struct Observation {
    name: &'static str,
    value: u64,
    context: Option<TraceContext>,
}
struct Observe {
    name: &'static str,
    seen: Arc<Mutex<Vec<Observation>>>,
}
impl Actor for Observe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = message.downcast::<Work>().unwrap().0;
            self.seen.lock().unwrap().push(Observation {
                name: self.name,
                value,
                context: TraceContext::current(),
            });
            Ok(Box::new(Work(value + 1)) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
struct RawLookup(Arc<ThreadActorSystem>);
#[async_trait::async_trait]
impl LocalLookup for RawLookup {
    async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
        self.0.get_actor(&ActorPath::placeholder(path)).await
    }
}
async fn exercise(tcp: bool, kind: u8, traced: bool) {
    let engine = ThreadActorSystem::shared(Default::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    for (name, path) in [("primary", "/app/source"), ("shadow", "/shadow")] {
        engine
            .spawn_at(
                Observe {
                    name,
                    seen: seen.clone(),
                },
                path,
                None,
                Default::default(),
            )
            .await
            .unwrap();
    }
    let config = |suffix| {
        let id = format!(
            "wire-mirror-{}-{tcp}-{kind}-{traced}-{suffix}",
            std::process::id()
        );
        if tcp {
            RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap()))
        } else {
            RemoteConfig::mem(id)
        }
    };
    let source =
        RemoteActorSystem::new(config("source"), Arc::new(RawLookup(engine.clone()))).unwrap();
    let peer = RemoteActorSystem::new(config("peer"), Arc::new(RawLookup(engine.clone()))).unwrap();
    let recorder = TraceRecorder::new(32).unwrap();
    let _attachment = peer.attach_trace_recorder(recorder.clone());
    let unrelated = TraceRecorder::new(32).unwrap();
    let ambient = unrelated.root();
    let handle = ambient
        .in_scope(|| {
            peer.register_wire_mirror(
                WireMirrorPolicy::new("/app", "/shadow"),
                WireMirrorOptions {
                    delivery_timeout: Duration::from_secs(3),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    source.start().await.unwrap();
    peer.start().await.unwrap();
    if tcp {
        source
            .connect(&NodeAddr::tcp(
                &peer.config.node_id,
                peer.local_addr().unwrap(),
            ))
            .await
            .unwrap();
    } else {
        source.connect_mem_pair(&peer).await.unwrap();
    }
    let reference = source
        .remote_ref(&format!("parrot://{}/app/source", peer.config.node_id))
        .unwrap();
    let root = traced.then(TraceContext::root);
    trace::scope(root.clone(), async {
        match kind {
            0 => {
                let reply = reference.send(Box::new(Work(41))).await.unwrap();
                assert_eq!(reply.downcast::<Work>().unwrap().0, 42);
            }
            1 => reference.deliver(Box::new(Work(41))).await.unwrap(),
            _ => reference
                .deliver_with_ack(Box::new(Work(41)), Duration::from_secs(3))
                .await
                .unwrap(),
        }
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if seen.lock().unwrap().len() == 2 && handle.stats().delivered == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    source.shutdown().await.unwrap();
    peer.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    assert!(handle.is_closed());
    assert_eq!(handle.stats().enqueued, 1);
    assert_eq!(handle.stats().delivered, 1);
    let observations = seen.lock().unwrap().clone();
    assert_eq!(observations.len(), 2);
    for name in ["primary", "shadow"] {
        let record = observations.iter().find(|r| r.name == name).unwrap();
        assert_eq!(record.value, 41);
        match (&root, &record.context) {
            (Some(parent), Some(child)) => {
                assert_eq!(child.trace_id(), parent.trace_id());
                assert_eq!(child.parent_span_id(), Some(parent.span_id()));
            }
            (None, None) => {}
            _ => panic!("trace presence changed across mirror wire"),
        }
    }
    if traced {
        assert_ne!(
            observations[0].context.as_ref().unwrap().span_id(),
            observations[1].context.as_ref().unwrap().span_id()
        );
    }
    recorder.close();
    let snapshot = recorder.drain();
    assert_eq!(snapshot.records.len(), if traced { 2 } else { 0 });
    assert!(snapshot.terminal);
    unrelated.close();
    assert!(unrelated.drain().records.is_empty());
}
macro_rules! case {
    ($name:ident,$tcp:expr,$kind:expr,$traced:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            exercise($tcp, $kind, $traced).await;
        }
    };
}
case!(mem_ask, false, 0, true);
case!(mem_tell, false, 1, true);
case!(mem_completion, false, 2, true);
case!(tcp_ask, true, 0, true);
case!(tcp_tell, true, 1, true);
case!(tcp_completion, true, 2, true);
case!(mem_ask_untraced, false, 0, false);
case!(mem_tell_untraced, false, 1, false);
case!(mem_completion_untraced, false, 2, false);
case!(tcp_ask_untraced, true, 0, false);
case!(tcp_tell_untraced, true, 1, false);
case!(tcp_completion_untraced, true, 2, false);
