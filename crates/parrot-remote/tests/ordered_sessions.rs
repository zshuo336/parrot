use bytes::Bytes;
#[path = "ordered_sessions/admission_measurements.rs"]
mod admission_measurements;
use parrot_api::{
    address::ActorRef,
    message::{MessageOptions, RemoteMessage},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    frame::{frame_type, Frame},
    handshake::{caps, HandshakeBody, TopologyRole},
    ordered::{CONTROL_KEY, DATA_KEY, RECEIPT_KEY},
    transport::{run_connection, ConnParams, ConnSide, ConnectionHandle, RuntimeKnobs},
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Work(u64);
impl parrot_api::Message for Work {
    type Result = ();
}

#[derive(Debug)]
struct State {
    seen: tokio::sync::mpsc::UnboundedSender<u64>,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    dropping: tokio::sync::Notify,
    dropped: AtomicBool,
    gate: (Mutex<bool>, Condvar),
}
fn state() -> (Arc<State>, tokio::sync::mpsc::UnboundedReceiver<u64>) {
    let (seen, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(State {
            seen,
            entered: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
            dropping: tokio::sync::Notify::new(),
            dropped: AtomicBool::new(false),
            gate: (Mutex::new(false), Condvar::new()),
        }),
        receiver,
    )
}
struct Release(Arc<State>);
impl Release {
    fn release(&self) {
        *self.0.gate.0.lock().unwrap() = true;
        self.0.gate.1.notify_all();
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        self.release();
    }
}
struct HeldDrop(Arc<State>);
impl Future for HeldDrop {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.entered.notify_one();
        Poll::Pending
    }
}
impl Drop for HeldDrop {
    fn drop(&mut self) {
        self.0.dropping.notify_one();
        let mut released = self.0.gate.0.lock().unwrap();
        while !*released {
            released = self.0.gate.1.wait(released).unwrap();
        }
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}
#[derive(Clone, Debug)]
struct Target(Arc<State>);
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
#[async_trait::async_trait]
impl LocalLookup for Target {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/target").then(|| Box::new(self.clone()) as BoxedActorRef)
    }
}
impl ActorRef for Target {
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        let value = message.downcast_ref::<Work>().unwrap().0;
        if value == 700 {
            return Box::pin(HeldDrop(self.0.clone()));
        }
        Box::pin(async move {
            if value == 701 {
                self.0.entered.notify_one();
                self.0.resume.notified().await;
            }
            self.0.seen.send(value).unwrap();
            Ok(())
        })
    }
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
async fn start(mut config: RemoteConfig, lookup: Arc<dyn LocalLookup>) -> Arc<RemoteActorSystem> {
    config.heartbeat_enabled = false;
    let system = RemoteActorSystem::new(config, lookup).unwrap();
    system.start().await.unwrap();
    system
}
async fn seen(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<u64>, value: u64) {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .unwrap(),
        Some(value)
    );
}
async fn strict(reference: &parrot_remote::RemoteActorRef, value: u64) {
    reference
        .deliver_ordered(
            Box::new(Work(value)),
            MessageOptions {
                timeout: Some(Duration::from_secs(4)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn authenticated_memory_hub_keeps_sources_independent_and_closes_sessions() {
    let (state, mut receiver) = state();
    let hub = start(
        RemoteConfig::mem("ordered-hub").with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    let target = start(
        RemoteConfig::mem("ordered-target").with_ordered_relays(["ordered-hub".into()]),
        Arc::new(Target(state)),
    )
    .await;
    let first = start(RemoteConfig::mem("ordered-first"), Arc::new(Empty)).await;
    let second = start(RemoteConfig::mem("ordered-second"), Arc::new(Empty)).await;
    for source in [&first, &second, &target] {
        source.connect_mem_pair(&hub).await.unwrap();
    }
    let a = first.remote_ref("parrot://ordered-target/target").unwrap();
    let b = second.remote_ref("parrot://ordered-target/target").unwrap();
    for value in 1..=5 {
        strict(&a, value).await;
        strict(&b, value + 10).await;
    }
    let mut actual = Vec::new();
    for _ in 0..10 {
        actual.push(
            tokio::time::timeout(Duration::from_secs(3), receiver.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(
        actual
            .iter()
            .copied()
            .filter(|&value| value < 10)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(
        actual
            .iter()
            .copied()
            .filter(|&value| value > 10)
            .collect::<Vec<_>>(),
        vec![11, 12, 13, 14, 15]
    );
    a.close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    strict(&a, 20).await;
    seen(&mut receiver, 20).await;
    for node in [&first, &second, &hub, &target] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn saturated_frame_budget_preserves_the_shared_hub_and_next_ordered_sequence() {
    check_ordered_saturation("frames", 1, 1 << 20).await;
}

#[tokio::test]
async fn saturated_byte_budget_preserves_the_shared_hub_and_next_ordered_sequence() {
    check_ordered_saturation("bytes", 32, 256).await;
}

async fn check_ordered_saturation(label: &str, frames: usize, bytes: usize) {
    let hub_id = format!("busy-hub-{label}");
    let target_id = format!("busy-target-{label}");
    let path = format!("parrot://{target_id}/target");
    let (target_state, mut rx) = state();
    let hub = start(
        RemoteConfig::mem(&hub_id).with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    let target = start(
        RemoteConfig::mem(&target_id)
            .with_ordered_relays([hub_id])
            .with_knobs(RuntimeKnobs {
                reorder_frame_capacity: frames,
                reorder_byte_capacity: bytes,
                reorder_gap_timeout_ms: 20_000,
                ..Default::default()
            }),
        Arc::new(Target(target_state.clone())),
    )
    .await;
    let first = start(
        RemoteConfig::mem(format!("busy-first-{label}")),
        Arc::new(Empty),
    )
    .await;
    let second = start(
        RemoteConfig::mem(format!("busy-second-{label}")),
        Arc::new(Empty),
    )
    .await;
    for node in [&first, &second, &target] {
        node.connect_mem_pair(&hub).await.unwrap();
    }
    let held = first.remote_ref(&path).unwrap();
    let busy = second.remote_ref(&path).unwrap();
    let [(_, downstream, _)] = target
        .links_snapshot()
        .await
        .try_into()
        .unwrap_or_else(|_| panic!("expected one hub connection"));
    strict(&held, 701).await;
    tokio::time::timeout(Duration::from_secs(3), target_state.entered.notified())
        .await
        .unwrap();
    let error = busy
        .deliver_ordered(Box::new(Work(2)), Default::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("remote overloaded"), "{error}");
    assert!(rx.try_recv().is_err());
    // The same downstream connection still serves this other source before any budget is released.
    let echo = held
        .send_with_timeout(Box::new(Work(77)), Some(Duration::from_secs(3)))
        .await
        .unwrap();
    assert_eq!(echo.downcast_ref::<Work>().unwrap().0, 77);
    assert!(!downstream.is_closed());
    assert!(target
        .links_snapshot()
        .await
        .iter()
        .any(|(_, link, _)| link.same_connection(&downstream)));
    target_state.resume.notify_one();
    seen(&mut rx, 701).await;
    held.close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    strict(&busy, 3).await;
    // The configured gap deadline is 20 seconds. A consumed rejected sequence would stall here.
    seen(&mut rx, 3).await;
    busy.close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    assert!(rx.try_recv().is_err());
    for node in [&first, &second, &hub, &target] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn strict_delivery_rejects_missing_relay_grants_but_ordinary_delivery_can_use_legacy() {
    let (state, mut receiver) = state();
    let hub = start(
        RemoteConfig::mem("ungranted-hub").with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    let target = start(
        RemoteConfig::mem("ungranted-target"),
        Arc::new(Target(state)),
    )
    .await;
    let source = start(
        RemoteConfig::mem("ungranted-source").with_knobs(RuntimeKnobs {
            reorder_stream_capacity: 1,
            ..Default::default()
        }),
        Arc::new(Empty),
    )
    .await;
    source.connect_mem_pair(&hub).await.unwrap();
    target.connect_mem_pair(&hub).await.unwrap();
    let reference = source
        .remote_ref("parrot://ungranted-target/target")
        .unwrap();
    assert!(reference
        .deliver_ordered(Box::new(Work(1)), Default::default())
        .await
        .is_err());
    reference.deliver(Box::new(Work(2))).await.unwrap();
    seen(&mut receiver, 2).await;
    assert!(receiver.try_recv().is_err());
    let (direct_state, mut direct_rx) = self::state();
    let direct = start(
        RemoteConfig::mem("direct-target"),
        Arc::new(Target(direct_state)),
    )
    .await;
    source.connect_mem_pair(&direct).await.unwrap();
    let direct_ref = source.remote_ref("parrot://direct-target/target").unwrap();
    strict(&direct_ref, 3).await;
    seen(&mut direct_rx, 3).await;
    for node in [&source, &hub, &target, &direct] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn plain_tcp_destinations_do_not_consume_ordered_session_capacity() {
    let source = start(
        RemoteConfig::tcp("plain-source", None).with_knobs(RuntimeKnobs {
            reorder_stream_capacity: 1,
            ..Default::default()
        }),
        Arc::new(Empty),
    )
    .await;
    let mut targets = Vec::new();
    for (name, value) in [("plain-first", 1), ("plain-second", 2)] {
        let (target_state, mut receiver) = state();
        let target = start(
            RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap())),
            Arc::new(Target(target_state)),
        )
        .await;
        source
            .connect(&NodeAddr::tcp(name, target.local_addr().unwrap()))
            .await
            .unwrap();
        let reference = source
            .remote_ref(&format!("parrot://{name}/target"))
            .unwrap();
        reference.deliver(Box::new(Work(value))).await.unwrap();
        seen(&mut receiver, value).await;
        targets.push(target);
    }
    source.shutdown().await.unwrap();
    for target in targets {
        target.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn plain_tcp_falls_back_without_claiming_authenticated_ordering() {
    let (state, mut receiver) = state();
    let target = start(
        RemoteConfig::tcp("plain-order-target", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Target(state)),
    )
    .await;
    let source = start(
        RemoteConfig::tcp("plain-order-source", None),
        Arc::new(Empty),
    )
    .await;
    source
        .connect(&NodeAddr::tcp(
            "plain-order-target",
            target.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let reference = source
        .remote_ref("parrot://plain-order-target/target")
        .unwrap();
    assert!(reference
        .deliver_ordered(Box::new(Work(1)), Default::default())
        .await
        .is_err());
    assert!(source.callbacks.is_empty());
    reference.deliver(Box::new(Work(2))).await.unwrap();
    seen(&mut receiver, 2).await;
    assert!(receiver.try_recv().is_err());
    for node in [&source, &target] {
        node.shutdown().await.unwrap();
    }
}

struct Pki {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    ca: Vec<u8>,
}
impl Pki {
    fn new() -> Self {
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().unwrap();
        let ca = params.self_signed(&key).unwrap().pem().into_bytes();
        Self {
            issuer: rcgen::Issuer::new(params, key),
            ca,
        }
    }
    fn node(&self, name: &str) -> parrot_remote::tls::TlsConfig {
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        parrot_remote::tls::TlsConfig {
            cert_pem: cert.pem().into_bytes(),
            key_pem: key.serialize_pem().into_bytes(),
            ca_pem: self.ca.clone(),
        }
    }
}
#[tokio::test]
async fn strict_sessions_execute_over_real_mtls_tcp_and_quic() {
    let pki = Pki::new();
    for scheme in ["tcp", "quic"] {
        let (state, mut receiver) = state();
        let source_name = format!("ordered-source-{scheme}");
        let target_name = format!("ordered-target-{scheme}");
        let config = |name: &str| {
            let bind = Some("127.0.0.1:0".parse().unwrap());
            let config = if scheme == "tcp" {
                RemoteConfig::tcp(name, bind)
            } else {
                RemoteConfig::quic(name, bind)
            };
            config.with_mtls(pki.node(name), [])
        };
        let source = start(config(&source_name), Arc::new(Empty)).await;
        let target = start(config(&target_name), Arc::new(Target(state))).await;
        let address = if scheme == "tcp" {
            NodeAddr::tcp(&target_name, target.local_addr().unwrap())
        } else {
            NodeAddr::quic(&target_name, target.local_addr().unwrap())
        };
        source.connect(&address).await.unwrap();
        let reference = source
            .remote_ref(&format!("parrot://{target_name}/target"))
            .unwrap();
        for value in 1..=3 {
            strict(&reference, value).await;
            seen(&mut receiver, value).await;
        }
        reference
            .close_ordered_session(Duration::from_secs(3))
            .await
            .unwrap();
        source.shutdown().await.unwrap();
        target.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn idle_expiry_and_explicitly_retired_receiver_recreation_get_new_leases() {
    let (state, mut receiver) = state();
    let source = start(RemoteConfig::mem("expiry-source"), Arc::new(Empty)).await;
    let target = start(
        RemoteConfig::mem("expiry-target"),
        Arc::new(Target(state.clone())),
    )
    .await;
    source.connect_mem_pair(&target).await.unwrap();
    let reference = source.remote_ref("parrot://expiry-target/target").unwrap();
    strict(&reference, 1).await;
    seen(&mut receiver, 1).await;
    tokio::time::advance(Duration::from_secs(31)).await;
    strict(&reference, 2).await;
    seen(&mut receiver, 2).await;
    reference
        .close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    target.shutdown().await.unwrap();
    drop(target);
    let target = start(RemoteConfig::mem("expiry-target"), Arc::new(Target(state))).await;
    source.connect_mem_pair(&target).await.unwrap();
    strict(&reference, 3).await;
    seen(&mut receiver, 3).await;
    reference
        .close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacement_receiver_cannot_certify_retirement_of_the_previous_instance() {
    let (old_state, _old_receiver) = state();
    let release = Release(old_state.clone());
    let old = start(
        RemoteConfig::tcp("replacement-target", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Target(old_state.clone())),
    )
    .await;
    let source = start(
        RemoteConfig::tcp("replacement-source", None).with_insecure_development(),
        Arc::new(Empty),
    )
    .await;
    source
        .connect(&NodeAddr::tcp(
            "replacement-target",
            old.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let reference = source
        .remote_ref("parrot://replacement-target/target")
        .unwrap();
    strict(&reference, 700).await;
    tokio::time::timeout(Duration::from_secs(3), old_state.entered.notified())
        .await
        .unwrap();
    let retiring = old.clone();
    let shutdown = tokio::spawn(async move { retiring.shutdown().await });
    tokio::time::timeout(Duration::from_secs(3), old_state.dropping.notified())
        .await
        .unwrap();

    let (new_state, mut new_receiver) = state();
    let replacement = start(
        RemoteConfig::tcp("replacement-target", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Target(new_state)),
    )
    .await;
    source
        .connect(&NodeAddr::tcp(
            "replacement-target",
            replacement.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let result = reference
        .close_ordered_session(Duration::from_secs(2))
        .await;
    assert!(!old_state.dropped.load(Ordering::SeqCst));
    assert!(
        result.is_err(),
        "a replacement receiver certified old retirement while the original Drop was blocked"
    );
    for ordinary in [false, true] {
        let result = if ordinary {
            reference.deliver(Box::new(Work(99))).await
        } else {
            reference
                .deliver_ordered(Box::new(Work(99)), Default::default())
                .await
        };
        assert!(
            result.is_err(),
            "replacement bypassed unresolved predecessor"
        );
    }
    assert!(new_receiver.try_recv().is_err());
    release.release();
    shutdown.await.unwrap().unwrap();
    source.shutdown().await.unwrap();
    replacement.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_receiver_handoff_waits_for_original_drop_then_delivers_to_the_replacement() {
    let (old_state, _old_receiver) = state();
    let release = Release(old_state.clone());
    let old = start(
        RemoteConfig::tcp(
            "live-replacement-target",
            Some("127.0.0.1:0".parse().unwrap()),
        )
        .with_insecure_development(),
        Arc::new(Target(old_state.clone())),
    )
    .await;
    let source = start(
        RemoteConfig::tcp("live-replacement-source", None).with_insecure_development(),
        Arc::new(Empty),
    )
    .await;
    source
        .connect(&NodeAddr::tcp(
            "live-replacement-target",
            old.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let reference = source
        .remote_ref("parrot://live-replacement-target/target")
        .unwrap();
    strict(&reference, 700).await;
    tokio::time::timeout(Duration::from_secs(3), old_state.entered.notified())
        .await
        .unwrap();
    let (new_state, mut receiver) = state();
    let replacement = start(
        RemoteConfig::tcp(
            "live-replacement-target",
            Some("127.0.0.1:0".parse().unwrap()),
        )
        .with_insecure_development(),
        Arc::new(Target(new_state)),
    )
    .await;
    source
        .connect(&NodeAddr::tcp(
            "live-replacement-target",
            replacement.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let sending = reference.clone();
    let attempt = tokio::spawn(async move {
        sending
            .deliver_ordered(Box::new(Work(1)), Default::default())
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), old_state.dropping.notified())
        .await
        .expect("handoff never retired its original receiver");
    // The held synchronous destructor may own Tokio's timer driver. Observe
    // the barrier with an independent wall clock so the test can release it.
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(100)))
        .await
        .unwrap();
    assert!(
        !attempt.is_finished(),
        "replacement committed before the original worker's actual destructor finished"
    );
    assert!(!old_state.dropped.load(Ordering::SeqCst));
    assert!(receiver.try_recv().is_err());
    release.release();
    attempt.await.unwrap().unwrap();
    assert!(old_state.dropped.load(Ordering::SeqCst));
    seen(&mut receiver, 1).await;
    strict(&reference, 2).await;
    seen(&mut receiver, 2).await;
    reference
        .close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    assert!(receiver.try_recv().is_err());
    for node in [&source, &old, &replacement] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_recreation_and_cancelled_intermediate_handoff_cannot_bypass_actual_old_drop() {
    let (state, mut receiver) = state();
    let release = Release(state.clone());
    let target = start(
        RemoteConfig::mem("handoff-target"),
        Arc::new(Target(state.clone())),
    )
    .await;
    let first = start(RemoteConfig::mem("handoff-source"), Arc::new(Empty)).await;
    first.connect_mem_pair(&target).await.unwrap();
    let reference = first.remote_ref("parrot://handoff-target/target").unwrap();
    strict(&reference, 700).await;
    tokio::time::timeout(Duration::from_secs(3), state.entered.notified())
        .await
        .unwrap();
    first.shutdown().await.unwrap();
    drop(first);
    let second = start(RemoteConfig::mem("handoff-source"), Arc::new(Empty)).await;
    second.connect_mem_pair(&target).await.unwrap();
    let next = second.remote_ref("parrot://handoff-target/target").unwrap();
    let mut attempt = tokio::spawn(async move {
        next.deliver_ordered(Box::new(Work(1)), Default::default())
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), state.dropping.notified())
        .await
        .unwrap();
    assert!(
        !attempt.is_finished(),
        "COMMIT acknowledged while actual old future Drop was blocked"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut attempt)
            .await
            .is_err(),
        "COMMIT completed before the old future's real destructor finished"
    );
    second.shutdown().await.unwrap();
    assert!(attempt.await.unwrap().is_err());
    drop(second);
    let third = start(RemoteConfig::mem("handoff-source"), Arc::new(Empty)).await;
    third.connect_mem_pair(&target).await.unwrap();
    let next = third.remote_ref("parrot://handoff-target/target").unwrap();
    assert!(
        next.deliver_ordered(Box::new(Work(2)), Default::default())
            .await
            .is_err(),
        "a third incarnation bypassed old cleanup"
    );
    assert!(!state.dropped.load(Ordering::SeqCst));
    assert!(receiver.try_recv().is_err());
    release.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match next
                .deliver_ordered(Box::new(Work(3)), Default::default())
                .await
            {
                Ok(()) => break,
                Err(error) if error.to_string().contains("remote overloaded") => {
                    tokio::task::yield_now().await
                }
                Err(error) => panic!("unexpected handoff recovery failure: {error}"),
            }
        }
    })
    .await
    .unwrap();
    seen(&mut receiver, 3).await;
    assert!(receiver.try_recv().is_err());
    third.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_commit_and_retried_close_wait_for_actual_predecessor_cleanup() {
    let (state, mut receiver) = state();
    let release = Release(state.clone());
    let target = start(
        RemoteConfig::mem("close-handoff-target"),
        Arc::new(Target(state.clone())),
    )
    .await;
    let first = start(RemoteConfig::mem("close-handoff-source"), Arc::new(Empty)).await;
    first.connect_mem_pair(&target).await.unwrap();
    let reference = first
        .remote_ref("parrot://close-handoff-target/target")
        .unwrap();
    strict(&reference, 700).await;
    tokio::time::timeout(Duration::from_secs(3), state.entered.notified())
        .await
        .unwrap();
    first.shutdown().await.unwrap();
    drop(first);

    let second = start(RemoteConfig::mem("close-handoff-source"), Arc::new(Empty)).await;
    second.connect_mem_pair(&target).await.unwrap();
    let reference = second
        .remote_ref("parrot://close-handoff-target/target")
        .unwrap();
    let sending = reference.clone();
    let attempt = tokio::spawn(async move {
        sending
            .deliver_ordered(Box::new(Work(1)), Default::default())
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), state.dropping.notified())
        .await
        .unwrap();
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    assert!(matches!(
        reference
            .close_ordered_session(Duration::from_millis(100))
            .await,
        Err(parrot_api::errors::ActorError::Timeout
            | parrot_api::errors::ActorError::TimeoutDetail(_))
    ));
    let closing = reference.clone();
    let mut retry =
        tokio::spawn(async move { closing.close_ordered_session(Duration::from_secs(3)).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut retry)
            .await
            .is_err(),
        "a repeated CLOSE acknowledged while its predecessor's real Drop was blocked"
    );
    assert!(!state.dropped.load(Ordering::SeqCst));
    assert!(receiver.try_recv().is_err());
    release.release();
    retry.await.unwrap().unwrap();
    assert!(state.dropped.load(Ordering::SeqCst));
    strict(&reference, 2).await;
    seen(&mut receiver, 2).await;
    assert!(receiver.try_recv().is_err());
    second.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn explicit_close_reclaims_the_bounded_outgoing_destination_slot() {
    let (state, mut receiver) = state();
    let source = start(
        RemoteConfig::mem("slot-source").with_knobs(RuntimeKnobs {
            reorder_stream_capacity: 1,
            ..Default::default()
        }),
        Arc::new(Empty),
    )
    .await;
    let first = start(
        RemoteConfig::mem("slot-first"),
        Arc::new(Target(state.clone())),
    )
    .await;
    let second = start(RemoteConfig::mem("slot-second"), Arc::new(Target(state))).await;
    source.connect_mem_pair(&first).await.unwrap();
    source.connect_mem_pair(&second).await.unwrap();
    let a = source.remote_ref("parrot://slot-first/target").unwrap();
    let b = source.remote_ref("parrot://slot-second/target").unwrap();
    strict(&a, 1).await;
    seen(&mut receiver, 1).await;
    let error = b
        .deliver_ordered(Box::new(Work(99)), Default::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("remote overloaded"), "{error}");
    a.close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    strict(&b, 2).await;
    seen(&mut receiver, 2).await;
    b.close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    strict(&a, 3).await;
    seen(&mut receiver, 3).await;
    assert!(receiver.try_recv().is_err());
    for node in [&source, &first, &second] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_closes_observe_one_completed_retirement() {
    let (state, _rx) = state();
    let release = Release(state.clone());
    let target = start(
        RemoteConfig::mem("joined-target"),
        Arc::new(Target(state.clone())),
    )
    .await;
    let source = start(RemoteConfig::mem("joined-source"), Arc::new(Empty)).await;
    source.connect_mem_pair(&target).await.unwrap();
    let reference = source.remote_ref("parrot://joined-target/target").unwrap();
    strict(&reference, 700).await;
    let timeout = Duration::from_secs(3);
    tokio::time::timeout(timeout, state.entered.notified())
        .await
        .unwrap();
    let first = tokio::spawn({
        let reference = reference.clone();
        async move { reference.close_ordered_session(timeout).await }
    });
    tokio::time::timeout(timeout, state.dropping.notified())
        .await
        .unwrap();
    let mut second = Box::pin(reference.close_ordered_session(timeout));
    assert!(futures::poll!(&mut second).is_pending());
    assert!(!state.dropped.load(Ordering::SeqCst));
    release.release();
    first.await.unwrap().unwrap();
    target.shutdown().await.unwrap();
    second.await.unwrap();
    assert!(state.dropped.load(Ordering::SeqCst));
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn repeated_close_after_peer_disconnect_requires_no_new_route() {
    for retired in [true, false] {
        check_disconnected_close(retired).await;
    }
}

async fn check_disconnected_close(retired: bool) {
    let (target_state, mut rx) = state();
    let target = start(
        RemoteConfig::mem("departing-target"),
        Arc::new(Target(target_state)),
    )
    .await;
    let source = start(
        RemoteConfig::mem("closing-source").with_knobs(RuntimeKnobs {
            reorder_stream_capacity: 1,
            ..Default::default()
        }),
        Arc::new(Empty),
    )
    .await;
    source.connect_mem_pair(&target).await.unwrap();
    let reference = source
        .remote_ref("parrot://departing-target/target")
        .unwrap();
    strict(&reference, 1).await;
    seen(&mut rx, 1).await;
    let timeout = Duration::from_secs(3);
    if retired {
        reference.close_ordered_session(timeout).await.unwrap();
    }
    target.shutdown().await.unwrap();
    tokio::time::timeout(timeout, async {
        while source
            .missing_direct_connections(&["departing-target"])
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        reference.close_ordered_session(timeout).await.is_ok(),
        retired
    );
    assert!(reference
        .close_ordered_session(Duration::ZERO)
        .await
        .is_err());
    let (other_state, mut other_rx) = state();
    let other = start(
        RemoteConfig::mem("other-target"),
        Arc::new(Target(other_state)),
    )
    .await;
    source.connect_mem_pair(&other).await.unwrap();
    let dest = source.remote_ref("parrot://other-target/target").unwrap();
    let result = dest
        .deliver_ordered(Box::new(Work(2)), Default::default())
        .await;
    if retired {
        result.unwrap();
        seen(&mut other_rx, 2).await;
    } else {
        assert!(result.unwrap_err().to_string().contains("capacity"));
        assert!(other_rx.try_recv().is_err());
    }
    source.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
    assert!(reference.close_ordered_session(timeout).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_request_admission_is_not_a_session_retirement_receipt() {
    let (state, _receiver) = state();
    let release = Release(state.clone());
    let target = start(
        RemoteConfig::mem("closed-admission-target"),
        Arc::new(Target(state.clone())),
    )
    .await;
    let source = start(
        RemoteConfig::mem("closed-admission-source"),
        Arc::new(Empty),
    )
    .await;
    source.connect_mem_pair(&target).await.unwrap();
    let reference = source
        .remote_ref("parrot://closed-admission-target/target")
        .unwrap();
    strict(&reference, 700).await;
    tokio::time::timeout(Duration::from_secs(3), state.entered.notified())
        .await
        .unwrap();
    target.ingress.close_asks();
    let result = reference
        .close_ordered_session(Duration::from_secs(3))
        .await;
    assert!(
        result.is_err(),
        "generic admission Stopped was treated as proof of actual session retirement"
    );
    assert!(!state.dropped.load(Ordering::SeqCst));
    release.release();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

struct Forwarding;
#[async_trait::async_trait]
impl LocalLookup for Forwarding {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
    fn forwarding_target(&self, path: &str) -> Option<String> {
        (path == "/target").then(|| "parrot://alias-target/target".into())
    }
}

struct MixedPaths {
    local: Target,
    alias_to: String,
    forward: AtomicBool,
}
#[async_trait::async_trait]
impl LocalLookup for MixedPaths {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        // Include a real local namesake. It must never receive the alias traffic.
        matches!(path, "/local" | "/alias").then(|| self.local.clone_boxed())
    }
    fn forwarding_target(&self, path: &str) -> Option<String> {
        (path == "/alias" && self.forward.load(Ordering::Acquire)).then(|| self.alias_to.clone())
    }
}

#[tokio::test]
async fn warmed_direct_ticket_cannot_admit_an_unconfirmed_alias_path() {
    check_warmed_alias(false, false).await;
}

#[tokio::test]
async fn warmed_routed_ticket_cannot_admit_an_unconfirmed_alias_path() {
    check_warmed_alias(true, false).await;
}

#[tokio::test]
async fn warmed_direct_path_changed_to_alias_cannot_capture_the_local_namesake() {
    check_warmed_alias(false, true).await;
}

#[tokio::test]
async fn warmed_routed_path_changed_to_alias_cannot_capture_the_local_namesake() {
    check_warmed_alias(true, true).await;
}

async fn check_warmed_alias(routed: bool, change_same_path: bool) {
    let label = format!("{routed}-{change_same_path}");
    let host_id = format!("warm-host-{label}");
    let hub_id = format!("warm-hub-{label}");
    let end_id = format!("warm-end-{label}");
    let (actual, mut actual_rx) = state();
    let (local, mut local_rx) = state();
    let end = start(RemoteConfig::mem(&end_id), Arc::new(Target(actual))).await;
    let routes = Arc::new(MixedPaths {
        local: Target(local),
        alias_to: format!("parrot://{end_id}/target"),
        forward: AtomicBool::new(!change_same_path),
    });
    let host = start(
        RemoteConfig::mem(&host_id)
            .with_ordered_relays([hub_id.clone()])
            .with_knobs(RuntimeKnobs {
                reorder_gap_timeout_ms: 20_000,
                ..Default::default()
            }),
        routes.clone(),
    )
    .await;
    let source = start(
        RemoteConfig::mem(format!("warm-source-{label}")),
        Arc::new(Empty),
    )
    .await;
    let hub = start(
        RemoteConfig::mem(hub_id).with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    if routed {
        source.connect_mem_pair(&hub).await.unwrap();
        host.connect_mem_pair(&hub).await.unwrap();
    } else {
        source.connect_mem_pair(&host).await.unwrap();
    }
    host.connect_mem_pair(&end).await.unwrap();
    let alias = source
        .remote_ref(&format!("parrot://{host_id}/alias"))
        .unwrap();
    let raw = source
        .remote_ref(&format!("parrot://{host_id}/local"))
        .unwrap();
    strict(if change_same_path { &alias } else { &raw }, 1).await;
    seen(&mut local_rx, 1).await;
    routes.forward.store(true, Ordering::Release);
    assert!(
        alias
            .deliver_ordered(Box::new(Work(2)), Default::default())
            .await
            .is_err(),
        "the local path's ticket certified an alias whose endpoint did not grant the session"
    );
    assert!(
        alias.deliver(Box::new(Work(3))).await.is_err(),
        "active ordered lane downgraded"
    );
    strict(&raw, 4).await;
    seen(&mut local_rx, 4).await;
    raw.close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    alias.deliver(Box::new(Work(5))).await.unwrap();
    seen(&mut actual_rx, 5).await;
    assert!(local_rx.try_recv().is_err() && actual_rx.try_recv().is_err());
    for node in [&source, &host, &hub, &end] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ordinary_tell_preserves_application_alias_forwarding_and_strict_tell_rejects_before_data()
{
    let (state, mut receiver) = state();
    let target = start(RemoteConfig::mem("alias-target"), Arc::new(Target(state))).await;
    let alias = start(RemoteConfig::mem("alias-host"), Arc::new(Forwarding)).await;
    let source = start(RemoteConfig::mem("alias-source"), Arc::new(Empty)).await;
    source.connect_mem_pair(&alias).await.unwrap();
    alias.connect_mem_pair(&target).await.unwrap();
    let reference = source.remote_ref("parrot://alias-host/target").unwrap();
    assert!(
        reference
            .deliver_ordered(Box::new(Work(99)), Default::default())
            .await
            .is_err(),
        "strict session falsely confirmed an alias instead of the final business endpoint"
    );
    reference.deliver(Box::new(Work(1))).await.unwrap();
    seen(&mut receiver, 1).await;
    assert!(receiver.try_recv().is_err());
    for node in [&source, &alias, &target] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ordinary_delivery_cannot_downgrade_a_lane_after_ordered_publication() {
    let (state, mut receiver) = state();
    let target = start(
        RemoteConfig::tcp("downgrade-target", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Target(state.clone())),
    )
    .await;
    let source = start(
        RemoteConfig::tcp("downgrade-source", None).with_insecure_development(),
        Arc::new(Empty),
    )
    .await;
    source
        .connect(&NodeAddr::tcp(
            "downgrade-target",
            target.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let reference = source
        .remote_ref("parrot://downgrade-target/target")
        .unwrap();
    reference.deliver(Box::new(Work(1))).await.unwrap();
    seen(&mut receiver, 1).await;
    target.shutdown().await.unwrap();
    drop(target);
    let target = start(
        RemoteConfig::tcp("downgrade-target", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Target(state)),
    )
    .await;
    source
        .connect(&NodeAddr::tcp(
            "downgrade-target",
            target.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    assert!(reference.deliver(Box::new(Work(99))).await.is_err());
    reference
        .send_with_timeout(Box::new(Work(2)), Some(Duration::from_secs(3)))
        .await
        .unwrap();
    assert!(receiver.try_recv().is_err());
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

struct Raw {
    handle: ConnectionHandle,
    incoming: tokio::sync::mpsc::Receiver<(Frame, parrot_remote::transport::FrameSender, String)>,
    shutdown: tokio::sync::watch::Sender<bool>,
    cid: u64,
}
impl Raw {
    async fn connect(target: &RemoteActorSystem) -> Self {
        Self::connect_with_caps(target, caps::BIN | caps::ORDERED_TELL).await
    }
    async fn connect_with_caps(target: &RemoteActorSystem, capabilities: u32) -> Self {
        let io = tokio::net::TcpStream::connect(target.local_addr().unwrap())
            .await
            .unwrap();
        let (inbound, incoming) = tokio::sync::mpsc::channel(64);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let handle = run_connection(
            io,
            ConnParams {
                side: ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "tcp",
                local_handshake: HandshakeBody {
                    node_id: "wire-source".into(),
                    capabilities,
                    ..Default::default()
                },
                inbound,
                on_disconnect: Arc::new(|_| {}),
                shutdown: shutdown.subscribe(),
                knobs: Some(RuntimeKnobs {
                    heartbeat_enabled: false,
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap();
        Self {
            handle,
            incoming,
            shutdown,
            cid: 0,
        }
    }
    async fn control(
        &mut self,
        operation: u8,
        incarnation: u8,
        stream: u8,
        ticket: [u8; 16],
        source: &str,
    ) -> Frame {
        let mut payload = vec![2, operation];
        payload.extend_from_slice(&[incarnation; 16]);
        payload.extend_from_slice(&[stream; 16]);
        payload.extend_from_slice(&ticket);
        payload.extend_from_slice(&(source.len() as u16).to_le_bytes());
        payload.extend_from_slice(source.as_bytes());
        self.cid += 1;
        self.handle
            .sender
            .send(Frame::ask(
                self.cid,
                "parrot://wire-target/target",
                CONTROL_KEY,
                payload.into(),
                None,
            ))
            .await
            .unwrap();
        let (reply, _, _) = tokio::time::timeout(Duration::from_secs(3), self.incoming.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.header.correlation_id, self.cid);
        reply
    }
    async fn data(&self, incarnation: u8, stream: u8, ticket: [u8; 16], sequence: u64, value: u64) {
        let source = "wire-source";
        let key = Work::TYPE_KEY;
        let mut payload = vec![2];
        payload.extend_from_slice(&[incarnation; 16]);
        payload.extend_from_slice(&[stream; 16]);
        payload.extend_from_slice(&ticket);
        payload.extend_from_slice(&sequence.to_le_bytes());
        payload.extend_from_slice(&(source.len() as u16).to_le_bytes());
        payload.extend_from_slice(&(key.len() as u16).to_le_bytes());
        payload.extend_from_slice(source.as_bytes());
        payload.extend_from_slice(key.as_bytes());
        payload
            .extend_from_slice(&parrot_api::message::serde_remote_serialize(&Work(value)).unwrap());
        self.handle
            .sender
            .send(Frame::tell(
                "parrot://wire-target/target",
                DATA_KEY,
                Bytes::from(payload),
            ))
            .await
            .unwrap();
    }
}
fn ticket(reply: &Frame, active: bool) -> [u8; 16] {
    assert_eq!(reply.header.frame_type, frame_type::REPLY);
    assert_eq!(reply.type_key, RECEIPT_KEY);
    assert_eq!(reply.payload.len(), 46);
    assert_eq!(&reply.payload[..2], &[2, u8::from(active)]);
    assert_ne!(&reply.payload[30..46], &[0; 16]);
    reply.payload[2..18].try_into().unwrap()
}

#[tokio::test]
async fn negotiated_link_rejects_malformed_ordered_payload_without_business_delivery() {
    let (state, mut receiver) = state();
    let target = start(
        RemoteConfig::tcp("bad-ordered-target", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Target(state)),
    )
    .await;
    let raw = Raw::connect(&target).await;
    raw.handle
        .sender
        .send(Frame::tell(
            "parrot://bad-ordered-target/target",
            DATA_KEY,
            Bytes::from_static(&[0]),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), raw.handle.sender.closed())
        .await
        .unwrap();
    assert!(receiver.try_recv().is_err());
    raw.shutdown.send_replace(true);
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn nonnegotiated_ordered_frames_fail_before_sender_and_receiver_publication() {
    let (state, mut receiver) = state();
    let target = start(
        RemoteConfig::tcp("unnegotiated-target", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Target(state)),
    )
    .await;
    let raw = Raw::connect_with_caps(&target, caps::BIN).await;
    assert!(!raw.handle.sender.supports_ordered_tell());
    let data = Frame::tell(
        "parrot://unnegotiated-target/target",
        DATA_KEY,
        Bytes::new(),
    );
    assert!(raw.handle.sender.send(data.clone()).await.is_err());
    assert!(!raw.handle.sender.is_closed());
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let unnegotiated = parrot_remote::transport::FrameSender::new(tx, "wire-source");
    target
        .ingress
        .dispatch(data, &unnegotiated, "wire-source")
        .await;
    assert!(unnegotiated.is_closed());
    assert!(receiver.try_recv().is_err());
    raw.shutdown.send_replace(true);
    target.shutdown().await.unwrap();
}
#[tokio::test]
async fn actual_wire_old_open_commit_close_and_data_cannot_retire_or_overwrite_the_new_ticket() {
    let (state, mut receiver) = state();
    let target = start(
        RemoteConfig::tcp("wire-target", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Target(state)),
    )
    .await;
    let mut raw = Raw::connect(&target).await;
    let forged = raw.control(0, 1, 2, [0; 16], "forged-origin").await;
    assert_eq!(forged.header.frame_type, frame_type::REPLY_ERR);
    assert_eq!(
        parrot_remote::error::decode_err_payload(&forged.payload)
            .unwrap()
            .0,
        parrot_remote::error::ErrCode::Forbidden
    );
    let old = ticket(&raw.control(0, 1, 2, [0; 16], "wire-source").await, false);
    assert_eq!(
        ticket(&raw.control(1, 1, 2, old, "wire-source").await, true),
        old
    );
    raw.data(1, 2, old, 2, 2).await;
    raw.data(1, 2, old, 2, 99).await;
    raw.data(1, 2, old, 1, 1).await;
    seen(&mut receiver, 1).await;
    seen(&mut receiver, 2).await;
    let new = ticket(&raw.control(0, 3, 4, [0; 16], "wire-source").await, false);
    assert_ne!(new, old);
    raw.data(1, 2, old, 3, 3).await;
    seen(&mut receiver, 3).await;
    assert_eq!(
        ticket(&raw.control(1, 3, 4, new, "wire-source").await, true),
        new
    );
    raw.data(1, 2, old, 4, 99).await;
    assert_eq!(
        raw.control(1, 1, 2, old, "wire-source")
            .await
            .header
            .frame_type,
        frame_type::REPLY_ERR
    );
    assert_eq!(
        ticket(&raw.control(2, 1, 2, old, "wire-source").await, false),
        old
    );
    raw.data(3, 4, new, 1, 5).await;
    seen(&mut receiver, 5).await;
    let stale_open = raw.control(0, 1, 2, [0; 16], "wire-source").await;
    let _ = ticket(&stale_open, false);
    raw.data(3, 4, new, 2, 6).await;
    seen(&mut receiver, 6).await;
    assert_eq!(
        ticket(&raw.control(2, 3, 4, new, "wire-source").await, false),
        new
    );
    raw.data(3, 4, new, 3, 99).await;
    let fresh = ticket(&raw.control(0, 5, 6, [0; 16], "wire-source").await, false);
    assert_ne!(fresh, new);
    let _ = ticket(&raw.control(1, 5, 6, fresh, "wire-source").await, true);
    raw.data(5, 6, fresh, 1, 7).await;
    seen(&mut receiver, 7).await;
    assert!(receiver.try_recv().is_err());
    raw.shutdown.send_replace(true);
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn closing_an_uncommitted_challenge_releases_receiver_source_capacity() {
    let (state, mut receiver) = state();
    let target = start(
        RemoteConfig::tcp("wire-target", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development()
            .with_knobs(RuntimeKnobs {
                reorder_stream_capacity: 1,
                ..Default::default()
            }),
        Arc::new(Target(state)),
    )
    .await;
    let mut raw = Raw::connect(&target).await;
    let pending = ticket(&raw.control(0, 1, 2, [0; 16], "wire-source").await, false);
    let source = start(
        RemoteConfig::tcp("challenge-next", None).with_insecure_development(),
        Arc::new(Empty),
    )
    .await;
    source
        .connect(&NodeAddr::tcp("wire-target", target.local_addr().unwrap()))
        .await
        .unwrap();
    let reference = source.remote_ref("parrot://wire-target/target").unwrap();
    let error = reference
        .deliver_ordered(Box::new(Work(99)), Default::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("remote overloaded"), "{error}");
    assert_eq!(
        ticket(&raw.control(2, 1, 2, pending, "wire-source").await, false),
        pending
    );
    strict(&reference, 1).await;
    seen(&mut receiver, 1).await;
    assert!(receiver.try_recv().is_err());
    raw.shutdown.send_replace(true);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}
