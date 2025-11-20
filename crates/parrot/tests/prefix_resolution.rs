//! Path-aware fallback with real actors and transport effects. The table below
//! is a test resolver, not distributed sharding ownership or activation.
use parrot::{
    system::{ParrotActorSystem, PrefixResolver, RemoteGateway},
    thread::{context::ThreadContext, system::ThreadActorSystem},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorPath, ActorRef},
    message::MessageOptions,
    system::ActorSystem,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[path = "support/network.rs"]
mod network_support;

#[derive(Clone, serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Work(u64);
impl parrot_api::Message for Work {
    type Result = ();
}
#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Read;
impl parrot_api::Message for Read {
    type Result = ();
}
#[derive(
    Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage,
)]
struct State {
    name: String,
    ids: Vec<u64>,
}
impl parrot_api::Message for State {
    type Result = ();
}
struct Observation {
    name: &'static str,
    id: u64,
    context: Option<TraceContext>,
}
struct Entity {
    name: &'static str,
    ids: Vec<u64>,
    observations: Arc<Mutex<Vec<Observation>>>,
}
impl Actor for Entity {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(Work(id)) = message.downcast_ref::<Work>() {
                self.ids.push(*id);
                self.observations.lock().unwrap().push(Observation {
                    name: self.name,
                    id: *id,
                    context: TraceContext::current(),
                });
            } else {
                assert!(message.is::<Read>());
            }
            Ok(Box::new(State {
                name: self.name.to_owned(),
                ids: self.ids.clone(),
            }) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

struct Table {
    entries: BTreeMap<String, Arc<dyn ActorRef>>,
    looked_up: Arc<Mutex<Vec<String>>>,
}
impl PrefixResolver for Table {
    fn resolve(&self, path: &str) -> Option<BoxedActorRef> {
        self.looked_up.lock().unwrap().push(path.to_owned());
        self.entries.get(path).map(|actor| actor.clone_boxed())
    }
}
struct FacadeLookup(Arc<ParrotActorSystem>);
#[async_trait::async_trait]
impl LocalLookup for FacadeLookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.0.get_actor(&ActorPath::placeholder(path)).await
    }
}
struct Gateway(Arc<RemoteActorSystem>);
impl RemoteGateway for Gateway {
    fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.0
            .remote_ref(path)
            .ok()
            .map(|actor| Box::new(actor) as BoxedActorRef)
    }
}

struct Host {
    facade: Arc<ParrotActorSystem>,
    engine: Arc<ThreadActorSystem>,
    actors: Vec<Arc<dyn ActorRef>>,
    observations: Arc<Mutex<Vec<Observation>>>,
    looked_up: Arc<Mutex<Vec<String>>>,
}
impl Host {
    async fn new() -> Self {
        let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
        let engine = ThreadActorSystem::shared(Default::default());
        facade
            .register_thread_system("engine".into(), engine.clone(), true)
            .await
            .unwrap();
        let observations = Arc::new(Mutex::new(Vec::new()));
        let looked_up = Arc::new(Mutex::new(Vec::new()));
        let mut actors: Vec<Arc<dyn ActorRef>> = Vec::new();
        for name in ["a", "b"] {
            actors.push(Arc::new(
                engine
                    .spawn_at(
                        Entity {
                            name,
                            ids: Vec::new(),
                            observations: observations.clone(),
                        },
                        &format!("/instances/{name}"),
                        None,
                        Default::default(),
                    )
                    .await
                    .unwrap(),
            ));
        }
        facade
            .register_prefix_resolver(
                "/entity/",
                Arc::new(Table {
                    entries: [
                        ("/entity/a".to_owned(), actors[0].clone()),
                        ("/entity/b".to_owned(), actors[1].clone()),
                    ]
                    .into(),
                    looked_up: looked_up.clone(),
                }),
            )
            .unwrap();
        Self {
            facade,
            engine,
            actors,
            observations,
            looked_up,
        }
    }
}

async fn snapshot(reference: &dyn ActorRef) -> State {
    *reference
        .send_with_timeout(Box::new(Read), Some(Duration::from_secs(3)))
        .await
        .unwrap()
        .downcast::<State>()
        .unwrap()
}

async fn exercise_modes(references: &[BoxedActorRef], host: &Host) {
    let root = TraceContext::root();
    for (index, reference) in references.iter().enumerate() {
        for mode in 0..5 {
            let id = index as u64 * 100 + mode;
            let msg = Box::new(Work(id)) as BoxedMessage;
            trace::scope(Some(root.clone()), async {
                let response = match mode {
                    0 => Some(reference.send(msg).await.unwrap()),
                    1 => Some(
                        reference
                            .send_with_timeout(msg, Some(Duration::from_secs(3)))
                            .await
                            .unwrap(),
                    ),
                    2 => Some(
                        reference
                            .send_with_options(
                                msg,
                                MessageOptions {
                                    timeout: Some(Duration::from_secs(3)),
                                    ..Default::default()
                                },
                            )
                            .await
                            .unwrap(),
                    ),
                    3 => {
                        reference.deliver(msg).await.unwrap();
                        None
                    }
                    _ => {
                        reference
                            .deliver_with_options(msg, Default::default())
                            .await
                            .unwrap();
                        None
                    }
                };
                if let Some(response) = response {
                    let state = response.downcast::<State>().unwrap();
                    assert_eq!(state.name, ["a", "b"][index]);
                    assert_eq!(
                        state.ids,
                        (0..=mode)
                            .map(|n| index as u64 * 100 + n)
                            .collect::<Vec<_>>()
                    );
                }
            })
            .await;
            // A local state read is the completion barrier for remote TELL,
            // whose receipt does not imply execution or ordering with ASK.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let state = snapshot(host.actors[index].as_ref()).await;
                    if state.ids.len() == mode as usize + 1 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
    }
    for (index, actor) in host.actors.iter().enumerate() {
        assert_eq!(
            snapshot(actor.as_ref()).await,
            State {
                name: ["a", "b"][index].to_owned(),
                ids: (0..5).map(|n| index as u64 * 100 + n).collect(),
            }
        );
    }
    let seen = host.observations.lock().unwrap();
    assert_eq!(seen.len(), 10);
    for (i, observation) in seen.iter().enumerate() {
        assert_eq!(observation.name, ["a", "b"][i / 5]);
        assert_eq!(observation.id, (i / 5 * 100 + i % 5) as u64);
        let context = observation.context.as_ref().unwrap();
        assert_eq!(context.trace_id(), root.trace_id());
        assert_eq!(context.parent_span_id(), Some(root.span_id()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_keys_preserve_all_message_modes_and_trace() {
    let host = Host::new().await;
    let mut references = Vec::new();
    for name in ["a", "b"] {
        references.push(
            host.facade
                .get_actor(&ActorPath::placeholder(format!("/entity/{name}")))
                .await
                .unwrap(),
        );
    }
    exercise_modes(&references, &host).await;
    assert_eq!(*host.looked_up.lock().unwrap(), ["/entity/a", "/entity/b"]);
    assert!(
        host.facade
            .get_actor(&ActorPath::placeholder("/entity/missing"))
            .await
            .is_none()
    );
    host.facade.shutdown_runtime().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_local_actors_precede_resolvers() {
    let host = Host::new().await;
    // Match even the explicit instance paths; no resolver may run for them.
    host.facade
        .register_prefix_resolver(
            "/instances/",
            Arc::new(Table {
                entries: [("/instances/a".to_owned(), host.actors[1].clone())].into(),
                looked_up: host.looked_up.clone(),
            }),
        )
        .unwrap();
    let reference = host
        .facade
        .get_actor(&ActorPath::placeholder("/instances/a"))
        .await
        .unwrap();
    assert_eq!(snapshot(reference.as_ref()).await.name, "a");
    assert!(host.looked_up.lock().unwrap().is_empty());
    host.facade.shutdown_runtime().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mirror_resolves_shadow_path_into_actual_entity_effects() {
    use parrot::mirror::{MirrorCopier, MirrorPolicy};
    let host = Host::new().await;
    host.facade
        .register_mirror_copier(MirrorCopier::cloned::<Work>(|_| 8))
        .unwrap();
    let handle = host
        .facade
        .register_mirror_policy(MirrorPolicy::new("/instances/", "/entity/b"))
        .unwrap();
    let primary = host
        .facade
        .get_actor(&ActorPath::placeholder("/instances/a"))
        .await
        .unwrap();
    let reply = primary
        .send(Box::new(Work(71)))
        .await
        .unwrap()
        .downcast::<State>()
        .unwrap();
    assert_eq!(
        *reply,
        State {
            name: "a".into(),
            ids: vec![71]
        }
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let shadow = snapshot(host.actors[1].as_ref()).await;
            if shadow.ids == [71] && handle.stats().admitted == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    handle.shutdown().await;
    assert_eq!(*host.looked_up.lock().unwrap(), ["/entity/b"]);
    assert_eq!(snapshot(host.actors[0].as_ref()).await.ids, [71]);
    assert_eq!(host.observations.lock().unwrap().len(), 2);
    host.facade.shutdown_runtime().await.unwrap();
}

async fn network_keys(tcp: bool) {
    let host = Host::new().await;
    let caller = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let config = |name| {
        let id = format!("prefix-{}-{tcp}-{name}", std::process::id());
        if tcp {
            RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap()))
        } else {
            RemoteConfig::mem(id)
        }
    };
    let source =
        RemoteActorSystem::new(config("source"), Arc::new(FacadeLookup(caller.clone()))).unwrap();
    let target = RemoteActorSystem::new(
        config("target"),
        Arc::new(FacadeLookup(host.facade.clone())),
    )
    .unwrap();
    source.start().await.unwrap();
    target.start().await.unwrap();
    if tcp {
        source
            .connect(&NodeAddr::tcp(
                &target.config.node_id,
                target.local_addr().unwrap(),
            ))
            .await
            .unwrap();
    } else {
        source.connect_mem_pair(&target).await.unwrap();
    }
    let looked_up = Arc::new(Mutex::new(Vec::new()));
    caller
        .register_prefix_resolver(
            "/entity/",
            Arc::new(Table {
                entries: ["a", "b"]
                    .into_iter()
                    .map(|name| {
                        (
                            format!("/entity/{name}"),
                            Arc::new(
                                source
                                    .remote_ref(&format!(
                                        "parrot://{}/entity/{name}",
                                        target.config.node_id
                                    ))
                                    .unwrap(),
                            ) as Arc<dyn ActorRef>,
                        )
                    })
                    .collect(),
                looked_up: looked_up.clone(),
            }),
        )
        .unwrap();
    let mut references = Vec::new();
    for name in ["a", "b"] {
        references.push(
            caller
                .get_actor(&ActorPath::placeholder(format!("/entity/{name}")))
                .await
                .unwrap(),
        );
    }
    exercise_modes(&references, &host).await;
    assert_eq!(*looked_up.lock().unwrap(), ["/entity/a", "/entity/b"]);
    assert_eq!(
        *host.looked_up.lock().unwrap(),
        (0..2)
            .flat_map(|index| std::iter::repeat_n(format!("/entity/{}", ["a", "b"][index]), 5))
            .collect::<Vec<_>>()
    );

    // An explicit remote intent must bypass even a matching rejecting prefix.
    caller
        .register_remote_gateway(Arc::new(Gateway(source.clone())))
        .await
        .unwrap();
    caller
        .register_prefix_resolver(
            "parrot://",
            Arc::new(Table {
                entries: BTreeMap::new(),
                looked_up: looked_up.clone(),
            }),
        )
        .unwrap();
    let qualified = caller
        .get_actor(&ActorPath::placeholder(format!(
            "parrot://{}/entity/a",
            target.config.node_id
        )))
        .await
        .unwrap();
    assert_eq!(
        snapshot(qualified.as_ref()).await.ids,
        (0..5).collect::<Vec<_>>()
    );
    assert_eq!(looked_up.lock().unwrap().len(), 2);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
    caller.shutdown_runtime().await.unwrap();
    host.facade.shutdown_runtime().await.unwrap();
    assert!(
        host.engine
            .get_actor(&ActorPath::placeholder("/instances/a"))
            .await
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mem_keys_cross_both_facades_and_execute_separately() {
    network_keys(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_keys_cross_both_facades_and_execute_separately() {
    if network_support::skip_tcp("TCP prefix resolution") {
        return;
    }
    network_keys(true).await;
}
