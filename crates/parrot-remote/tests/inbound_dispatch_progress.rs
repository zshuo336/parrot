//! A blocked application operation must not stall unrelated connections/replies.
use parrot_api::{
    address::ActorRef,
    message::RemoteMessage,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    admin::{encode_admin_cmd, AdminCommand},
    frame::{frame_type, Frame},
    LocalLookup, RemoteActorSystem, RemoteConfig,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Value(u64);
impl parrot_api::Message for Value {
    type Result = Self;
}
#[derive(Default, Debug)]
struct Gate {
    entered: tokio::sync::Notify,
    active: AtomicUsize,
    lookup_panics: AtomicUsize,
    release: tokio::sync::Notify,
    values: std::sync::Mutex<Vec<u64>>,
    delivered: tokio::sync::Notify,
}
struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[derive(Clone, Debug)]
struct Echo(Arc<Gate>);
#[async_trait::async_trait]
impl LocalLookup for Echo {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        if path == "/panic" {
            self.0.lookup_panics.fetch_add(1, Ordering::SeqCst);
            panic!("controlled application lookup panic");
        }
        if path == "/blocked" {
            self.0.active.fetch_add(1, Ordering::SeqCst);
            let _active = Active(&self.0.active);
            self.0.entered.notify_one();
            self.0.release.notified().await;
        }
        Some(Box::new(self.clone()))
    }
}
#[async_trait::async_trait]
impl ActorRef for Echo {
    fn send<'a>(&'a self, m: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(m) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        m: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(m)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let value = message.downcast_ref::<Value>().unwrap().0;
            self.0.values.lock().unwrap().push(value);
            self.0.delivered.notify_one();
            Ok(())
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/echo".into()
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
async fn node(name: &str, gate: Arc<Gate>) -> Arc<RemoteActorSystem> {
    let mut config = RemoteConfig::mem(name);
    config.heartbeat_enabled = false;
    let sys = RemoteActorSystem::new(config, Arc::new(Echo(gate))).unwrap();
    sys.start().await.unwrap();
    sys
}
async fn limited_node(
    name: &str,
    gate: Arc<Gate>,
    capacity: usize,
    per_connection: usize,
    bytes: usize,
) -> Arc<RemoteActorSystem> {
    // Exercise the actual TOML -> resolved config -> system wiring, not just
    // fields on a separately constructed dispatcher.
    let path = std::env::temp_dir().join(format!("{name}-{}.toml", std::process::id()));
    std::fs::write(&path, format!(
        "[remote.node]\nnode_id = '{name}'\nscheme = 'mem'\n[remote.transport]\ninbound_dispatch_capacity = {capacity}\ninbound_dispatch_per_connection = {per_connection}\ninbound_dispatch_bytes = {bytes}\n"
    )).unwrap();
    let resolved = parrot_config::ParrotConfig::builder()
        .load_file(&path)
        .unwrap()
        .build()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    let mut config = RemoteConfig::from_resolved(&resolved).unwrap();
    config.heartbeat_enabled = false;
    let sys = RemoteActorSystem::new(config, Arc::new(Echo(gate))).unwrap();
    sys.start().await.unwrap();
    sys
}
async fn inactive(gate: &Gate) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while gate.active.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dispatch future was not dropped");
}
fn blocking_frame(kind: u8) -> Frame {
    match kind {
        frame_type::TELL => Frame::tell(
            "/blocked",
            Value::TYPE_KEY,
            parrot_api::message::serde_remote_serialize(&Value(7))
                .unwrap()
                .into(),
        ),
        frame_type::STOP => Frame::stop("/blocked"),
        frame_type::SYSTEM_EVENT => {
            let mut frame = Frame::stop("");
            frame.header.frame_type = frame_type::SYSTEM_EVENT;
            frame.header.correlation_id = 71;
            frame.payload = encode_admin_cmd(&AdminCommand::AdminStop {
                req_id: 71,
                target_path: "/blocked".into(),
                reply_to: String::new(),
            });
            frame
        }
        _ => unreachable!(),
    }
}
async fn echo(sys: &Arc<RemoteActorSystem>, target: &str) {
    let reference = sys.remote_ref(&format!("parrot://{target}/echo")).unwrap();
    let reply = reference.send(Box::new(Value(42))).await.unwrap();
    assert_eq!(reply.downcast_ref::<Value>().unwrap().0, 42);
}
async fn values(gate: &Gate, n: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while gate.values.lock().unwrap().len() < n {
            gate.delivered.notified().await;
        }
    })
    .await
    .expect("expected application deliveries did not arrive");
}
fn tell(value: u64, path: &str) -> Frame {
    Frame::tell(
        path,
        Value::TYPE_KEY,
        parrot_api::message::serde_remote_serialize(&Value(value))
            .unwrap()
            .into(),
    )
}
async fn progress(kind: u8) {
    let gate = Arc::new(Gate::default());
    let server_id = format!("dispatch-server-{kind}");
    let client_id = format!("dispatch-client-{kind}");
    let server = node(&server_id, gate.clone()).await;
    let client = node(&client_id, Arc::default()).await;
    let other = node(&format!("dispatch-other-{kind}"), Arc::default()).await;
    client.connect_mem_pair(&server).await.unwrap();
    other.connect_mem_pair(&server).await.unwrap();
    client.links_snapshot().await[0]
        .1
        .send(blocking_frame(kind))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .expect("the slow frame must actually reach lookup");
    let result = tokio::time::timeout(Duration::from_secs(1), async {
        // Same-connection requests, other connections, and the server's own
        // inbound replies all have to progress before the blocked work ends.
        let (_, _, _, admin) = tokio::join!(
            echo(&client, &server_id),
            echo(&other, &server_id),
            echo(&server, &client_id),
            server.admin_stop(&client_id, "/echo"),
        );
        admin.expect("management replies must bypass slow inbound work too");
    })
    .await;
    server.shutdown().await.unwrap();
    client.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
    assert_eq!(gate.active.load(Ordering::SeqCst), 0);
    assert!(result.is_ok(), "slow frame {kind} blocked global dispatch");
}
#[tokio::test]
async fn blocked_unsequenced_tell_does_not_stall_requests_or_replies() {
    progress(frame_type::TELL).await;
}
#[tokio::test]
async fn blocked_stop_does_not_stall_requests_or_replies() {
    progress(frame_type::STOP).await;
}
#[tokio::test]
async fn blocked_admin_does_not_stall_requests_or_replies() {
    progress(frame_type::SYSTEM_EVENT).await;
}

#[tokio::test]
async fn slow_lanes_preserve_unsequenced_tell_order() {
    let gate = Arc::new(Gate::default());
    let server = node("dispatch-order-server", gate.clone()).await;
    let client = node("dispatch-order-client", Arc::default()).await;
    client.connect_mem_pair(&server).await.unwrap();
    let sender = client.links_snapshot().await[0].1.clone();
    sender.send(tell(0, "/blocked")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    for value in 1..16 {
        sender.send(tell(value, "/echo")).await.unwrap();
    }
    tokio::time::timeout(
        Duration::from_secs(1),
        echo(&client, "dispatch-order-server"),
    )
    .await
    .unwrap();
    assert!(
        gate.values.lock().unwrap().is_empty(),
        "later TELL overtook blocked TELL"
    );
    gate.release.notify_one();
    values(&gate, 16).await;
    server.shutdown().await.unwrap();
    client.shutdown().await.unwrap();
    assert_eq!(*gate.values.lock().unwrap(), (0..16).collect::<Vec<_>>());
}

#[tokio::test]
async fn physical_connections_with_the_same_peer_name_have_independent_lanes() {
    let gate = Arc::new(Gate::default());
    let server = node("dispatch-identity-server", gate.clone()).await;
    let client = node("dispatch-identity-client", Arc::default()).await;
    client.connect_mem_pair(&server).await.unwrap();
    client.connect_mem_pair(&server).await.unwrap();
    let senders = client.links_snapshot().await;
    assert_eq!(senders.len(), 2);
    senders[0].1.send(tell(0, "/blocked")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    senders[1].1.send(tell(99, "/echo")).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), gate.delivered.notified()).await;
    server.shutdown().await.unwrap();
    client.shutdown().await.unwrap();
    assert!(
        result.is_ok(),
        "same advertised node identity merged independent connections"
    );
    assert_eq!(*gate.values.lock().unwrap(), vec![99]);
    assert_eq!(gate.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn disconnect_drops_active_and_queued_work_before_system_shutdown() {
    let gate = Arc::new(Gate::default());
    let server = node("dispatch-disconnect-server", gate.clone()).await;
    let client = node("dispatch-disconnect-client", Arc::default()).await;
    client.connect_mem_pair(&server).await.unwrap();
    let sender = client.links_snapshot().await[0].1.clone();
    sender.send(tell(0, "/blocked")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    sender.send(tell(1, "/echo")).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        echo(&client, "dispatch-disconnect-server"),
    )
    .await
    .unwrap();
    sender.close();
    let result = tokio::time::timeout(Duration::from_secs(1), async {
        while gate.active.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    server.shutdown().await.unwrap();
    client.shutdown().await.unwrap();
    assert!(
        result.is_ok(),
        "disconnected lookup future survived until system shutdown"
    );
    assert!(gate.values.lock().unwrap().is_empty());
}

#[tokio::test]
async fn contained_application_panic_preserves_its_connection_and_dispatcher() {
    let gate = Arc::new(Gate::default());
    let server = node("dispatch-panic-server", gate.clone()).await;
    let affected = node("dispatch-panic-affected", Arc::default()).await;
    let healthy = node("dispatch-panic-healthy", Arc::default()).await;
    affected.connect_mem_pair(&server).await.unwrap();
    healthy.connect_mem_pair(&server).await.unwrap();
    let sender = affected.links_snapshot().await[0].1.clone();
    sender.send(tell(0, "/panic")).await.unwrap();
    sender.send(tell(7, "/echo")).await.unwrap();
    // Same-lane FIFO proves the faulty lookup ran before the healthy delivery.
    // A closed/reconnected lane cannot satisfy this receipt on the original sender.
    let progress = tokio::time::timeout(Duration::from_secs(1), async {
        values(&gate, 1).await;
        tokio::join!(
            echo(&affected, "dispatch-panic-server"),
            echo(&healthy, "dispatch-panic-server")
        );
    })
    .await;
    let same_connection = affected
        .links_snapshot()
        .await
        .iter()
        .any(|(_, current, _)| current.same_connection(&sender));
    let open = !sender.is_closed();
    server.shutdown().await.unwrap();
    affected.shutdown().await.unwrap();
    healthy.shutdown().await.unwrap();
    assert!(
        progress.is_ok(),
        "contained panic stalled subsequent messages"
    );
    assert!(
        open && same_connection,
        "must preserve the original healthy transport"
    );
    assert_eq!(gate.lookup_panics.load(Ordering::SeqCst), 1);
    assert_eq!(
        *gate.values.lock().unwrap(),
        vec![7],
        "failed message must not be delivered"
    );
}

#[tokio::test]
async fn per_connection_limit_counts_active_and_queued_frames_and_isolates_overload() {
    let gate = Arc::new(Gate::default());
    let server = limited_node("dispatch-per-lane-server", gate.clone(), 8, 2, 65536).await;
    let client = node("dispatch-per-lane-client", Arc::default()).await;
    let other = node("dispatch-per-lane-other", Arc::default()).await;
    client.connect_mem_pair(&server).await.unwrap();
    other.connect_mem_pair(&server).await.unwrap();
    let sender = client.links_snapshot().await[0].1.clone();
    sender.send(tell(0, "/blocked")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    sender.send(tell(1, "/echo")).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        echo(&client, "dispatch-per-lane-server"),
    )
    .await
    .unwrap();
    assert!(!sender.is_closed(), "exact lane capacity must be accepted");
    sender.send(tell(2, "/echo")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), sender.closed())
        .await
        .expect("third frame must exceed capacity 2");
    inactive(&gate).await;
    other.links_snapshot().await[0]
        .1
        .send(tell(99, "/echo"))
        .await
        .unwrap();
    values(&gate, 1).await;
    server.shutdown().await.unwrap();
    client.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
    assert_eq!(
        *gate.values.lock().unwrap(),
        vec![99],
        "overloaded queue must be cancelled, not delivered"
    );
}

async fn shared_budget(bytes_limited: bool) {
    let kind = if bytes_limited { "bytes" } else { "count" };
    let server_id = format!("dispatch-budget-{kind}-server");
    let first_id = format!("dispatch-budget-{kind}-first");
    let frame = tell(0, "/blocked");
    // Exactly one encoded frame and its copied source identity fit. The other
    // test instead allows plenty of bytes but only one operation globally.
    let byte_limit = if bytes_limited {
        frame.path.len() + frame.type_key.len() + frame.payload.len() + first_id.len()
    } else {
        65536
    };
    let gate = Arc::new(Gate::default());
    let server = limited_node(
        &server_id,
        gate.clone(),
        if bytes_limited { 8 } else { 1 },
        8,
        byte_limit,
    )
    .await;
    let first = node(&first_id, Arc::default()).await;
    let second = node(&format!("dispatch-budget-{kind}-second"), Arc::default()).await;
    first.connect_mem_pair(&server).await.unwrap();
    second.connect_mem_pair(&server).await.unwrap();
    let first_sender = first.links_snapshot().await[0].1.clone();
    let second_sender = second.links_snapshot().await[0].1.clone();
    first_sender.send(frame).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    second_sender.send(Frame::stop("/echo")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), second_sender.closed())
        .await
        .expect("shared budget was exceeded");
    assert!(
        !first_sender.is_closed(),
        "capacity rejection closed an unrelated connection"
    );
    // Replies and ASK retain a separate budget and still work at this boundary.
    tokio::time::timeout(Duration::from_secs(1), echo(&first, &server_id))
        .await
        .unwrap();
    gate.release.notify_one();
    values(&gate, 1).await;
    // Receiving the application notification precedes dispatcher completion;
    // the next ASK round trip makes the completion/budget release observable.
    tokio::time::timeout(Duration::from_secs(1), echo(&first, &server_id))
        .await
        .unwrap();
    first_sender.send(tell(42, "/echo")).await.unwrap();
    values(&gate, 2).await;
    server.shutdown().await.unwrap();
    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
    assert_eq!(*gate.values.lock().unwrap(), vec![0, 42]);
}
#[tokio::test]
async fn global_dispatch_count_is_shared_and_released_after_completion() {
    shared_budget(false).await;
}
#[tokio::test]
async fn global_dispatch_bytes_are_shared_and_released_after_completion() {
    shared_budget(true).await;
}

#[tokio::test]
async fn saturated_sequenced_tell_worker_does_not_stall_dispatch() {
    let gate = Arc::new(Gate::default());
    let server = limited_node(
        "dispatch-sequence-server",
        gate.clone(),
        1024,
        512,
        1024 * 1024,
    )
    .await;
    let client = node("dispatch-sequence-client", Arc::default()).await;
    client.connect_mem_pair(&server).await.unwrap();
    let sender = client.links_snapshot().await[0].1.clone();
    let mut first = tell(1, "/blocked");
    first.header.seq = 1;
    sender.send(first).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    // More than the existing per-source reorder queue's 256 slots.
    for seq in 2..=300 {
        let mut frame = tell(u64::from(seq), "/echo");
        frame.header.seq = seq;
        sender.send(frame).await.unwrap();
    }
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        echo(&client, "dispatch-sequence-server"),
    )
    .await;
    server.shutdown().await.unwrap();
    client.shutdown().await.unwrap();
    assert!(
        result.is_ok(),
        "waiting for reorder queue capacity blocked global dispatch"
    );
    assert_eq!(gate.active.load(Ordering::SeqCst), 0);
    assert!(gate.values.lock().unwrap().is_empty());
}

#[tokio::test]
async fn system_drop_cancels_lanes_even_if_ingress_is_retained() {
    let gate = Arc::new(Gate::default());
    let server = node("dispatch-drop-server", gate.clone()).await;
    let client = node("dispatch-drop-client", Arc::default()).await;
    client.connect_mem_pair(&server).await.unwrap();
    client.links_snapshot().await[0]
        .1
        .send(tell(0, "/blocked"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    let retained = server.ingress.clone();
    let weak = Arc::downgrade(&server);
    drop(server);
    inactive(&gate).await;
    assert!(
        weak.upgrade().is_none(),
        "dispatcher retained its owning system"
    );
    assert!(gate.values.lock().unwrap().is_empty());
    client.shutdown().await.unwrap();
    drop(retained);
}

#[test]
fn direct_construction_rejects_invalid_dispatch_limits() {
    use parrot_remote::transport::RuntimeKnobs;
    for value in [0, usize::MAX] {
        for knobs in [
            RuntimeKnobs {
                inbound_dispatch_capacity: value,
                ..Default::default()
            },
            RuntimeKnobs {
                inbound_dispatch_per_connection: value,
                ..Default::default()
            },
            RuntimeKnobs {
                inbound_dispatch_bytes: value,
                ..Default::default()
            },
        ] {
            let result = RemoteActorSystem::new(
                RemoteConfig::mem("dispatch-invalid").with_knobs(knobs),
                Arc::new(Echo(Arc::default())),
            );
            assert!(
                result.is_err(),
                "invalid dispatch limit reached transport construction"
            );
        }
    }
}
