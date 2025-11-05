use bytes::Bytes;
use parrot_remote::{
    durable::*, frame::Frame, registry::ReplyPayload, LocalLookup, NodeAddr, RemoteActorSystem,
    RemoteConfig,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
fn unique() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "inbox-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let p = std::env::temp_dir().join(unique());
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn message(seq: u64) -> DurableEnvelope {
    DurableEnvelope {
        source: "source".into(),
        session: "session".into(),
        receiver: "receiver".into(),
        path: "/state".into(),
        sequence: seq,
        type_key: "counter:add#v1".into(),
        payload: Bytes::copy_from_slice(&7u64.to_le_bytes()),
    }
}
fn value(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().unwrap())
}
fn add(state: &[u8], message: &DurableEnvelope) -> Result<Bytes, String> {
    if message.type_key != "counter:add#v1" || message.payload.len() != 8 {
        return Err("unsupported operation".into());
    }
    let n = value(state)
        .checked_add(value(&message.payload))
        .ok_or("overflow")?;
    Ok(Bytes::copy_from_slice(&n.to_le_bytes()))
}
async fn inbox(dir: &Scratch, node: &str) -> TransactionalInbox {
    TransactionalInbox::open(
        &dir.0,
        node,
        "/state",
        Bytes::copy_from_slice(&0u64.to_le_bytes()),
        InboxLimits::default(),
        add,
    )
    .await
    .unwrap()
}
struct Lookup(Option<Arc<dyn DurableReceiver>>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
    fn durable_receiver(&self, path: &str) -> Option<Arc<dyn DurableReceiver>> {
        if path == "/state" {
            self.0.clone()
        } else {
            None
        }
    }
}
async fn node(
    config: RemoteConfig,
    receiver: Option<Arc<dyn DurableReceiver>>,
) -> Arc<RemoteActorSystem> {
    let node = RemoteActorSystem::new(config, Arc::new(Lookup(receiver))).unwrap();
    node.start().await.unwrap();
    node
}
async fn wait_revision(h: &InboxHandle, n: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.snapshot().revision < n {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn outbox_config() -> OutboxConfig {
    OutboxConfig {
        transactional: true,
        attempt_timeout: Duration::from_millis(200),
        retry_min: Duration::from_millis(10),
        retry_max: Duration::from_millis(50),
        ..Default::default()
    }
}
async fn raw(
    source: &Arc<RemoteActorSystem>,
    via: &str,
    envelope: &DurableEnvelope,
    hop: u8,
    path: Option<&str>,
) -> ReplyPayload {
    let sender = source
        .links_snapshot()
        .await
        .into_iter()
        .find(|v| v.0 == via)
        .unwrap()
        .1;
    let cid = source.callbacks.next_cid();
    let (tx, rx) = tokio::sync::oneshot::channel();
    source.callbacks.insert_on(cid, via, tx, &sender).unwrap();
    let mut frame = Frame::tell_with_ack(
        cid,
        path.unwrap_or(&envelope.target_path()),
        DURABLE_KEY,
        envelope.encode().unwrap(),
    );
    frame.header.hop_count = hop;
    sender.send(frame).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), rx).await;
    source.callbacks.remove(cid);
    result.unwrap().unwrap()
}
fn success(reply: ReplyPayload, seq: u64) {
    match reply {
        ReplyPayload::Ok(bytes, key) => {
            assert_eq!(key, DURABLE_RECEIPT_KEY);
            assert_eq!(&bytes[..], &seq.to_le_bytes());
        }
        other => panic!("{other:?}"),
    }
}
fn denied(reply: ReplyPayload) {
    assert!(matches!(reply, ReplyPayload::Err(_, _)), "{reply:?}");
}

#[test]
fn envelope_has_a_stable_bounded_format_and_rejects_invalid_identity() {
    let e = message(1);
    let encoded = e.encode().unwrap();
    assert_eq!(encoded[0], 1);
    assert_eq!(&encoded[1..9], &1u64.to_le_bytes());
    let mut golden = vec![1, 1, 0, 0, 0, 0, 0, 0, 0, 6, 0, 7, 0, 8, 0, 6, 0, 14, 0];
    golden.extend_from_slice(b"sourcesessionreceiver/statecounter:add#v1");
    golden.extend_from_slice(&7u64.to_le_bytes());
    assert_eq!(encoded.as_ref(), golden);
    assert_eq!(DurableEnvelope::decode(encoded.clone()).unwrap(), e);
    // Every identity/header truncation must fail. Payload has no separate length;
    // empty business payload is valid and its codec owns payload validation.
    for n in 0..encoded.len() - e.payload.len() {
        assert!(
            DurableEnvelope::decode(encoded.slice(..n)).is_err(),
            "prefix {n}"
        );
    }
    for field in 0..5 {
        let mut bytes = encoded.to_vec();
        bytes[9 + field * 2..11 + field * 2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(DurableEnvelope::decode(bytes.into()).is_err());
    }
    let mut bad = e.clone();
    bad.sequence = 0;
    assert!(bad.encode().is_err());
    bad = e.clone();
    bad.type_key = DURABLE_KEY.into();
    assert!(bad.encode().is_err());
    bad = e.clone();
    bad.source = "bad/node".into();
    assert!(bad.encode().is_err());
    bad = e;
    bad.payload = Bytes::from(vec![0; parrot_remote::frame::MAX_FRAME_LEN as usize]);
    assert!(bad.encode().is_err());
    let f = Frame::tell("/state", DURABLE_KEY, Bytes::new());
    assert!(f.validate_payload_flags().is_err());
}
#[tokio::test]
async fn atomic_state_and_stream_history_survive_reopen_and_duplicate_never_calls_transition() {
    let dir = Scratch::new();
    let mut owner = inbox(&dir, "receiver").await;
    let h = owner.handle();
    h.apply(message(1)).await.unwrap();
    h.apply(message(2)).await.unwrap();
    assert_eq!(value(&h.snapshot().state), 14);
    owner.close().await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mut reopened = TransactionalInbox::open(
        &dir.0,
        "receiver",
        "/state",
        Bytes::from_static(b"ignored"),
        InboxLimits::default(),
        move |state, m| {
            counter.fetch_add(1, Ordering::SeqCst);
            add(state, m)
        },
    )
    .await
    .unwrap();
    for seq in [2, 1, 2] {
        reopened.handle().apply(message(seq)).await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(reopened.handle().snapshot().revision, 2);
    reopened.handle().apply(message(3)).await.unwrap();
    assert_eq!(value(&reopened.handle().snapshot().state), 21);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    reopened.close().await.unwrap();
}
#[tokio::test]
async fn gaps_failed_panicking_or_oversized_transitions_leave_state_and_history_retryable() {
    let dir = Scratch::new();
    let mode = Arc::new(AtomicUsize::new(1));
    let input = mode.clone();
    let mut owner = TransactionalInbox::open(
        &dir.0,
        "receiver",
        "/state",
        0u64.to_le_bytes().to_vec().into(),
        InboxLimits {
            max_state_bytes: 8,
            ..Default::default()
        },
        move |s, m| match input.load(Ordering::SeqCst) {
            1 => Err("failure".into()),
            2 => panic!("transition panic"),
            3 => Ok(vec![0; 9].into()),
            _ => add(s, m),
        },
    )
    .await
    .unwrap();
    let h = owner.handle();
    assert!(matches!(
        h.apply(message(2)).await,
        Err(InboxError::Gap {
            expected: 1,
            received: 2
        })
    ));
    for n in 1..=3 {
        mode.store(n, Ordering::SeqCst);
        assert!(h.apply(message(1)).await.is_err());
        assert_eq!(h.snapshot().revision, 0);
        assert_eq!(value(&h.snapshot().state), 0);
    }
    mode.store(0, Ordering::SeqCst);
    h.apply(message(1)).await.unwrap();
    assert_eq!(value(&h.snapshot().state), 7);
    owner.close().await.unwrap();
}
#[tokio::test]
async fn stream_limits_reject_before_business_but_duplicates_and_existing_streams_progress() {
    let dir = Scratch::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mut owner = TransactionalInbox::open(
        &dir.0,
        "receiver",
        "/state",
        0u64.to_le_bytes().to_vec().into(),
        InboxLimits {
            max_streams: 1,
            ..Default::default()
        },
        move |s, m| {
            counter.fetch_add(1, Ordering::SeqCst);
            add(s, m)
        },
    )
    .await
    .unwrap();
    let h = owner.handle();
    h.apply(message(1)).await.unwrap();
    let mut other = message(1);
    other.session = "another".into();
    assert!(matches!(h.apply(other).await, Err(InboxError::Capacity)));
    h.apply(message(1)).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    h.apply(message(2)).await.unwrap();
    assert_eq!(value(&h.snapshot().state), 14);
    owner.close().await.unwrap();
}
#[tokio::test]
async fn receiver_binding_and_exclusive_owner_are_enforced() {
    let dir = Scratch::new();
    let mut owner = inbox(&dir, "receiver").await;
    assert!(TransactionalInbox::open(
        &dir.0,
        "receiver",
        "/state",
        Bytes::new(),
        InboxLimits::default(),
        add
    )
    .await
    .is_err());
    let mut m = message(1);
    m.receiver = "other".into();
    assert!(owner.handle().apply(m).await.is_err());
    owner.close().await.unwrap();
    assert!(TransactionalInbox::open(
        &dir.0,
        "other",
        "/state",
        Bytes::new(),
        InboxLimits::default(),
        add
    )
    .await
    .is_err());
    assert!(TransactionalInbox::open(
        &dir.0,
        "receiver",
        "/other",
        Bytes::new(),
        InboxLimits::default(),
        add
    )
    .await
    .is_err());
    assert!(owner.handle().apply(message(1)).await.is_err());
}
#[tokio::test]
async fn corrupt_snapshot_is_preserved_and_abandoned_temporary_is_safe_only_with_committed_state() {
    let dir = Scratch::new();
    let mut owner = inbox(&dir, "receiver").await;
    owner.handle().apply(message(1)).await.unwrap();
    owner.close().await.unwrap();
    std::fs::write(dir.0.join("inbox.next"), b"partial").unwrap();
    let mut recovered = inbox(&dir, "receiver").await;
    assert_eq!(value(&recovered.handle().snapshot().state), 7);
    assert!(!dir.0.join("inbox.next").exists());
    recovered.close().await.unwrap();
    let main = dir.0.join("inbox.state");
    let mut bytes = std::fs::read(&main).unwrap();
    bytes[12] ^= 1;
    std::fs::write(&main, &bytes).unwrap();
    assert!(TransactionalInbox::open(
        &dir.0,
        "receiver",
        "/state",
        Bytes::new(),
        InboxLimits::default(),
        add
    )
    .await
    .is_err());
    assert_eq!(std::fs::read(main).unwrap(), bytes);
    let empty = Scratch::new();
    std::fs::write(empty.0.join("inbox.next"), b"partial").unwrap();
    assert!(TransactionalInbox::open(
        &empty.0,
        "receiver",
        "/state",
        Bytes::new(),
        InboxLimits::default(),
        add
    )
    .await
    .is_err());
}
struct LostReceipt {
    handle: InboxHandle,
    lose: AtomicBool,
}
#[async_trait::async_trait]
impl DurableReceiver for LostReceipt {
    async fn receive(&self, m: DurableEnvelope) -> Result<(), InboxError> {
        self.handle.apply(m).await?;
        if self.lose.swap(false, Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}
#[tokio::test]
async fn actual_outbox_receipt_loss_and_receiver_restart_do_not_repeat_committed_business_state() {
    let incoming = Scratch::new();
    let outgoing = Scratch::new();
    let target_id = unique();
    let source_id = unique();
    let mut receiver = inbox(&incoming, &target_id).await;
    let target = node(
        RemoteConfig::mem(&target_id),
        Some(Arc::new(LostReceipt {
            handle: receiver.handle(),
            lose: true.into(),
        })),
    )
    .await;
    let source = node(RemoteConfig::mem(&source_id), None).await;
    let mut cfg = outbox_config();
    cfg.attempt_timeout = Duration::from_secs(60);
    let mut outbox = DurableOutbox::open(
        &outgoing.0,
        &source,
        NodeAddr::mem(&target_id),
        "/state",
        "session",
        cfg.clone(),
    )
    .await
    .unwrap();
    outbox
        .handle()
        .enqueue("counter:add#v1", &7u64.to_le_bytes())
        .await
        .unwrap();
    wait_revision(&receiver.handle(), 1).await;
    assert_eq!(outbox.handle().status().last_acked, 0);
    assert_eq!(value(&receiver.handle().snapshot().state), 7);
    outbox.close().await.unwrap();
    target.shutdown().await.unwrap();
    receiver.close().await.unwrap();
    let mut receiver = inbox(&incoming, &target_id).await;
    let target = node(
        RemoteConfig::mem(&target_id),
        Some(Arc::new(receiver.handle())),
    )
    .await;
    let mut outbox = DurableOutbox::open(
        &outgoing.0,
        &source,
        NodeAddr::mem(&target_id),
        "/state",
        "session",
        cfg,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), outbox.handle().wait_acked(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value(&receiver.handle().snapshot().state), 7);
    assert_eq!(receiver.handle().snapshot().revision, 1);
    outbox
        .handle()
        .enqueue("counter:add#v1", &7u64.to_le_bytes())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), outbox.handle().wait_acked(2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value(&receiver.handle().snapshot().state), 14);
    outbox.close().await.unwrap();
    target.shutdown().await.unwrap();
    receiver.close().await.unwrap();
    source.shutdown().await.unwrap();
}
#[tokio::test]
async fn real_connection_origin_spoof_destination_rewrite_and_untrusted_relay_are_rejected() {
    let dir = Scratch::new();
    let target_id = unique();
    let source_id = unique();
    let mut receiver = inbox(&dir, &target_id).await;
    let target = node(
        RemoteConfig::mem(&target_id),
        Some(Arc::new(receiver.handle())),
    )
    .await;
    let source = node(RemoteConfig::mem(&source_id), None).await;
    source.connect(&NodeAddr::mem(&target_id)).await.unwrap();
    let mut m = message(1);
    m.source = source_id.clone();
    m.receiver = target_id.clone();
    let mut forged = m.clone();
    forged.source = "other-node".into();
    denied(raw(&source, &target_id, &forged, 0, None).await);
    denied(
        raw(
            &source,
            &target_id,
            &m,
            0,
            Some(&format!("parrot://{target_id}/other")),
        )
        .await,
    );
    denied(raw(&source, &target_id, &m, 1, None).await);
    assert_eq!(receiver.handle().snapshot().revision, 0);
    success(raw(&source, &target_id, &m, 0, None).await, 1);
    assert_eq!(value(&receiver.handle().snapshot().state), 7);
    target.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    receiver.close().await.unwrap();
}
#[tokio::test]
async fn real_hub_requires_explicit_receiver_trust_and_preserves_durable_identity() {
    let dir = Scratch::new();
    let target_id = unique();
    let source_id = unique();
    let hub_id = unique();
    let mut receiver = inbox(&dir, &target_id).await;
    let target = node(
        RemoteConfig::mem(&target_id),
        Some(Arc::new(receiver.handle())),
    )
    .await;
    let hub = node(RemoteConfig::mem(&hub_id), None).await;
    let source = node(RemoteConfig::mem(&source_id), None).await;
    source.connect(&NodeAddr::mem(&hub_id)).await.unwrap();
    hub.connect(&NodeAddr::mem(&target_id)).await.unwrap();
    let mut m = message(1);
    m.source = source_id;
    m.receiver = target_id;
    denied(raw(&source, &hub_id, &m, 0, None).await);
    assert_eq!(receiver.handle().snapshot().revision, 0);
    target
        .ingress
        .set_durable_relays([hub_id.clone()].into_iter().collect())
        .unwrap();
    success(raw(&source, &hub_id, &m, 0, None).await, 1);
    success(raw(&source, &hub_id, &m, 0, None).await, 1);
    assert_eq!(receiver.handle().snapshot().revision, 1);
    target.shutdown().await.unwrap();
    hub.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    receiver.close().await.unwrap();
}
#[tokio::test]
async fn unauthenticated_tcp_cannot_select_another_senders_persistent_stream() {
    let dir = Scratch::new();
    let target_id = unique();
    let source_id = unique();
    let mut receiver = inbox(&dir, &target_id).await;
    let target = node(
        RemoteConfig::tcp(&target_id, Some("127.0.0.1:0".parse().unwrap())),
        Some(Arc::new(receiver.handle())),
    )
    .await;
    let source = node(RemoteConfig::tcp(&source_id, None), None).await;
    source
        .connect(&NodeAddr::tcp(&target_id, target.local_addr().unwrap()))
        .await
        .unwrap();
    let mut m = message(1);
    m.source = source_id;
    m.receiver = target_id.clone();
    denied(raw(&source, &target_id, &m, 0, None).await);
    assert_eq!(receiver.handle().snapshot().revision, 0);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
    receiver.close().await.unwrap();
}
#[tokio::test]
async fn transactional_outbox_binding_cannot_downgrade_on_reopen() {
    let dir = Scratch::new();
    let source = node(RemoteConfig::mem(unique()), None).await;
    let peer = NodeAddr::mem(unique());
    let mut owner = DurableOutbox::open(
        &dir.0,
        &source,
        peer.clone(),
        "/state",
        "session",
        outbox_config(),
    )
    .await
    .unwrap();
    owner.close().await.unwrap();
    assert!(DurableOutbox::open(
        &dir.0,
        &source,
        peer,
        "/state",
        "session",
        OutboxConfig::default()
    )
    .await
    .is_err());
    source.shutdown().await.unwrap();
}
#[tokio::test]
async fn concurrent_duplicates_commit_once_and_each_source_and_session_is_independent() {
    let dir = Scratch::new();
    let mut owner = inbox(&dir, "receiver").await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let h = owner.handle();
        tasks.spawn(async move {
            h.apply(message(1)).await.unwrap();
        });
    }
    while let Some(r) = tasks.join_next().await {
        r.unwrap();
    }
    assert_eq!(owner.handle().snapshot().revision, 1);
    let mut second = message(1);
    second.source = "another-source".into();
    owner.handle().apply(second).await.unwrap();
    let mut third = message(1);
    third.session = "another-session".into();
    owner.handle().apply(third).await.unwrap();
    assert_eq!(value(&owner.handle().snapshot().state), 21);
    owner.close().await.unwrap();
}

#[tokio::test]
async fn mtls_tcp_and_quic_deliver_through_the_actual_transactional_outbox() {
    let mut ca_params = rcgen::CertificateParams::default();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    let ca = ca_params.self_signed(&key).unwrap().pem().into_bytes();
    let issuer = rcgen::Issuer::new(ca_params, key);
    let credentials = |name: &str| {
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &issuer).unwrap();
        parrot_remote::tls::TlsConfig {
            cert_pem: cert.pem().into_bytes(),
            key_pem: key.serialize_pem().into_bytes(),
            ca_pem: ca.clone(),
        }
    };
    for quic in [false, true] {
        let incoming = Scratch::new();
        let outgoing = Scratch::new();
        let mut receiver = inbox(&incoming, "receiver.test").await;
        let config = |name| {
            let bind = Some("127.0.0.1:0".parse().unwrap());
            if quic {
                RemoteConfig::quic(name, bind)
            } else {
                RemoteConfig::tcp(name, bind)
            }
        };
        let target = node(
            config("receiver.test").with_mtls(credentials("receiver.test"), []),
            Some(Arc::new(receiver.handle())),
        )
        .await;
        let source = node(
            config("source.test").with_mtls(credentials("source.test"), []),
            None,
        )
        .await;
        let peer = if quic {
            NodeAddr::quic("receiver.test", target.local_addr().unwrap())
        } else {
            NodeAddr::tcp("receiver.test", target.local_addr().unwrap())
        };
        let mut outbox = DurableOutbox::open(
            &outgoing.0,
            &source,
            peer,
            "/state",
            "session",
            outbox_config(),
        )
        .await
        .unwrap();
        outbox
            .handle()
            .enqueue("counter:add#v1", &7u64.to_le_bytes())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), outbox.handle().wait_acked(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(value(&receiver.handle().snapshot().state), 7);
        assert_eq!(
            source.links_snapshot().await[0].1.authentication(),
            parrot_remote::PeerAuthentication::MutualTls
        );
        outbox.close().await.unwrap();
        source.shutdown().await.unwrap();
        target.shutdown().await.unwrap();
        receiver.close().await.unwrap();
    }
}
#[tokio::test]
async fn legacy_completion_capability_cannot_be_mistaken_for_transactional_support() {
    use parrot_remote::{
        handshake::{caps, HandshakeBody},
        transport::{memory::MemoryTransport, Transport},
    };
    let dir = Scratch::new();
    let source = node(RemoteConfig::mem(unique()), None).await;
    let peer_id = unique();
    let (inbound, mut rx) = tokio::sync::mpsc::channel(8);
    let (stop, _) = tokio::sync::watch::channel(false);
    let peer = MemoryTransport::new(
        HandshakeBody {
            node_id: peer_id.clone(),
            capabilities: caps::BIN | caps::TELL_COMPLETION,
            ..Default::default()
        },
        inbound,
        Arc::new(|_| {}),
        stop,
        None,
    )
    .unwrap();
    let addr = NodeAddr::mem(&peer_id);
    let (_, connection) = tokio::try_join!(source.connect(&addr), peer.accept()).unwrap();
    let mut owner =
        DurableOutbox::open(&dir.0, &source, addr, "/state", "session", outbox_config())
            .await
            .unwrap();
    owner
        .handle()
        .enqueue("counter:add#v1", &7u64.to_le_bytes())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while owner.handle().status().last_error.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(owner.handle().status().last_acked, 0);
    assert!(
        rx.try_recv().is_err(),
        "no business frame to unsupported peer"
    );
    owner.close().await.unwrap();
    connection.sender.close();
    source.shutdown().await.unwrap();
    peer.shutdown().await;
}
#[tokio::test]
async fn cancelled_waiter_does_not_interrupt_a_started_atomic_commit() {
    let dir = Scratch::new();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let mut entered = Some(entered_tx);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    // Fn rather than FnMut: locks only coordinate this deliberate transition stall.
    let signals = std::sync::Mutex::new((entered.take(), release_rx));
    let mut owner = TransactionalInbox::open(
        &dir.0,
        "receiver",
        "/state",
        0u64.to_le_bytes().to_vec().into(),
        InboxLimits::default(),
        move |state, m| {
            let mut signals = signals.lock().unwrap();
            if let Some(tx) = signals.0.take() {
                let _ = tx.send(());
                signals.1.recv().unwrap();
            }
            add(state, m)
        },
    )
    .await
    .unwrap();
    let h = owner.handle();
    let pending = tokio::spawn(async move { h.apply(message(1)).await });
    entered_rx.await.unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    release_tx.send(()).unwrap();
    wait_revision(&owner.handle(), 1).await;
    owner.handle().apply(message(1)).await.unwrap();
    assert_eq!(value(&owner.handle().snapshot().state), 7);
    assert_eq!(owner.handle().snapshot().revision, 1);
    owner.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn sigkill_after_receiver_commit_preserves_business_and_dedup_as_one_unit() {
    const ENV: &str = "PARROT_INBOX_KILL_CHILD";
    if let Some(path) = std::env::var_os(ENV) {
        let owner = TransactionalInbox::open(
            PathBuf::from(&path),
            "receiver",
            "/state",
            0u64.to_le_bytes().to_vec().into(),
            InboxLimits::default(),
            add,
        )
        .await
        .unwrap();
        owner.handle().apply(message(1)).await.unwrap();
        std::fs::write(PathBuf::from(path).join("ready"), b"committed").unwrap();
        std::future::pending::<()>().await;
        return;
    }
    let dir = Scratch::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sigkill_after_receiver_commit_preserves_business_and_dedup_as_one_unit",
            "--nocapture",
        ])
        .env(ENV, &dir.0)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !dir.0.join("ready").exists() {
        if let Some(s) = child.try_wait().unwrap() {
            panic!("child ended: {s}");
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child commit stalled");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    child.kill().unwrap();
    let exit = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(exit.signal(), Some(libc::SIGKILL));
    let mut owner = inbox(&dir, "receiver").await;
    owner.handle().apply(message(1)).await.unwrap();
    assert_eq!(value(&owner.handle().snapshot().state), 7);
    assert_eq!(owner.handle().snapshot().revision, 1);
    owner.handle().apply(message(2)).await.unwrap();
    assert_eq!(value(&owner.handle().snapshot().state), 14);
    owner.close().await.unwrap();
}
#[cfg(unix)]
#[test]
fn partial_snapshot_write_never_publishes_half_a_business_dedup_transaction() {
    const ENV: &str = "PARROT_INBOX_IO_CHILD";
    if let Some(path) = std::env::var_os(ENV) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut owner = TransactionalInbox::open(
                    PathBuf::from(path),
                    "receiver",
                    "/state",
                    0u64.to_le_bytes().to_vec().into(),
                    InboxLimits::default(),
                    |s, m| {
                        if m.type_key == "large" {
                            Ok(vec![0; 4096].into())
                        } else {
                            add(s, m)
                        }
                    },
                )
                .await
                .unwrap();
                owner.handle().apply(message(1)).await.unwrap();
                unsafe {
                    assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
                    let limit = libc::rlimit {
                        rlim_cur: 512,
                        rlim_max: 512,
                    };
                    assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
                }
                let mut m = message(2);
                m.type_key = "large".into();
                assert!(matches!(
                    owner.handle().apply(m).await,
                    Err(InboxError::Storage(_))
                ));
                assert!(owner.close().await.is_err());
                assert!(owner.handle().snapshot().closed);
            });
        return;
    }
    let dir = Scratch::new();
    probe(
        "partial_snapshot_write_never_publishes_half_a_business_dedup_transaction",
        ENV,
        &dir.0,
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut owner = inbox(&dir, "receiver").await;
            assert_eq!(value(&owner.handle().snapshot().state), 7);
            assert_eq!(owner.handle().snapshot().revision, 1);
            owner.handle().apply(message(2)).await.unwrap();
            assert_eq!(value(&owner.handle().snapshot().state), 14);
            owner.close().await.unwrap();
        });
}
#[cfg(unix)]
fn probe(test: &str, key: &str, dir: &std::path::Path) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(key, dir)
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
            assert!(output.status.success(), "{test}: {}", output.status);
            return;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child {test} stalled");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[tokio::test]
async fn persisted_transactions_match_an_independent_stream_model_across_repeated_restarts() {
    let dir = Scratch::new();
    let mut owner = inbox(&dir, "receiver").await;
    let mut model = std::collections::BTreeMap::<String, u64>::new();
    let mut committed = 0u64;
    let mut seed = 0xabcddcbau64;
    for step in 0..256 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let session = format!("stream{}", (seed >> 16) % 4);
        let watermark = model.get(&session).copied().unwrap_or(0);
        let seq = match (seed >> 32) % 3 {
            0 => watermark.max(1),
            1 => watermark + 1,
            _ => watermark + 2,
        };
        let mut m = message(seq);
        m.session = session.clone();
        let result = owner.handle().apply(m).await;
        if seq > watermark + 1 {
            assert!(matches!(result, Err(InboxError::Gap { .. })));
        } else {
            result.unwrap();
            if seq > watermark {
                model.insert(session, seq);
                committed += 1;
            }
        }
        assert_eq!(value(&owner.handle().snapshot().state), committed * 7);
        assert_eq!(owner.handle().snapshot().revision, committed);
        if step % 32 == 31 {
            owner.close().await.unwrap();
            owner = inbox(&dir, "receiver").await;
            assert_eq!(owner.handle().snapshot().revision, committed);
            assert_eq!(value(&owner.handle().snapshot().state), committed * 7);
        }
    }
    owner.close().await.unwrap();
}
#[cfg(unix)]
#[test]
fn special_snapshot_paths_fail_without_following_symlinks_or_blocking() {
    const ENV: &str = "PARROT_INBOX_FILES_CHILD";
    if let Some(path) = std::env::var_os(ENV) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let dir = Scratch(PathBuf::from(path));
                let mut owner = inbox(&dir, "receiver").await;
                owner.close().await.unwrap();
                let main = dir.0.join("inbox.state");
                let saved = dir.0.join("saved");
                std::fs::rename(&main, &saved).unwrap();
                std::os::unix::fs::symlink(&saved, &main).unwrap();
                assert!(TransactionalInbox::open(
                    &dir.0,
                    "receiver",
                    "/state",
                    Bytes::new(),
                    InboxLimits::default(),
                    add
                )
                .await
                .is_err());
                std::fs::remove_file(&main).unwrap();
                let name = std::ffi::CString::new(main.as_os_str().as_encoded_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                assert!(TransactionalInbox::open(
                    &dir.0,
                    "receiver",
                    "/state",
                    Bytes::new(),
                    InboxLimits::default(),
                    add
                )
                .await
                .is_err());
            });
        return;
    }
    let dir = Scratch::new();
    probe(
        "special_snapshot_paths_fail_without_following_symlinks_or_blocking",
        ENV,
        &dir.0,
    );
}
#[tokio::test]
async fn command_and_snapshot_budgets_reject_without_publishing_a_new_revision() {
    let dir = Scratch::new();
    let invalid = InboxLimits {
        command_capacity: 0,
        ..Default::default()
    };
    assert!(TransactionalInbox::open(
        dir.0.join("invalid"),
        "receiver",
        "/state",
        Bytes::new(),
        invalid,
        add
    )
    .await
    .is_err());
    assert!(!dir.0.join("invalid").exists());
    let mut owner = TransactionalInbox::open(
        &dir.0,
        "receiver",
        "/state",
        0u64.to_le_bytes().to_vec().into(),
        InboxLimits {
            command_bytes: 1,
            ..Default::default()
        },
        add,
    )
    .await
    .unwrap();
    assert!(matches!(
        owner.handle().apply(message(1)).await,
        Err(InboxError::Capacity)
    ));
    owner.close().await.unwrap();
    let before = std::fs::metadata(dir.0.join("inbox.state")).unwrap().len() as usize;
    let mut owner = TransactionalInbox::open(
        &dir.0,
        "receiver",
        "/state",
        Bytes::new(),
        InboxLimits {
            max_state_bytes: 8,
            max_snapshot_bytes: before + 1,
            ..Default::default()
        },
        add,
    )
    .await
    .unwrap();
    assert!(matches!(
        owner.handle().apply(message(1)).await,
        Err(InboxError::Capacity)
    ));
    assert_eq!(owner.handle().snapshot().revision, 0);
    owner.close().await.unwrap();
    assert_eq!(
        std::fs::metadata(dir.0.join("inbox.state")).unwrap().len() as usize,
        before
    );
}

#[tokio::test]
async fn large_valid_stream_history_is_recoverable_under_its_configured_snapshot_budget() {
    // Independently build a valid version-1 checkpoint representing 300,000
    // committed streams. Raising configured limits must not let writes outgrow a
    // hidden decoder value limit and make an otherwise valid inbox unrecoverable.
    #[derive(serde::Serialize)]
    struct Snapshot {
        version: u8,
        node: String,
        path: String,
        revision: u64,
        state: Bytes,
        streams: std::collections::BTreeMap<(String, String), u64>,
    }
    let count = 300_000u64;
    let saved = Snapshot {
        version: 1,
        node: "receiver".into(),
        path: "/state".into(),
        revision: count,
        state: (count * 7).to_le_bytes().to_vec().into(),
        streams: (0..count)
            .map(|n| (("s".to_owned(), n.to_string()), 1))
            .collect(),
    };
    let body = bincode::serde::encode_to_vec(&saved, bincode::config::standard()).unwrap();
    let mut crc = u32::MAX;
    for byte in &body {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb88320 } else { 0 };
        }
    }
    crc = !crc;
    let dir = Scratch::new();
    let mut bytes = b"PINBOX1\0".to_vec();
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes.extend_from_slice(&body);
    assert!(bytes.len() < 8 * 1024 * 1024);
    std::fs::write(dir.0.join("inbox.state"), bytes).unwrap();
    let limits = InboxLimits {
        max_streams: count as usize,
        max_stream_bytes: 4 * 1024 * 1024,
        max_snapshot_bytes: 8 * 1024 * 1024,
        ..Default::default()
    };
    let mut owner =
        TransactionalInbox::open(&dir.0, "receiver", "/state", Bytes::new(), limits, add)
            .await
            .expect("valid checkpoints within configured budgets must recover");
    assert_eq!(owner.handle().snapshot().streams, count as usize);
    assert_eq!(owner.handle().snapshot().revision, count);
    let mut duplicate = message(1);
    duplicate.source = "s".into();
    duplicate.session = "0".into();
    owner.handle().apply(duplicate.clone()).await.unwrap();
    assert_eq!(value(&owner.handle().snapshot().state), count * 7);
    duplicate.sequence = 2;
    owner.handle().apply(duplicate.clone()).await.unwrap();
    assert_eq!(owner.handle().snapshot().revision, count + 1);
    assert_eq!(value(&owner.handle().snapshot().state), (count + 1) * 7);
    owner.close().await.unwrap();
    let committed = std::fs::read(dir.0.join("inbox.state")).unwrap();
    assert!(TransactionalInbox::open(
        &dir.0,
        "receiver",
        "/state",
        Bytes::new(),
        InboxLimits::default(),
        add,
    )
    .await
    .is_err());
    assert_eq!(std::fs::read(dir.0.join("inbox.state")).unwrap(), committed);
    let mut reopened =
        TransactionalInbox::open(&dir.0, "receiver", "/state", Bytes::new(), limits, add)
            .await
            .expect("a fresh production commit must also recover");
    reopened.handle().apply(duplicate).await.unwrap();
    assert_eq!(reopened.handle().snapshot().revision, count + 1);
    assert_eq!(value(&reopened.handle().snapshot().state), (count + 1) * 7);
    reopened.close().await.unwrap();
}
