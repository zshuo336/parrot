#![cfg(unix)]
//! Persistent ownership exercised over actual transports and Thread actors.
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    trace::TraceContext,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    mirror_control::{
        MirrorControl as C, MirrorControlReply as R, MirrorPolicySpec as P,
        MirrorSessionPhase as Phase,
    },
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig, RemoteSecurity,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(serde::Serialize, serde::Deserialize)]
struct Work(u64);
impl parrot_api::Message for Work {
    type Result = ();
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:orchestrated-mirror#v1", type_id: std::any::TypeId::of::<Work>(),
        encode: |message| parrot_api::message::serde_remote_serialize(message.downcast_ref::<Work>().ok_or("Work required")?),
        decode: |bytes| {
            if std::env::var_os("PARROT_MIRROR_SOURCE_NO_DECODER").is_some() { return Err("source process has no business decoder".into()); }
            Ok(Box::new(parrot_api::message::serde_remote_deserialize::<Work>(bytes)?) as BoxedMessage)
        },
    }
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
            ("primary", "/user/primary"),
            ("shadow", "/user/shadow"),
            ("other", "/user/other"),
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
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let id = format!(
                "parrot-mirror-{}-{suffix}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
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
        assert!(self
            .client
            .control_mirror(&self.node.config.node_id, command)
            .await
            .is_err());
    }
    async fn send(&self, value: u64) {
        let actor = self
            .client
            .remote_ref(&format!(
                "parrot://{}/user/primary",
                self.node.config.node_id
            ))
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

use parrot_app::{
    orchestrator::{
        mirror::MirrorIntentPhase, HealthWatch, MirrorSupervisor, ObservedState, ReconcileAction,
        SystemClock,
    },
    AppManifest, ApplicationLimits,
};
fn manifest() -> AppManifest {
    AppManifest::from_toml("name = 'mirror-app'\nversion = '1.0.0'\n[[components]]\nname = 'primary'\nengine = 'parrot'\nartifact = { Props = { factory = 'mirror-test' } }\n[[components]]\nname = 'shadow'\nengine = 'parrot'\nartifact = { Props = { factory = 'mirror-test' } }\n[[mirrors]]\nsource = 'primary'\nshadow = 'shadow'\ndelivery_timeout_ms = 3000\n").unwrap()
}
fn owner(directory: &tempfile::TempDir) -> MirrorSupervisor {
    MirrorSupervisor::open(
        directory.path(),
        manifest(),
        Arc::new(SystemClock),
        ApplicationLimits::default(),
    )
    .unwrap()
}
fn observed(host: &Host) -> ObservedState {
    let node = &host.node.config.node_id;
    let mut watch = HealthWatch::new(vec![node.clone()]).unwrap();
    watch
        .register_node_components(node, vec!["primary".into(), "shadow".into()])
        .unwrap();
    for name in ["primary", "shadow"] {
        let ticket = watch.begin_poll(node, name).unwrap();
        watch
            .apply_poll(
                ticket,
                vec![parrot_remote::admin_v2::ComponentStateReport {
                    path: format!("/user/{name}"),
                    state: "running".into(),
                    version: "1.0.0".into(),
                }],
            )
            .unwrap();
    }
    watch.observed().clone()
}
async fn converge(owner: &mut MirrorSupervisor, host: &Host, observed: &ObservedState) {
    for _ in 0..8 {
        if owner
            .reconcile_once(observed, &host.client)
            .await
            .unwrap()
            .converged
        {
            return;
        }
    }
    panic!("bounded reconciliation did not converge");
}
async fn durable_business(tcp: bool) {
    let host = Host::new(tcp, true).await;
    let dir = tempfile::tempdir().unwrap();
    let state = observed(&host);
    let mut controller = owner(&dir);
    assert!(
        !controller
            .reconcile_once(&state, &host.client)
            .await
            .unwrap()
            .converged
    );
    let session = controller.responsibilities()[&host.node.config.node_id].session;
    // Candidate reads cannot create a session or install policy.
    assert!(host
        .client
        .control_mirror(&host.node.config.node_id, C::Status { session })
        .await
        .is_err());
    // Restart at each durable boundary, including after the successful apply.
    for phase in [
        MirrorIntentPhase::Allocating,
        MirrorIntentPhase::Applying,
        MirrorIntentPhase::Applied,
    ] {
        assert_eq!(
            controller.responsibilities()[&host.node.config.node_id].phase,
            phase
        );
        drop(controller);
        controller = owner(&dir);
        let report = controller
            .reconcile_once(&state, &host.client)
            .await
            .unwrap();
        assert_eq!(report.converged, phase == MirrorIntentPhase::Applied);
    }
    host.send(41).await;
    host.wait("shadow", 41).await;
    let mut removed = controller.desired().clone();
    removed.mirrors.clear();
    controller.submit(removed).unwrap();
    assert!(!controller.responsibilities().is_empty());
    converge(&mut controller, &host, &state).await;
    assert!(controller.responsibilities().is_empty());
    assert_eq!(
        host.command(C::Status { session }).await,
        R::Retired { session }
    );
    host.send(90).await;
    assert!(!host
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|(name, value, _)| *name == "shadow" && *value == 90));
    host.close().await;
}
#[tokio::test]
async fn memory_transport_survives_all_durable_boundaries() {
    durable_business(false).await;
}
#[tokio::test]
async fn tcp_transport_survives_all_durable_boundaries() {
    durable_business(true).await;
}
#[tokio::test]
async fn retired_allocation_can_never_be_reopened_by_a_delayed_request() {
    let host = Host::new(true, true).await;
    let session = host.command(C::Candidate).await.session();
    let allocate = C::Allocate {
        owner: "persistent".into(),
        session,
    };
    let first = host.command(allocate.clone()).await;
    assert_eq!(host.command(allocate.clone()).await, first);
    host.command(C::Close { session }).await;
    for _ in 0..3 {
        host.reject(allocate.clone()).await;
    }
    let next = host.command(C::Candidate).await.session();
    assert_eq!(next.sequence, session.sequence + 1);
    host.reject(C::Apply {
        session,
        revision: 1,
        policies: vec![P::new("/user/primary", "/user/shadow")],
    })
    .await;
    host.close().await;
}
#[tokio::test]
async fn competing_allocator_cannot_be_closed_or_reused_by_our_journal() {
    let host = Host::new(false, true).await;
    let dir = tempfile::tempdir().unwrap();
    let state = observed(&host);
    let mut c = owner(&dir);
    c.reconcile_once(&state, &host.client).await.unwrap();
    let session = c.responsibilities()[&host.node.config.node_id].session;
    host.command(C::Allocate {
        owner: "foreign".into(),
        session,
    })
    .await;
    assert!(
        !c.reconcile_once(&state, &host.client)
            .await
            .unwrap()
            .converged
    );
    assert!(
        matches!(host.command(C::Status{session}).await,R::Session{owner,phase:Phase::Empty,..} if owner=="foreign")
    );
    converge(&mut c, &host, &state).await;
    host.send(42).await;
    host.wait("shadow", 42).await;
    host.command(C::Close { session }).await;
    host.close().await;
}
#[tokio::test]
async fn changing_desired_before_allocate_retires_the_recorded_slot_first() {
    let host = Host::new(false, true).await;
    let dir = tempfile::tempdir().unwrap();
    let state = observed(&host);
    let mut c = owner(&dir);
    c.reconcile_once(&state, &host.client).await.unwrap();
    let old = c.responsibilities()[&host.node.config.node_id].clone();
    let mut next = c.desired().clone();
    next.mirrors[0].capacity = 3;
    c.submit(next).unwrap();
    assert!(
        !c.reconcile_once(&state, &host.client)
            .await
            .unwrap()
            .converged
    );
    assert_eq!(
        c.responsibilities()[&host.node.config.node_id].phase,
        MirrorIntentPhase::Closing
    );
    converge(&mut c, &host, &state).await;
    assert_eq!(
        host.command(C::Status {
            session: old.session
        })
        .await,
        R::Retired {
            session: old.session
        }
    );
    let new = &c.responsibilities()[&host.node.config.node_id];
    assert_ne!(new.session, old.session);
    assert_eq!(new.policies[0].capacity, 3);
    host.reject(C::Allocate {
        owner: old.owner,
        session: old.session,
    })
    .await;
    host.close().await;
}
#[tokio::test]
async fn stale_active_metadata_does_not_prove_current_policy_convergence() {
    let host = Host::new(false, true).await;
    let dir = tempfile::tempdir().unwrap();
    let state = observed(&host);
    let mut c = owner(&dir);
    converge(&mut c, &host, &state).await;
    let old = c.responsibilities()[&host.node.config.node_id].session;
    host.command(C::Close { session: old }).await;
    assert!(
        !c.reconcile_once(&state, &host.client)
            .await
            .unwrap()
            .converged
    );
    converge(&mut c, &host, &state).await;
    assert_ne!(c.responsibilities()[&host.node.config.node_id].session, old);
    host.send(45).await;
    host.wait("shadow", 45).await;
    host.close().await;
}
#[tokio::test]
async fn components_cannot_be_stopped_before_policy_retirement() {
    let host = Host::new(false, true).await;
    let dir = tempfile::tempdir().unwrap();
    let state = observed(&host);
    let mut c = owner(&dir);
    converge(&mut c, &host, &state).await;
    let session = c.responsibilities()[&host.node.config.node_id].session;
    let mut next = c.desired().clone();
    next.mirrors.clear();
    next.components
        .retain(|component| component.name == "primary");
    c.submit(next).unwrap();
    let report = c.reconcile_once(&state, &host.client).await.unwrap();
    assert!(report
        .actions
        .iter()
        .any(|a| matches!(a, ReconcileAction::Stop { .. })));
    assert_eq!(
        host.command(C::Status { session }).await,
        R::Retired { session }
    );
    host.close().await;
}
#[tokio::test]
async fn unknown_or_ineligible_provenance_never_installs_a_policy() {
    let host = Host::new(false, true).await;
    let dir = tempfile::tempdir().unwrap();
    let mut state = observed(&host);
    let mut c = owner(&dir);
    let before = host.command(C::Candidate).await;
    state
        .component_nodes
        .insert("shadow".into(), "parrot-other".into());
    // The reported source is not in the authenticated live-node set.
    let report = c.reconcile_once(&state, &host.client).await.unwrap();
    assert!(!report.converged);
    assert!(c.responsibilities().is_empty());
    state = observed(&host);
    state.links_known = false;
    assert!(
        !c.reconcile_once(&state, &host.client)
            .await
            .unwrap()
            .converged
    );
    assert!(c.responsibilities().is_empty());
    assert_eq!(host.command(C::Candidate).await, before);
    host.close().await;
}
#[tokio::test]
async fn failed_intent_persistence_cannot_allocate_remote_resources() {
    let host = Host::new(false, true).await;
    let dir = tempfile::tempdir().unwrap();
    let state = observed(&host);
    let mut c = owner(&dir);
    let before = host.command(C::Candidate).await;
    std::fs::write(dir.path().join("desired.next"), b"owned failure injection").unwrap();
    assert!(c.reconcile_once(&state, &host.client).await.is_err());
    assert_eq!(host.command(C::Candidate).await, before);
    assert!(c.responsibilities().is_empty());
    drop(c);
    let mut recovered = owner(&dir);
    converge(&mut recovered, &host, &state).await;
    host.close().await;
}
#[test]
fn two_local_owners_cannot_drive_the_same_durable_journal() {
    let dir = tempfile::tempdir().unwrap();
    let c = owner(&dir);
    assert!(MirrorSupervisor::open(
        dir.path(),
        manifest(),
        Arc::new(SystemClock),
        ApplicationLimits::default()
    )
    .is_err());
    drop(c);
    drop(owner(&dir));
}

#[tokio::test]
async fn cancelling_each_network_wait_keeps_a_recoverable_responsibility() {
    let host = Host::new(true, true).await;
    let dir = tempfile::tempdir().unwrap();
    let state = observed(&host);
    let mut c = owner(&dir);
    c.reconcile_once(&state, &host.client).await.unwrap();
    let node = &host.node.config.node_id;
    let session = c.responsibilities()[node].session;
    for phase in [
        MirrorIntentPhase::Allocating,
        MirrorIntentPhase::Applying,
        MirrorIntentPhase::Closing,
    ] {
        if phase == MirrorIntentPhase::Closing {
            let mut next = c.desired().clone();
            next.mirrors.clear();
            c.submit(next).unwrap();
        }
        {
            let request = c.reconcile_once(&state, &host.client);
            tokio::pin!(request);
            assert!(
                futures::poll!(&mut request).is_pending(),
                "actual transport response must be in flight"
            );
        }
        assert_eq!(c.responsibilities()[node].phase, phase);
        // Observe the remote side effect after the requester has been cancelled.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let reply = host
                    .client
                    .control_mirror(node, C::Status { session })
                    .await;
                let reached = match phase {
                    MirrorIntentPhase::Allocating => matches!(
                        reply,
                        Ok(R::Session {
                            phase: Phase::Empty,
                            ..
                        })
                    ),
                    MirrorIntentPhase::Applying => matches!(
                        reply,
                        Ok(R::Session {
                            phase: Phase::Active,
                            ..
                        })
                    ),
                    MirrorIntentPhase::Closing => matches!(reply, Ok(R::Retired { .. })),
                    _ => unreachable!(),
                };
                if reached {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled caller must not erase remote side effect");
        drop(c);
        c = owner(&dir);
        c.reconcile_once(&state, &host.client).await.unwrap();
    }
    assert!(c.responsibilities().is_empty());
    host.close().await;
}
#[tokio::test]
async fn lost_source_link_does_not_erase_remote_retirement_responsibility() {
    let host = Host::new(false, true).await;
    let dir = tempfile::tempdir().unwrap();
    let state = observed(&host);
    let mut c = owner(&dir);
    converge(&mut c, &host, &state).await;
    let session = c.responsibilities()[&host.node.config.node_id].session;
    // An offline observation is not itself a retirement acknowledgement. Even
    // with no desired placement the controller still obtains a real Close reply.
    let mut offline = state.clone();
    offline.live_nodes.clear();
    offline.components.clear();
    offline.component_nodes.clear();
    assert!(
        !c.reconcile_once(&offline, &host.client)
            .await
            .unwrap()
            .converged
    );
    assert_eq!(
        host.command(C::Status { session }).await,
        R::Retired { session }
    );
    assert!(c.responsibilities().is_empty());
    host.close().await;
}
#[tokio::test]
async fn plain_supervisor_cannot_bypass_pending_retirement_after_manifest_removal() {
    use parrot_app::orchestrator::{AppSupervisor, FileStateStore};
    let host = Host::new(false, true).await;
    let dir = tempfile::tempdir().unwrap();
    let state = observed(&host);
    let mut c = owner(&dir);
    converge(&mut c, &host, &state).await;
    let mut next = c.desired().clone();
    next.mirrors.clear();
    c.submit(next.clone()).unwrap();
    drop(c);
    let store = Arc::new(FileStateStore::open(dir.path(), ApplicationLimits::default()).unwrap());
    assert!(AppSupervisor::new(next, store.clone(), Arc::new(SystemClock)).is_err());
    drop(store);
    let mut c = owner(&dir);
    converge(&mut c, &host, &state).await;
    assert!(c.responsibilities().is_empty());
    host.close().await;
}
#[tokio::test]
async fn fenced_allocation_still_requires_management_authorization() {
    let host = Host::new(true, false).await;
    host.reject(C::Candidate).await;
    host.reject(C::Allocate {
        owner: "forbidden".into(),
        session: parrot_remote::mirror_control::MirrorSessionId {
            incarnation: [1; 16],
            sequence: 1,
        },
    })
    .await;
    host.close().await;
}

async fn cross_node(tcp: bool) {
    use parrot_remote::mirror::{WireMirrorOptions, WireMirrorPolicy};
    let primary = Host::new(tcp, true).await;
    let shadow = Host::new(tcp, true).await;
    let address = if tcp {
        NodeAddr::tcp(
            &shadow.node.config.node_id,
            shadow.node.local_addr().unwrap(),
        )
    } else {
        NodeAddr::mem(&shadow.node.config.node_id)
    };
    primary.node.connect(&address).await.unwrap();
    // A reverse mirror would form a cycle if the wire shadow marker were lost.
    let reverse = shadow
        .node
        .register_wire_mirror(
            WireMirrorPolicy::new(
                "/user/shadow",
                format!("parrot://{}/user/primary", primary.node.config.node_id),
            ),
            WireMirrorOptions::default(),
        )
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut c = owner(&dir);
    let mut state = observed(&primary);
    state.live_nodes.push(shadow.node.config.node_id.clone());
    state
        .component_nodes
        .insert("shadow".into(), shadow.node.config.node_id.clone());
    converge(&mut c, &primary, &state).await;
    let policy = &c.responsibilities()[&primary.node.config.node_id].policies[0];
    assert_eq!(
        policy.shadow_path,
        format!("parrot://{}/user/shadow", shadow.node.config.node_id)
    );
    drop(c);
    let mut c = owner(&dir);
    assert!(
        c.reconcile_once(&state, &primary.client)
            .await
            .unwrap()
            .converged
    );
    let root = TraceContext::root();
    parrot_api::trace::scope(Some(root.clone()), primary.send(700)).await;
    shadow.wait("shadow", 700).await;
    assert!(
        !primary
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, value, _)| *name == "shadow" && *value == 700),
        "remote shadow must not resolve to a local namesake"
    );
    assert_eq!(
        reverse.stats().enqueued,
        0,
        "accepted shadow must never be mirrored again"
    );
    let remote_context = shadow
        .seen
        .lock()
        .unwrap()
        .iter()
        .find(|(name, value, _)| *name == "shadow" && *value == 700)
        .unwrap()
        .2
        .clone()
        .unwrap();
    assert_eq!(remote_context.trace_id(), root.trace_id());
    let mut desired = c.desired().clone();
    desired.mirrors.clear();
    c.submit(desired).unwrap();
    converge(&mut c, &primary, &state).await;
    primary.send(701).await;
    assert!(!shadow
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|(_, value, _)| *value == 701));
    reverse.close();
    reverse.wait_closed().await;
    primary.close().await;
    shadow.close().await;
}
#[tokio::test]
async fn cross_node_memory_shadow_preserves_trace_and_cannot_loop() {
    cross_node(false).await;
}
#[tokio::test]
async fn cross_node_tcp_shadow_preserves_trace_and_cannot_loop() {
    cross_node(true).await;
}

struct ForwardTo(String);
#[async_trait::async_trait]
impl LocalLookup for ForwardTo {
    async fn lookup(&self, _: &str) -> Option<Box<dyn ActorRef>> {
        None
    }
    fn forwarding_target(&self, path: &str) -> Option<String> {
        (path == "/user/primary").then(|| format!("parrot://{}/user/primary", self.0))
    }
}
#[tokio::test]
async fn source_process_fixture() {
    let Some(ready) = std::env::var_os("PARROT_MIRROR_SOURCE_READY") else {
        let bytes = parrot_api::message::serde_remote_serialize(&Work(41)).unwrap();
        let decoded = parrot_remote::codec_registry::CodecRegistry::global()
            .decode_incoming("bin:orchestrated-mirror#v1", &bytes)
            .unwrap();
        assert_eq!(decoded.downcast_ref::<Work>().unwrap().0, 41);
        return;
    };
    assert!(std::env::var_os("PARROT_MIRROR_SOURCE_NO_DECODER").is_some());
    let mut config = RemoteConfig::tcp(
        format!("parrot-source-{}", std::process::id()),
        Some("127.0.0.1:0".parse().unwrap()),
    );
    config.security = RemoteSecurity::InsecureDevelopment;
    let source = RemoteActorSystem::new(
        config,
        Arc::new(ForwardTo(
            std::env::var("PARROT_MIRROR_PARENT_NODE").unwrap(),
        )),
    )
    .unwrap();
    source.start().await.unwrap();
    let ready = std::path::PathBuf::from(ready);
    let temporary = ready.with_extension("next");
    std::fs::write(
        &temporary,
        serde_json::to_vec(&(source.config.node_id.clone(), source.local_addr().unwrap())).unwrap(),
    )
    .unwrap();
    std::fs::rename(temporary, ready).unwrap();
    tokio::task::spawn_blocking(|| {
        use std::io::Read;
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).unwrap();
    })
    .await
    .unwrap();
    source.shutdown().await.unwrap();
}
struct ChildOwner(std::process::Child);
impl Drop for ChildOwner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[tokio::test]
async fn isolated_source_process_forwards_primary_and_remote_shadow_without_business_decoder() {
    let parent = Host::new(true, true).await;
    let root = std::env::var_os("PARROT_APP_MIRROR_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("cross-node-source-")
        .tempdir_in(root)
        .unwrap()
        .keep();
    let ready = directory.join("ready.json");
    let log = std::fs::File::create(directory.join("source.log")).unwrap();
    let mut child = ChildOwner(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "source_process_fixture", "--nocapture"])
            .env("PARROT_MIRROR_SOURCE_READY", &ready)
            .env("PARROT_MIRROR_SOURCE_NO_DECODER", "1")
            .env("PARROT_MIRROR_PARENT_NODE", &parent.node.config.node_id)
            .stdin(std::process::Stdio::piped())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "source exited before readiness"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let (node, address): (String, std::net::SocketAddr) =
        serde_json::from_slice(&std::fs::read(ready).unwrap()).unwrap();
    parent
        .node
        .connect(&NodeAddr::tcp(&node, address))
        .await
        .unwrap();
    let session = parent
        .node
        .control_mirror(
            &node,
            C::Open {
                owner: "opaque-source".into(),
            },
        )
        .await
        .unwrap()
        .session();
    parent
        .node
        .control_mirror(
            &node,
            C::Apply {
                session,
                revision: 1,
                policies: vec![P::new(
                    "/user/primary",
                    format!("parrot://{}/user/shadow", parent.node.config.node_id),
                )],
            },
        )
        .await
        .unwrap();
    let actor = parent
        .node
        .remote_ref(&format!("parrot://{node}/user/primary"))
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), actor.send(Box::new(Work(41))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.downcast_ref::<Work>().unwrap().0, 42);
    parent.wait("shadow", 41).await;
    assert_eq!(
        parent
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, value, _)| *name == "primary" && *value == 41)
            .count(),
        1
    );
    assert_eq!(
        parent
            .node
            .control_mirror(&node, C::Close { session })
            .await
            .unwrap(),
        R::Retired { session }
    );
    child.0.stdin.take();
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(status.success());
    parent.close().await;
}
