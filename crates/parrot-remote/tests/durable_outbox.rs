use parrot_api::{
    address::ActorRef,
    message::RemoteMessage,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    durable::{DurableOutbox, OutboxConfig, OutboxError, Wal},
    ingress::LocalLookup,
    NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    any::Any,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Work(u64);
impl parrot_api::Message for Work {
    type Result = ();
}
fn payload(n: u64) -> Vec<u8> {
    parrot_api::message::serde_remote_serialize(&Work(n)).unwrap()
}
fn unique() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "outbox-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(unique());
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Default, Debug)]
struct State {
    calls: Mutex<Vec<u64>>,
    effects: Mutex<Vec<u64>>,
    fail_once: AtomicBool,
    lose_receipt_once: AtomicBool,
    hold: AtomicBool,
}
#[derive(Clone, Debug)]
struct Target(Arc<State>);
#[async_trait::async_trait]
impl ActorRef for Target {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let n = msg.downcast_ref::<Work>().unwrap().0;
            self.0.calls.lock().unwrap().push(n);
            if self.0.fail_once.swap(false, Ordering::SeqCst) {
                return Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "injected failure".into(),
                ));
            }
            if self.0.hold.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            self.0.effects.lock().unwrap().push(n);
            if self.0.lose_receipt_once.swap(false, Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        m: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(m)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { panic!("outbox must await completion") })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/sink".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[async_trait::async_trait]
impl LocalLookup for Target {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/sink").then(|| self.clone_boxed())
    }
}
async fn node(
    id: &str,
    tcp: Option<std::net::SocketAddr>,
    state: Arc<State>,
) -> Arc<RemoteActorSystem> {
    let config = match tcp {
        Some(addr) => RemoteConfig::tcp(id, Some(addr)),
        None => RemoteConfig::mem(id),
    }
    .with_insecure_development();
    let node = RemoteActorSystem::new(config, Arc::new(Target(state))).unwrap();
    node.start().await.unwrap();
    node
}
fn config() -> OutboxConfig {
    OutboxConfig {
        attempt_timeout: Duration::from_millis(500),
        retry_min: Duration::from_millis(20),
        retry_max: Duration::from_millis(80),
        ..Default::default()
    }
}
async fn acknowledged(h: &parrot_remote::durable::OutboxHandle, seq: u64) {
    tokio::time::timeout(Duration::from_secs(10), h.wait_acked(seq))
        .await
        .unwrap()
        .unwrap();
}
async fn wait_calls(state: &State, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.calls.lock().unwrap().len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn offline_acceptance_is_durable_and_reopen_automatically_replays_original_bytes() {
    let scratch = Scratch::new();
    let source_id = unique();
    let target_id = unique();
    let source = node(&source_id, None, Arc::default()).await;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    let handle = outbox.handle();
    for n in 1..=3 {
        assert_eq!(
            handle.enqueue(Work::TYPE_KEY, &payload(n)).await.unwrap(),
            n
        );
    }
    assert_eq!(handle.status().last_acked, 0);
    assert!(
        Wal::open(&scratch.0).is_err(),
        "worker owns exclusive WAL lock"
    );
    outbox.close().await.unwrap();
    assert!(matches!(
        handle.enqueue(Work::TYPE_KEY, &payload(4)).await,
        Err(OutboxError::Closed)
    ));
    {
        let wal = Wal::open(&scratch.0).unwrap();
        let records = wal.replay(&target_id).unwrap();
        assert_eq!(records.len(), 3);
        for (i, r) in records.iter().enumerate() {
            assert_eq!(r.payload.as_ref(), payload(i as u64 + 1));
        }
    }
    source.shutdown().await.unwrap();
    let source = node(&source_id, None, Arc::default()).await;
    let state = Arc::new(State::default());
    let target = node(&target_id, None, state.clone()).await;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    acknowledged(&outbox.handle(), 3).await;
    assert_eq!(*state.effects.lock().unwrap(), vec![1, 2, 3]);
    assert_eq!(
        outbox
            .handle()
            .enqueue(Work::TYPE_KEY, &payload(4))
            .await
            .unwrap(),
        4
    );
    acknowledged(&outbox.handle(), 4).await;
    outbox.close().await.unwrap();
    {
        let wal = Wal::open(&scratch.0).unwrap();
        assert_eq!(wal.pending_len().unwrap(), 0);
    }
    // Even an empty pending set retains the sequence watermark.
    let mut reopened = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    assert_eq!(
        reopened
            .handle()
            .enqueue(Work::TYPE_KEY, &payload(5))
            .await
            .unwrap(),
        5
    );
    acknowledged(&reopened.handle(), 5).await;
    reopened.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_persisted_outbox_requests_are_rejected_without_discarding_history() {
    let source = node(&unique(), None, Arc::default()).await;
    for transactional in [false, true] {
        let scratch = Scratch::new();
        let cfg = OutboxConfig {
            transactional,
            ..config()
        };
        let mut outbox = DurableOutbox::open(
            &scratch.0,
            &source,
            NodeAddr::mem("receiver"),
            "/sink",
            "session",
            cfg.clone(),
        )
        .await
        .unwrap();
        outbox.close().await.unwrap();
        {
            let wal = Wal::open(&scratch.0).unwrap();
            wal.append(parrot_remote::durable::WalRecord {
                endpoint: "receiver".into(),
                sender: "session".into(),
                seq: 1,
                path: "/sink".into(),
                type_key: parrot_remote::durable::DURABLE_KEY.into(),
                payload: payload(7).into(),
            })
            .unwrap();
            wal.sync().unwrap();
        }
        let result = DurableOutbox::open(
            &scratch.0,
            &source,
            NodeAddr::mem("receiver"),
            "/sink",
            "session",
            cfg,
        )
        .await;
        assert!(
            matches!(result, Err(OutboxError::Storage(_))),
            "unreplayable protocol records must fail recovery, not start an endless retry loop"
        );
        let wal = Wal::open(&scratch.0).unwrap();
        let records = wal.replay("receiver").unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].type_key, parrot_remote::durable::DURABLE_KEY);
        assert_eq!(records[0].payload.as_ref(), payload(7));
    }
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_outbox_bindings_are_rejected_before_creating_directories() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    for transactional in [false, true] {
        for (receiver, session) in [("bad receiver", "session"), ("receiver", "bad\0session")] {
            let directory = scratch
                .0
                .join(format!("{transactional}-{}", receiver.len()));
            let result = DurableOutbox::open(
                &directory,
                &source,
                NodeAddr::mem(receiver),
                "/sink",
                session,
                OutboxConfig {
                    transactional,
                    ..config()
                },
            )
            .await;
            assert!(
                matches!(result, Err(OutboxError::Invalid(_))),
                "invalid identity must fail before creating a permanently unusable binding"
            );
            assert!(!directory.exists());
        }
    }
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn outbox_rejects_mismatched_transport_before_creating_storage() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    for (index, peer) in [
        NodeAddr::tcp("peer", "127.0.0.1:12345".parse().unwrap()),
        NodeAddr::quic("peer", "127.0.0.1:12345".parse().unwrap()),
    ]
    .into_iter()
    .enumerate()
    {
        let directory = scratch.0.join(index.to_string());
        let result =
            DurableOutbox::open(&directory, &source, peer, "/sink", "session", config()).await;
        assert!(
            matches!(result, Err(OutboxError::Invalid(_))),
            "an incompatible transport must not create an unusable durable stream"
        );
        assert!(!directory.exists());
    }
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn outbox_rejects_zero_network_port_before_creating_storage() {
    let scratch = Scratch::new();
    let source = node(
        &unique(),
        Some("127.0.0.1:0".parse().unwrap()),
        Arc::default(),
    )
    .await;
    let directory = scratch.0.join("outbox");
    let result = DurableOutbox::open(
        &directory,
        &source,
        NodeAddr::tcp("peer", "127.0.0.1:0".parse().unwrap()),
        "/sink",
        "session",
        config(),
    )
    .await;
    assert!(
        matches!(result, Err(OutboxError::Invalid(_))),
        "port zero is a listener request, never a durable destination"
    );
    assert!(!directory.exists());
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn outbox_rejects_locally_forbidden_address_without_network_or_storage_work() {
    let scratch = Scratch::new();
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp(unique(), None),
        Arc::new(Target(Arc::default())),
    )
    .unwrap();
    let directory = scratch.0.join("outbox");
    let result = DurableOutbox::open(
        &directory,
        &source,
        NodeAddr::tcp("peer", "192.0.2.1:12345".parse().unwrap()),
        "/sink",
        "session",
        config(),
    )
    .await;
    assert!(
        matches!(result, Err(OutboxError::Invalid(_))),
        "default Local security cannot dial a non-loopback destination"
    );
    assert!(!directory.exists());
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_reopen_destination_preserves_history_and_corrected_address_replays_it() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let target_id = unique();
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    assert_eq!(
        outbox
            .handle()
            .enqueue(Work::TYPE_KEY, &payload(19))
            .await
            .unwrap(),
        1
    );
    outbox.close().await.unwrap();
    let files = || {
        std::fs::read_dir(&scratch.0)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.file_name().unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = files();
    let wrong = NodeAddr::tcp(&target_id, "127.0.0.1:12345".parse().unwrap());
    assert!(matches!(
        DurableOutbox::open(&scratch.0, &source, wrong, "/sink", "session", config()).await,
        Err(OutboxError::Invalid(_))
    ));
    assert_eq!(
        files(),
        before,
        "configuration rejection must not rewrite existing evidence"
    );
    let state = Arc::new(State::default());
    let target = node(&target_id, None, state.clone()).await;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    acknowledged(&outbox.handle(), 1).await;
    assert_eq!(*state.effects.lock().unwrap(), vec![19]);
    assert_eq!(
        outbox
            .handle()
            .enqueue(Work::TYPE_KEY, &payload(23))
            .await
            .unwrap(),
        2
    );
    acknowledged(&outbox.handle(), 2).await;
    assert_eq!(*state.effects.lock().unwrap(), vec![19, 23]);
    outbox.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn reserved_durable_protocol_keys_cannot_poison_an_ordinary_outbox() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let target_id = unique();
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    let handle = outbox.handle();
    for key in [
        parrot_remote::durable::DURABLE_KEY,
        parrot_remote::durable::DURABLE_RECEIPT_KEY,
    ] {
        assert!(
            matches!(
                handle.enqueue(key, &payload(7)).await,
                Err(OutboxError::Invalid(_))
            ),
            "reserved protocol key {key} must be rejected before durable acceptance"
        );
    }
    assert_eq!(handle.status().last_enqueued, 0);
    assert_eq!(handle.status().pending, 0);
    assert_eq!(
        handle.enqueue(Work::TYPE_KEY, &payload(7)).await.unwrap(),
        1
    );
    outbox.close().await.unwrap();
    let state = Arc::new(State::default());
    let target = node(&target_id, None, state.clone()).await;
    let mut reopened = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    acknowledged(&reopened.handle(), 1).await;
    assert_eq!(*state.effects.lock().unwrap(), vec![7]);
    reopened.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn business_failure_retries_head_without_acknowledging_or_overtaking_it() {
    let scratch = Scratch::new();
    let state = Arc::new(State::default());
    state.fail_once.store(true, Ordering::SeqCst);
    let source = node(&unique(), None, Arc::default()).await;
    let target_id = unique();
    let target = node(&target_id, None, state.clone()).await;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(target_id),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    let handle = outbox.handle();
    assert_eq!(
        handle.enqueue(Work::TYPE_KEY, &payload(1)).await.unwrap(),
        1
    );
    assert_eq!(
        handle.enqueue(Work::TYPE_KEY, &payload(2)).await.unwrap(),
        2
    );
    acknowledged(&handle, 2).await;
    assert_eq!(*state.calls.lock().unwrap(), vec![1, 1, 2]);
    assert_eq!(*state.effects.lock().unwrap(), vec![1, 2]);
    assert_eq!(handle.status().attempts, 3);
    outbox.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_receipt_retries_and_does_not_claim_exactly_once_business_effects() {
    let scratch = Scratch::new();
    let state = Arc::new(State::default());
    state.lose_receipt_once.store(true, Ordering::SeqCst);
    let source = node(&unique(), None, Arc::default()).await;
    let target_id = unique();
    let target = node(&target_id, None, state.clone()).await;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(target_id),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    let handle = outbox.handle();
    handle.enqueue(Work::TYPE_KEY, &payload(7)).await.unwrap();
    acknowledged(&handle, 1).await;
    assert_eq!(*state.effects.lock().unwrap(), vec![7, 7]);
    assert_eq!(source.callbacks.len(), 0);
    outbox.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn real_tcp_disconnect_redials_and_replays_without_manual_reconnect() {
    let scratch = Scratch::new();
    let source = node(
        &unique(),
        Some("127.0.0.1:0".parse().unwrap()),
        Arc::default(),
    )
    .await;
    let target_id = unique();
    let state = Arc::new(State::default());
    state.hold.store(true, Ordering::SeqCst);
    let target = node(
        &target_id,
        Some("127.0.0.1:0".parse().unwrap()),
        state.clone(),
    )
    .await;
    let address = target.local_addr().unwrap();
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::tcp(&target_id, address),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    outbox
        .handle()
        .enqueue(Work::TYPE_KEY, &payload(9))
        .await
        .unwrap();
    wait_calls(&state, 1).await;
    target.shutdown().await.unwrap();
    let recovered = Arc::new(State::default());
    let target = node(&target_id, Some(address), recovered.clone()).await;
    acknowledged(&outbox.handle(), 1).await;
    assert!(state.effects.lock().unwrap().is_empty());
    assert_eq!(*recovered.effects.lock().unwrap(), vec![9]);
    outbox.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn close_and_drop_cancel_callbacks_release_storage_and_preserve_pending() {
    for explicit in [true, false] {
        let scratch = Scratch::new();
        let source = node(&unique(), None, Arc::default()).await;
        let target_id = unique();
        let state = Arc::new(State::default());
        state.hold.store(true, Ordering::SeqCst);
        let target = node(&target_id, None, state.clone()).await;
        let mut cfg = config();
        cfg.attempt_timeout = Duration::from_secs(3600);
        let mut outbox = DurableOutbox::open(
            &scratch.0,
            &source,
            NodeAddr::mem(&target_id),
            "/sink",
            "session",
            cfg,
        )
        .await
        .unwrap();
        let h = outbox.handle();
        h.enqueue(Work::TYPE_KEY, &payload(1)).await.unwrap();
        wait_calls(&state, 1).await;
        if explicit {
            tokio::time::timeout(Duration::from_secs(2), outbox.close())
                .await
                .unwrap()
                .unwrap();
        }
        drop(outbox);
        assert!(h.status().closed);
        assert_eq!(source.callbacks.len(), 0);
        assert!(h.wait_acked(1).await.is_err());
        {
            let wal = Wal::open(&scratch.0).unwrap();
            assert_eq!(wal.pending_len().unwrap(), 1);
        }
        source.shutdown().await.unwrap();
        target.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn empty_outbox_binding_cannot_be_reused_for_another_destination_or_session() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let receiver = NodeAddr::mem(unique());
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        receiver.clone(),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    outbox.close().await.unwrap();
    for (peer, path, sender) in [
        (receiver.clone(), "/other", "session"),
        (receiver.clone(), "/sink", "other"),
        (NodeAddr::mem(unique()), "/sink", "session"),
    ] {
        assert!(matches!(
            DurableOutbox::open(&scratch.0, &source, peer, path, sender, config()).await,
            Err(OutboxError::Invalid(_))
        ));
    }
    let second_source = node(&unique(), None, Arc::default()).await;
    assert!(DurableOutbox::open(
        &scratch.0,
        &second_source,
        receiver,
        "/sink",
        "session",
        config()
    )
    .await
    .is_err());
    source.shutdown().await.unwrap();
    second_source.shutdown().await.unwrap();
}

#[tokio::test]
async fn corrupt_or_missing_binding_never_retargets_existing_work() {
    for corrupt in [true, false] {
        let scratch = Scratch::new();
        let source = node(&unique(), None, Arc::default()).await;
        let receiver = NodeAddr::mem(unique());
        let mut outbox = DurableOutbox::open(
            &scratch.0,
            &source,
            receiver.clone(),
            "/sink",
            "session",
            config(),
        )
        .await
        .unwrap();
        outbox
            .handle()
            .enqueue(Work::TYPE_KEY, &payload(1))
            .await
            .unwrap();
        outbox.close().await.unwrap();
        if corrupt {
            std::fs::write(scratch.0.join("outbox.identity"), b"partial").unwrap();
        } else {
            std::fs::remove_file(scratch.0.join("outbox.identity")).unwrap();
        }
        assert!(matches!(
            DurableOutbox::open(&scratch.0, &source, receiver, "/sink", "session", config()).await,
            Err(OutboxError::Storage(_))
        ));
        assert_eq!(Wal::open(&scratch.0).unwrap().pending_len().unwrap(), 1);
        source.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn record_limit_rejects_without_consuming_sequence_and_ack_headroom_allows_progress() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let target_id = unique();
    let mut cfg = config();
    cfg.wal_limits.max_pending_records = 2;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        cfg,
    )
    .await
    .unwrap();
    let h = outbox.handle();
    assert_eq!(h.enqueue(Work::TYPE_KEY, &payload(1)).await.unwrap(), 1);
    assert_eq!(h.enqueue(Work::TYPE_KEY, &payload(2)).await.unwrap(), 2);
    assert!(matches!(
        h.enqueue(Work::TYPE_KEY, &payload(3)).await,
        Err(OutboxError::Rejected(_))
    ));
    assert_eq!(h.status().last_enqueued, 2);
    let state = Arc::new(State::default());
    let target = node(&target_id, None, state.clone()).await;
    acknowledged(&h, 2).await;
    assert_eq!(h.enqueue(Work::TYPE_KEY, &payload(3)).await.unwrap(), 3);
    acknowledged(&h, 3).await;
    assert_eq!(*state.effects.lock().unwrap(), vec![1, 2, 3]);
    outbox.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn disk_limit_reserves_enough_space_for_all_acks_and_the_final_checkpoint() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let target_id = unique();
    let mut cfg = config();
    cfg.segment_bytes = 300;
    cfg.wal_limits.max_disk_bytes = 2048;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        cfg,
    )
    .await
    .unwrap();
    let h = outbox.handle();
    let mut accepted = 0;
    for n in 1..100 {
        match h.enqueue(Work::TYPE_KEY, &payload(n)).await {
            Ok(seq) => {
                assert_eq!(seq, n);
                accepted = n;
            }
            Err(OutboxError::Rejected(_)) => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(accepted > 0 && accepted < 99);
    let state = Arc::new(State::default());
    let target = node(&target_id, None, state.clone()).await;
    acknowledged(&h, accepted).await;
    assert_eq!(state.effects.lock().unwrap().len(), accepted as usize);
    assert_eq!(h.status().pending, 0);
    assert_eq!(
        h.enqueue(Work::TYPE_KEY, &payload(111)).await.unwrap(),
        accepted + 1
    );
    acknowledged(&h, accepted + 1).await;
    outbox.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn byte_budget_invalid_config_and_sequence_exhaustion_do_not_publish_work() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let peer = NodeAddr::mem(unique());
    let mut cfg = config();
    cfg.command_capacity = 0;
    assert!(DurableOutbox::open(
        scratch.0.join("invalid"),
        &source,
        peer.clone(),
        "/sink",
        "session",
        cfg
    )
    .await
    .is_err());
    assert!(!scratch.0.join("invalid").exists());
    let mut cfg = config();
    cfg.command_bytes = 1;
    let mut outbox =
        DurableOutbox::open(&scratch.0, &source, peer.clone(), "/sink", "session", cfg)
            .await
            .unwrap();
    assert!(matches!(
        outbox.handle().enqueue(Work::TYPE_KEY, &payload(1)).await,
        Err(OutboxError::Capacity)
    ));
    assert!(outbox.handle().wait_acked(0).await.is_err());
    assert!(outbox.handle().wait_acked(1).await.is_err());
    outbox.close().await.unwrap();
    {
        let wal = Wal::open(&scratch.0).unwrap();
        wal.ack(&peer.node_id, "session", u64::MAX).unwrap();
        wal.sync().unwrap();
    }
    let mut outbox = DurableOutbox::open(&scratch.0, &source, peer, "/sink", "session", config())
        .await
        .unwrap();
    assert_eq!(outbox.handle().status().last_acked, u64::MAX);
    assert!(matches!(
        outbox.handle().enqueue(Work::TYPE_KEY, &payload(1)).await,
        Err(OutboxError::Rejected(_))
    ));
    assert_eq!(outbox.handle().status().pending, 0);
    outbox.close().await.unwrap();
    source.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn sigkill_after_acceptance_recovers_and_automatically_delivers_in_a_new_process_session() {
    const ENV: &str = "PARROT_OUTBOX_KILL_CHILD";
    if let Some(dir) = std::env::var_os(ENV) {
        let source = node("kill-source", None, Arc::default()).await;
        let outbox = DurableOutbox::open(
            PathBuf::from(&dir),
            &source,
            NodeAddr::mem("kill-target"),
            "/sink",
            "kill-session",
            config(),
        )
        .await
        .unwrap();
        let handle = outbox.handle();
        let values: Vec<_> = (42..74).map(payload).collect();
        let results = futures::future::join_all(
            values
                .iter()
                .map(|bytes| handle.enqueue(Work::TYPE_KEY, bytes)),
        )
        .await;
        assert_eq!(
            results.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
            (1..=32).collect::<Vec<_>>()
        );
        assert!(handle.status().acceptance_syncs < 32);
        std::fs::write(PathBuf::from(dir).join("ready"), b"accepted").unwrap();
        std::future::pending::<()>().await;
        return;
    }
    let scratch = Scratch::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sigkill_after_acceptance_recovers_and_automatically_delivers_in_a_new_process_session",
            "--nocapture",
        ])
        .env(ENV, &scratch.0)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !scratch.0.join("ready").exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child ended before durable acceptance: {status}");
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child acceptance stalled");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    child.kill().unwrap();
    let killed = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(killed.signal(), Some(libc::SIGKILL));
    let source = node("kill-source", None, Arc::default()).await;
    let state = Arc::new(State::default());
    let target = node("kill-target", None, state.clone()).await;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem("kill-target"),
        "/sink",
        "kill-session",
        config(),
    )
    .await
    .unwrap();
    acknowledged(&outbox.handle(), 32).await;
    assert_eq!(*state.effects.lock().unwrap(), (42..74).collect::<Vec<_>>());
    outbox.close().await.unwrap();
    assert_eq!(Wal::open(&scratch.0).unwrap().pending_len().unwrap(), 0);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_producers_have_unique_contiguous_durable_sequences_and_matching_wire_order() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let target_id = unique();
    let state = Arc::new(State::default());
    let target = node(&target_id, None, state.clone()).await;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(target_id),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for value in 0..64 {
        let h = outbox.handle();
        tasks.spawn(async move {
            (
                h.enqueue(Work::TYPE_KEY, &payload(value)).await.unwrap(),
                value,
            )
        });
    }
    let mut accepted = Vec::new();
    while let Some(result) = tasks.join_next().await {
        accepted.push(result.unwrap());
    }
    accepted.sort_unstable();
    assert_eq!(
        accepted.iter().map(|v| v.0).collect::<Vec<_>>(),
        (1..=64).collect::<Vec<_>>()
    );
    acknowledged(&outbox.handle(), 64).await;
    assert_eq!(
        *state.effects.lock().unwrap(),
        accepted.iter().map(|v| v.1).collect::<Vec<_>>()
    );
    outbox.close().await.unwrap();
    // Outbox closure must not retire the shared connection created by its worker.
    let actor = source
        .remote_ref(&format!("parrot://{}/sink", target.config.node_id))
        .unwrap();
    actor
        .deliver_with_ack(Box::new(Work(99)), Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(state.effects.lock().unwrap().last(), Some(&99));
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn foreign_or_noncontiguous_pending_records_fail_closed_on_reopen() {
    for mode in ["foreign", "gap", "duplicate"] {
        let scratch = Scratch::new();
        let source = node(&unique(), None, Arc::default()).await;
        let peer = NodeAddr::mem(unique());
        let mut outbox = DurableOutbox::open(
            &scratch.0,
            &source,
            peer.clone(),
            "/sink",
            "session",
            config(),
        )
        .await
        .unwrap();
        outbox
            .handle()
            .enqueue(Work::TYPE_KEY, &payload(1))
            .await
            .unwrap();
        outbox.close().await.unwrap();
        {
            let wal = Wal::open(&scratch.0).unwrap();
            let mut record = wal.replay(&peer.node_id).unwrap()[0].clone();
            match mode {
                "foreign" => record.path = "/other".into(),
                "gap" => record.seq = 3,
                _ => {}
            }
            wal.append(record).unwrap();
            wal.sync().unwrap();
        }
        assert!(matches!(
            DurableOutbox::open(&scratch.0, &source, peer, "/sink", "session", config()).await,
            Err(OutboxError::Storage(_))
        ));
        assert_eq!(Wal::open(&scratch.0).unwrap().pending_len().unwrap(), 2);
        source.shutdown().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn identity_symlink_and_special_file_are_rejected_without_following_or_blocking() {
    const ENV: &str = "PARROT_OUTBOX_SPECIAL_CHILD";
    let Some(path) = std::env::var_os(ENV) else {
        let scratch = Scratch::new();
        run_probe(
            "identity_symlink_and_special_file_are_rejected_without_following_or_blocking",
            ENV,
            &scratch.0,
        );
        return;
    };
    use std::os::unix::fs::symlink;
    let scratch = Scratch(PathBuf::from(path));
    let source = node(&unique(), None, Arc::default()).await;
    let peer = NodeAddr::mem(unique());
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        peer.clone(),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    outbox.close().await.unwrap();
    let binding = scratch.0.join("outbox.identity");
    let saved = scratch.0.join("saved.identity");
    std::fs::rename(&binding, &saved).unwrap();
    symlink(&saved, &binding).unwrap();
    assert!(DurableOutbox::open(
        &scratch.0,
        &source,
        peer.clone(),
        "/sink",
        "session",
        config()
    )
    .await
    .is_err());
    std::fs::remove_file(&binding).unwrap();
    let cpath = std::ffi::CString::new(binding.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    // Parent process kills/reaps this probe if a future regression blocks open.
    assert!(
        DurableOutbox::open(&scratch.0, &source, peer, "/sink", "session", config())
            .await
            .is_err()
    );
    source.shutdown().await.unwrap();
}

#[cfg(unix)]
#[test]
fn actual_partial_write_stops_worker_rejects_future_commands_and_recovers_prior_acceptance() {
    const ENV: &str = "PARROT_OUTBOX_IO_CHILD";
    if let Some(path) = std::env::var_os(ENV) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let source = node("io-source", None, Arc::default()).await;
                let mut outbox = DurableOutbox::open(
                    PathBuf::from(&path),
                    &source,
                    NodeAddr::mem("io-target"),
                    "/sink",
                    "session",
                    OutboxConfig {
                        group_commit_max_records: 2,
                        group_commit_interval: Duration::from_secs(1),
                        ..config()
                    },
                )
                .await
                .unwrap();
                let h = outbox.handle();
                assert_eq!(h.enqueue(Work::TYPE_KEY, &payload(1)).await.unwrap(), 1);
                unsafe {
                    assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
                    let limit = libc::rlimit {
                        rlim_cur: 512,
                        rlim_max: 512,
                    };
                    assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
                }
                let small = payload(2);
                let large = vec![0; 4096];
                let (first, second) = tokio::join!(
                    h.enqueue(Work::TYPE_KEY, &small),
                    h.enqueue(Work::TYPE_KEY, &large)
                );
                assert!(
                    matches!(first, Err(OutboxError::Storage(_))),
                    "no earlier member may succeed before the group sync"
                );
                assert!(matches!(second, Err(OutboxError::Storage(_))));
                assert_eq!(h.status().last_enqueued, 1);
                assert_eq!(h.status().acceptance_syncs, 1);
                assert!(outbox.close().await.is_err());
                assert!(h.status().closed);
                assert!(matches!(
                    h.enqueue(Work::TYPE_KEY, &payload(3)).await,
                    Err(OutboxError::Closed)
                ));
                source.shutdown().await.unwrap();
            });
        return;
    }
    let scratch = Scratch::new();
    run_probe(
        "actual_partial_write_stops_worker_rejects_future_commands_and_recovers_prior_acceptance",
        ENV,
        &scratch.0,
    );
    let wal = Wal::open(&scratch.0).unwrap();
    let pending = wal.replay("io-target").unwrap();
    // The first, unacknowledged group member was a complete write and can
    // survive even though both calls failed. Only the partial final tail is cut.
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].payload.as_ref(), payload(1));
    assert_eq!(pending[1].seq, 2);
    assert_eq!(pending[1].payload.as_ref(), payload(2));
}

#[cfg(unix)]
fn run_probe(test: &str, key: &str, directory: &std::path::Path) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(key, directory)
        // RLIMIT_FSIZE must affect WAL writes, not the parent's redirected log.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            print!("{}", String::from_utf8_lossy(&output.stdout));
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success(), "child {test}: {}", output.status);
            return;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("probe {test} did not terminate");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[tokio::test]
async fn group_commit_configuration_is_checked_before_creating_storage() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    for (index, (max, interval)) in [
        (0, Duration::ZERO),
        (65537, Duration::ZERO),
        (1, Duration::from_secs(2)),
    ]
    .into_iter()
    .enumerate()
    {
        let directory = scratch.0.join(index.to_string());
        let cfg = OutboxConfig {
            group_commit_max_records: max,
            group_commit_interval: interval,
            ..config()
        };
        assert!(matches!(
            DurableOutbox::open(
                &directory,
                &source,
                NodeAddr::mem("offline"),
                "/sink",
                "session",
                cfg
            )
            .await,
            Err(OutboxError::Invalid(_))
        ));
        assert!(!directory.exists());
    }
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn group_collection_retains_byte_budget_and_cancelled_work_does_not_consume_sequence() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let encoded = payload(7);
    let cfg = OutboxConfig {
        command_bytes: (Work::TYPE_KEY.len() + encoded.len()) as u32,
        group_commit_interval: Duration::from_secs(1),
        ..config()
    };
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem("offline"),
        "/sink",
        "session",
        cfg,
    )
    .await
    .unwrap();
    let h = outbox.handle();
    let mut first = Box::pin(h.enqueue(Work::TYPE_KEY, &encoded));
    assert!(futures::poll!(&mut first).is_pending());
    assert!(matches!(
        h.enqueue(Work::TYPE_KEY, &encoded).await,
        Err(OutboxError::Capacity)
    ));
    drop(first);
    outbox.close().await.unwrap();
    assert_eq!(h.status().last_enqueued, 0);
    assert_eq!(Wal::open(&scratch.0).unwrap().pending_len().unwrap(), 0);
    let cfg = OutboxConfig {
        group_commit_max_records: 1,
        ..config()
    };
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem("offline"),
        "/sink",
        "session",
        cfg,
    )
    .await
    .unwrap();
    assert_eq!(
        outbox
            .handle()
            .enqueue(Work::TYPE_KEY, &encoded)
            .await
            .unwrap(),
        1
    );
    assert_eq!(outbox.handle().status().acceptance_syncs, 1);
    outbox.close().await.unwrap();
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn bounded_groups_recover_exact_submission_history_across_segment_rotation() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let peer = NodeAddr::mem(unique());
    let cfg = OutboxConfig {
        group_commit_max_records: 8,
        group_commit_interval: Duration::from_millis(100),
        segment_bytes: 256,
        ..config()
    };
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        peer.clone(),
        "/sink",
        "session",
        cfg.clone(),
    )
    .await
    .unwrap();
    let h = outbox.handle();
    let values: Vec<_> = (0..32).map(payload).collect();
    let results =
        futures::future::join_all(values.iter().map(|value| h.enqueue(Work::TYPE_KEY, value)))
            .await;
    assert_eq!(
        results.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
        (1..=32).collect::<Vec<_>>()
    );
    assert!((4..32).contains(&h.status().acceptance_syncs));
    assert!(h.status().last_group_records <= 8);
    assert_eq!(h.status().pending, 32);
    outbox.close().await.unwrap();
    let segments = std::fs::read_dir(&scratch.0)
        .unwrap()
        .filter(|p| {
            p.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("seg-")
        })
        .count();
    assert!(segments > 1);
    let state = Arc::new(State::default());
    let target = node(&peer.node_id, None, state.clone()).await;
    let mut outbox = DurableOutbox::open(&scratch.0, &source, peer, "/sink", "session", cfg)
        .await
        .unwrap();
    acknowledged(&outbox.handle(), 32).await;
    assert_eq!(*state.effects.lock().unwrap(), (0..32).collect::<Vec<_>>());
    outbox.close().await.unwrap();
    assert_eq!(Wal::open(&scratch.0).unwrap().pending_len().unwrap(), 0);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn keyed_retries_share_one_sequence_before_and_after_delivery_gc_and_reopen() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let target_id = unique();
    let cfg = OutboxConfig {
        idempotent_submissions: true,
        wal_limits: parrot_remote::durable::WalLimits {
            max_submission_receipts: 2,
            ..Default::default()
        },
        ..config()
    };
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        cfg.clone(),
    )
    .await
    .unwrap();
    let h = outbox.handle();
    let bytes = payload(10);
    let results = futures::future::join_all(
        (0..32).map(|_| h.enqueue_idempotent("producer:request-a", Work::TYPE_KEY, &bytes)),
    )
    .await;
    assert_eq!(
        results.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
        vec![1; 32]
    );
    assert_eq!(h.status().pending, 1);
    assert_eq!(h.status().last_enqueued, 1);
    assert_eq!(h.status().submission_receipts, 1);
    assert!(h
        .enqueue_idempotent("producer:request-a", Work::TYPE_KEY, &payload(99))
        .await
        .is_err());
    assert_eq!(
        h.enqueue_idempotent("producer:request-b", Work::TYPE_KEY, &payload(20))
            .await
            .unwrap(),
        2
    );
    assert!(h
        .enqueue_idempotent("producer:request-c", Work::TYPE_KEY, &payload(30))
        .await
        .is_err());
    assert_eq!(
        h.enqueue_idempotent("producer:request-a", Work::TYPE_KEY, &bytes)
            .await
            .unwrap(),
        1
    );
    outbox.close().await.unwrap();
    let state = Arc::new(State::default());
    let target = node(&target_id, None, state.clone()).await;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        cfg.clone(),
    )
    .await
    .unwrap();
    acknowledged(&outbox.handle(), 2).await;
    assert_eq!(*state.effects.lock().unwrap(), vec![10, 20]);
    outbox.close().await.unwrap();
    assert_eq!(Wal::open(&scratch.0).unwrap().pending_len().unwrap(), 0);
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        cfg,
    )
    .await
    .unwrap();
    assert_eq!(
        outbox
            .handle()
            .enqueue_idempotent("producer:request-a", Work::TYPE_KEY, &bytes)
            .await
            .unwrap(),
        1
    );
    assert_eq!(outbox.handle().status().last_enqueued, 2);
    assert_eq!(outbox.handle().status().pending, 0);
    assert_eq!(*state.effects.lock().unwrap(), vec![10, 20]);
    outbox.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn keyed_mode_is_explicit_bound_and_invalid_ids_or_reserved_keys_never_consume_sequences() {
    let s = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let target = NodeAddr::mem(unique());
    let mut plain = DurableOutbox::open(
        s.0.join("plain"),
        &source,
        target.clone(),
        "/sink",
        "session",
        config(),
    )
    .await
    .unwrap();
    assert!(matches!(
        plain
            .handle()
            .enqueue_idempotent("id", Work::TYPE_KEY, &payload(1))
            .await,
        Err(OutboxError::Invalid(_))
    ));
    plain.close().await.unwrap();
    let cfg = OutboxConfig {
        idempotent_submissions: true,
        ..config()
    };
    assert!(matches!(
        DurableOutbox::open(
            s.0.join("plain"),
            &source,
            target.clone(),
            "/sink",
            "session",
            cfg.clone()
        )
        .await,
        Err(OutboxError::Invalid(_))
    ));
    let mut outbox = DurableOutbox::open(
        s.0.join("keyed"),
        &source,
        target.clone(),
        "/sink",
        "session",
        cfg,
    )
    .await
    .unwrap();
    let h = outbox.handle();
    for id in [
        String::new(),
        "x".repeat(1025),
        "bad\0id".into(),
        "bad\nid".into(),
    ] {
        assert!(h
            .enqueue_idempotent(&id, Work::TYPE_KEY, &payload(1))
            .await
            .is_err());
    }
    for key in ["#outbox-accept-v1", "#outbox-receipt-v1"] {
        assert!(h.enqueue(key, b"body").await.is_err());
        assert!(h.enqueue_idempotent("id", key, b"body").await.is_err());
    }
    assert_eq!(
        h.enqueue_idempotent("id", Work::TYPE_KEY, &payload(1))
            .await
            .unwrap(),
        1
    );
    assert_eq!(h.enqueue(Work::TYPE_KEY, &payload(2)).await.unwrap(), 2);
    outbox.close().await.unwrap();
    assert!(matches!(
        DurableOutbox::open(
            s.0.join("keyed"),
            &source,
            target,
            "/sink",
            "session",
            config()
        )
        .await,
        Err(OutboxError::Invalid(_))
    ));
    source.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn sigkill_keyed_acceptance_preserves_original_ids_sequences_and_delivery_history() {
    const ENV: &str = "PARROT_OUTBOX_KEYED_KILL_CHILD";
    if let Some(dir) = std::env::var_os(ENV) {
        let source = node("keyed-kill-source", None, Arc::default()).await;
        let outbox = DurableOutbox::open(
            PathBuf::from(&dir),
            &source,
            NodeAddr::mem("keyed-kill-target"),
            "/sink",
            "keyed-kill-session",
            OutboxConfig {
                idempotent_submissions: true,
                ..config()
            },
        )
        .await
        .unwrap();
        let handle = outbox.handle();
        let values: Vec<_> = (42..74).map(payload).collect();
        let results = futures::future::join_all(values.iter().enumerate().map(|(index, bytes)| {
            let handle = handle.clone();
            async move {
                handle
                    .enqueue_idempotent(&format!("request-{index}"), Work::TYPE_KEY, bytes)
                    .await
            }
        }))
        .await;
        assert_eq!(
            results.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
            (1..=32).collect::<Vec<_>>()
        );
        assert!(handle.status().acceptance_syncs < 32);
        std::fs::write(PathBuf::from(dir).join("ready"), b"accepted").unwrap();
        std::future::pending::<()>().await;
        return;
    }
    let scratch = Scratch::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sigkill_keyed_acceptance_preserves_original_ids_sequences_and_delivery_history",
            "--nocapture",
        ])
        .env(ENV, &scratch.0)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !scratch.0.join("ready").exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child ended before durable acceptance: {status}");
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child acceptance stalled");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    child.kill().unwrap();
    let killed = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(killed.signal(), Some(libc::SIGKILL));
    let source = node("keyed-kill-source", None, Arc::default()).await;
    let state = Arc::new(State::default());
    let target = node("keyed-kill-target", None, state.clone()).await;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem("keyed-kill-target"),
        "/sink",
        "keyed-kill-session",
        OutboxConfig {
            idempotent_submissions: true,
            ..config()
        },
    )
    .await
    .unwrap();
    for index in 0..32 {
        assert_eq!(
            outbox
                .handle()
                .enqueue_idempotent(
                    &format!("request-{index}"),
                    Work::TYPE_KEY,
                    &payload(42 + index)
                )
                .await
                .unwrap(),
            index + 1
        );
    }
    assert_eq!(outbox.handle().status().last_enqueued, 32);
    acknowledged(&outbox.handle(), 32).await;
    assert_eq!(*state.effects.lock().unwrap(), (42..74).collect::<Vec<_>>());
    outbox.close().await.unwrap();
    assert_eq!(Wal::open(&scratch.0).unwrap().pending_len().unwrap(), 0);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn keyed_disk_limit_reserves_receipt_checkpoint_space_before_acceptance() {
    let scratch = Scratch::new();
    let source = node(&unique(), None, Arc::default()).await;
    let target_id = unique();
    let mut cfg = config();
    cfg.idempotent_submissions = true;
    cfg.segment_bytes = 300;
    cfg.wal_limits.max_disk_bytes = 2048;
    let mut outbox = DurableOutbox::open(
        &scratch.0,
        &source,
        NodeAddr::mem(&target_id),
        "/sink",
        "session",
        cfg,
    )
    .await
    .unwrap();
    let h = outbox.handle();
    let mut accepted = 0;
    for n in 1..100 {
        match h
            .enqueue_idempotent(&format!("request-{n}"), Work::TYPE_KEY, &payload(n))
            .await
        {
            Ok(seq) => {
                assert_eq!(seq, n);
                accepted = n;
            }
            Err(OutboxError::Rejected(_)) => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(accepted > 0 && accepted < 99);
    let state = Arc::new(State::default());
    let target = node(&target_id, None, state.clone()).await;
    acknowledged(&h, accepted).await;
    assert_eq!(
        *state.effects.lock().unwrap(),
        (1..=accepted).collect::<Vec<_>>()
    );
    assert_eq!(h.status().pending, 0);
    assert_eq!(
        h.enqueue_idempotent("next", Work::TYPE_KEY, &payload(111))
            .await
            .unwrap(),
        accepted + 1
    );
    acknowledged(&h, accepted + 1).await;
    outbox.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}
