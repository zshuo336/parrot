//! Negotiated control commands, actual transports and actual Thread actors.
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig, RemoteSecurity,
    mirror_control::{
        MirrorControl as C, MirrorControlReply as R, MirrorPolicySpec as P, MirrorSessionId,
        MirrorSessionPhase as Phase,
    },
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
type Seen = Arc<Mutex<Vec<(&'static str, u64, Option<TraceContext>)>>>;
struct Observe {
    name: &'static str,
    seen: Seen,
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
            let work = message.downcast::<Work>().unwrap();
            self.seen
                .lock()
                .unwrap()
                .push((self.name, work.0, TraceContext::current()));
            Ok(Box::new(Work(work.0 + 1)) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
struct Lookup(Arc<ThreadActorSystem>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
        self.0.get_actor_ref(path).map(|actor| actor.clone_boxed())
    }
}
struct Host {
    engine: Arc<ThreadActorSystem>,
    node: Arc<RemoteActorSystem>,
    client: Arc<RemoteActorSystem>,
    seen: Seen,
}
impl Host {
    async fn new(tcp: bool, authorized: bool) -> Self {
        let engine = ThreadActorSystem::shared(Default::default());
        let seen: Seen = Default::default();
        for (name, path) in [
            ("primary", "/source"),
            ("shadow", "/shadow"),
            ("other", "/other"),
        ] {
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
            let id = format!("mirror-control-{}-{suffix}", uuid::Uuid::new_v4());
            let mut c = if tcp {
                RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap()))
            } else {
                RemoteConfig::mem(id)
            };
            if authorized && tcp {
                c.security = RemoteSecurity::InsecureDevelopment;
            }
            c
        };
        let node =
            RemoteActorSystem::new(config("node"), Arc::new(Lookup(engine.clone()))).unwrap();
        let client =
            RemoteActorSystem::new(config("client"), Arc::new(Lookup(engine.clone()))).unwrap();
        node.start().await.unwrap();
        client.start().await.unwrap();
        let addr = if tcp {
            NodeAddr::tcp(&node.config.node_id, node.local_addr().unwrap())
        } else {
            NodeAddr::mem(&node.config.node_id)
        };
        client.connect(&addr).await.unwrap();
        Self {
            engine,
            node,
            client,
            seen,
        }
    }
    async fn command(&self, command: C) -> R {
        self.client
            .control_mirror(&self.node.config.node_id, command)
            .await
            .unwrap()
    }
    async fn reject(&self, command: C) {
        assert!(
            self.client
                .control_mirror(&self.node.config.node_id, command)
                .await
                .is_err()
        );
    }
    async fn open(&self, owner: &str) -> MirrorSessionId {
        let result = self
            .command(C::Open {
                owner: owner.into(),
            })
            .await;
        assert!(matches!(
            result,
            R::Session {
                revision: 0,
                phase: Phase::Empty,
                ..
            }
        ));
        result.session()
    }
    async fn apply(&self, session: MirrorSessionId, revision: u64, shadow: &str) -> R {
        let result = self
            .command(C::Apply {
                session,
                revision,
                policies: vec![P::new("/source", shadow)],
            })
            .await;
        assert!(matches!(
            result,
            R::Session {
                phase: Phase::Active,
                ..
            }
        ));
        result
    }
    async fn send(&self, value: u64) {
        let actor = self
            .client
            .remote_ref(&format!("parrot://{}/source", self.node.config.node_id))
            .unwrap();
        let reply = actor
            .send(Box::new(Work(value)))
            .await
            .unwrap()
            .downcast::<Work>()
            .unwrap();
        assert_eq!(reply.0, value + 1);
    }
    async fn wait(&self, name: &str, value: u64) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(n, v, _)| *n == name && *v == value)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("actual actor execution");
    }
    async fn close(self) {
        self.client.shutdown().await.unwrap();
        self.node.shutdown().await.unwrap();
        self.engine.shutdown_internal().await.unwrap();
    }
}
async fn business(tcp: bool) {
    let host = Host::new(tcp, true).await;
    let session = host.open("app").await;
    host.send(1).await;
    assert_eq!(
        host.seen.lock().unwrap().len(),
        1,
        "Open installs no policy"
    );
    host.apply(session, 1, "/shadow").await;
    let root = TraceContext::root();
    trace::scope(Some(root.clone()), host.send(41)).await;
    host.wait("shadow", 41).await;
    let result = host.command(C::Status { session }).await;
    assert!(matches!(
        result,
        R::Session {
            revision: 1,
            phase: Phase::Active,
            ..
        }
    ));
    assert_eq!(
        host.command(C::Close { session }).await,
        R::Retired { session }
    );
    host.send(50).await;
    assert_eq!(
        host.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _, _)| *n == "shadow")
            .count(),
        1
    );
    let rows = host.seen.lock().unwrap().clone();
    for (_, _, context) in rows.iter().filter(|(_, v, _)| *v == 41) {
        let context = context.as_ref().unwrap();
        assert_eq!(context.trace_id(), root.trace_id());
        assert_eq!(context.parent_span_id(), Some(root.span_id()));
    }
    assert!(
        host.engine
            .get_actor_ref("/source")
            .unwrap()
            .is_alive()
            .await
    );
    assert!(
        host.engine
            .get_actor_ref("/shadow")
            .unwrap()
            .is_alive()
            .await
    );
    host.close().await;
}
#[tokio::test]
async fn tcp_control_installs_real_mirroring_and_preserves_primary() {
    business(true).await;
}
#[tokio::test]
async fn memory_control_installs_real_mirroring_and_preserves_primary() {
    business(false).await;
}
#[tokio::test]
async fn revisions_retry_exactly_and_reject_conflicting_or_stale_updates() {
    let host = Host::new(false, true).await;
    let session = host.open("app").await;
    let first = host.apply(session, 1, "/shadow").await;
    assert_eq!(host.apply(session, 1, "/shadow").await, first);
    assert_eq!(
        host.command(C::Open {
            owner: "app".into()
        })
        .await,
        first
    );
    for (revision, shadow) in [(1, "/other"), (3, "/other")] {
        host.reject(C::Apply {
            session,
            revision,
            policies: vec![P::new("/source", shadow)],
        })
        .await;
    }
    host.send(10).await;
    host.wait("shadow", 10).await;
    host.apply(session, 2, "/other").await;
    host.reject(C::Apply {
        session,
        revision: 1,
        policies: vec![P::new("/source", "/shadow")],
    })
    .await;
    host.send(20).await;
    host.wait("other", 20).await;
    host.command(C::Close { session }).await;
    let rows = host.seen.lock().unwrap().clone();
    assert_eq!(rows.iter().filter(|(n, _, _)| *n == "shadow").count(), 1);
    assert_eq!(rows.iter().filter(|(n, _, _)| *n == "other").count(), 1);
    host.close().await;
}
#[tokio::test]
async fn retired_requests_never_resurrect_or_close_a_reused_owner() {
    let host = Host::new(false, true).await;
    let old = host.open("app").await;
    host.apply(old, 1, "/shadow").await;
    host.command(C::Close { session: old }).await;
    let next = host.open("app").await;
    assert_ne!(old, next);
    host.apply(next, 1, "/other").await;
    host.reject(C::Apply {
        session: old,
        revision: 2,
        policies: vec![P::new("/source", "/shadow")],
    })
    .await;
    assert_eq!(
        host.command(C::Close { session: old }).await,
        R::Retired { session: old }
    );
    assert_eq!(
        host.command(C::Status { session: old }).await,
        R::Retired { session: old }
    );
    host.send(10).await;
    host.wait("other", 10).await;
    assert!(
        !host
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(n, _, _)| *n == "shadow")
    );
    host.close().await;
}
#[tokio::test]
async fn conflicts_preserve_active_group_and_external_successor_ownership() {
    let host = Host::new(false, true).await;
    let session = host.open("app").await;
    host.apply(session, 1, "/shadow").await;
    let foreign = host
        .node
        .register_wire_mirror(
            parrot_remote::mirror::WireMirrorPolicy::new("/occupied", "/other"),
            Default::default(),
        )
        .unwrap();
    host.reject(C::Apply {
        session,
        revision: 2,
        policies: vec![P::new("/source", "/other"), P::new("/occupied", "/shadow")],
    })
    .await;
    host.send(10).await;
    host.wait("shadow", 10).await;
    assert!(!foreign.is_closed());
    let successor = host
        .node
        .register_wire_mirror(
            parrot_remote::mirror::WireMirrorPolicy::new("/source", "/other"),
            Default::default(),
        )
        .unwrap();
    assert!(matches!(
        host.command(C::Status { session }).await,
        R::Session {
            phase: Phase::Superseded,
            ..
        }
    ));
    host.reject(C::Apply {
        session,
        revision: 1,
        policies: vec![P::new("/source", "/shadow")],
    })
    .await;
    host.reject(C::Apply {
        session,
        revision: 2,
        policies: vec![P::new("/source", "/shadow")],
    })
    .await;
    host.command(C::Close { session }).await;
    assert!(!successor.is_closed());
    host.send(20).await;
    host.wait("other", 20).await;
    host.close().await;
}
#[tokio::test]
async fn sessions_are_bounded_reclaimable_and_unknown_tokens_are_not_receipts() {
    let host = Host::new(false, true).await;
    let mut ids = Vec::new();
    for index in 0..64 {
        ids.push(host.open(&format!("owner-{index}")).await);
    }
    host.reject(C::Open {
        owner: "overflow".into(),
    })
    .await;
    let mut invalid = ids[0];
    invalid.incarnation[0] ^= 1;
    host.reject(C::Status { session: invalid }).await;
    invalid = ids[0];
    invalid.sequence = 0;
    host.reject(C::Close { session: invalid }).await;
    invalid.sequence = 65;
    host.reject(C::Close { session: invalid }).await;
    for id in ids {
        host.command(C::Close { session: id }).await;
    }
    for _ in 0..70 {
        let id = host.open("reused").await;
        host.command(C::Close { session: id }).await;
    }
    host.close().await;
}
#[tokio::test]
async fn unauthorized_tcp_cannot_open_or_install_policies() {
    let host = Host::new(true, false).await;
    let error = host
        .client
        .control_mirror(
            &host.node.config.node_id,
            C::Open {
                owner: "forbidden".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("0x000d"), "{error}");
    host.close().await;
}
#[tokio::test]
async fn node_incarnations_fence_sessions_even_when_owner_and_sequence_match() {
    let first = Host::new(false, true).await;
    let second = Host::new(false, true).await;
    let old = first.open("app").await;
    let current = second.open("app").await;
    assert_eq!(old.sequence, current.sequence);
    assert_ne!(old.incarnation, current.incarnation);
    second.apply(current, 1, "/shadow").await;
    second.reject(C::Status { session: old }).await;
    second.reject(C::Close { session: old }).await;
    second
        .reject(C::Apply {
            session: old,
            revision: 1,
            policies: vec![P::new("/source", "/other")],
        })
        .await;
    second.send(41).await;
    second.wait("shadow", 41).await;
    first.close().await;
    second.close().await;
}
