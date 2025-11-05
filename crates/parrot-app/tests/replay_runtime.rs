use parrot::thread::{
    config::ThreadActorConfig, context::ThreadContext, system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::debug::replay::{
    record_sink, FileSink, JsonlRecord, RecordSink, ReplayDirection, ReplayError, ReplayLog,
    ReplayOptions,
};
use parrot_remote::{codec_registry::CodecRegistry, LocalLookup};
use std::sync::Arc;

#[derive(serde::Serialize, serde::Deserialize)]
struct Delta(i64);
#[derive(serde::Serialize, serde::Deserialize)]
struct Sum(i64);
#[derive(serde::Serialize, serde::Deserialize)]
struct Huge(Vec<u8>);
macro_rules! codec {
    ($ty:ty,$key:literal) => { parrot_api::message::inventory::submit! {
        parrot_api::message::CodecRegistration {
            type_key:$key, type_id:std::any::TypeId::of::<$ty>(),
            encode: |msg| parrot_api::message::serde_remote_serialize(msg.downcast_ref::<$ty>().ok_or("wrong type")?),
            decode: |bytes| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<$ty>(bytes)?) as BoxedMessage),
        }
    } };
}
codec!(Delta, "bin:replay.Delta");
codec!(Sum, "bin:replay.Sum");
codec!(Huge, "bin:replay.Huge");
struct BrokenCodec;
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key:"bin:replay.Broken",type_id:std::any::TypeId::of::<BrokenCodec>(),
        encode: |_| Ok(vec![]),decode: |_| panic!("deliberately broken codec"),
    }
}
struct Counter(i64);
impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let delta = msg.downcast::<Delta>().unwrap().0;
            if delta < 0 {
                return Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "negative delta rejected".into(),
                ));
            }
            self.0 += delta;
            if delta == 99 {
                Ok(Box::new(Huge(vec![1; 1024])) as BoxedMessage)
            } else {
                Ok(Box::new(Sum(self.0)) as BoxedMessage)
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
struct Lookup(BoxedActorRef);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/counter").then(|| self.0.clone_boxed())
    }
}
async fn engine() -> (Arc<ThreadActorSystem>, Lookup) {
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_at(
            Counter(0),
            "/counter",
            None,
            ThreadActorConfig {
                supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    (system, Lookup(Box::new(actor)))
}
async fn value(lookup: &Lookup) -> i64 {
    lookup
        .0
        .send(Box::new(Delta(0)))
        .await
        .unwrap()
        .downcast::<Sum>()
        .unwrap()
        .0
}
fn record(seq: u64, dir: &str, ft: u8, delta: i64) -> JsonlRecord {
    let msg = Box::new(Delta(delta)) as BoxedMessage;
    let (key, payload) = CodecRegistry::global().encode_outgoing(&msg).unwrap();
    record_sink(seq, 100 - seq, dir, ft, "/counter", &key, &payload)
}
#[tokio::test]
async fn file_replay_executes_real_actors_and_repeats_the_same_reply_trajectory() {
    let path = std::env::temp_dir().join(format!(
        "parrot-real-replay-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let records = vec![
        record(0, "rx", 0x13, 2),
        record(1, "rx", 0x10, 3),
        record(2, "rx", 0x11, 900),
        record(3, "tx", 0x10, 700),
        record(4, "rx", 0x10, 4),
    ];
    let mut file = FileSink::open(&path).unwrap();
    for r in &records {
        file.write(r).unwrap();
    }
    file.finish().unwrap();
    let log = ReplayLog::load(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let mut previous = None;
    for _ in 0..3 {
        let (system, lookup) = engine().await;
        let report = log
            .replay_local(&lookup, ReplayOptions::default())
            .await
            .unwrap();
        assert_eq!(report.delivered, 3);
        assert_eq!(value(&lookup).await, 9);
        let values: Vec<i64> = report
            .replies
            .iter()
            .map(|r| {
                CodecRegistry::global()
                    .decode_incoming(&r.type_key, &r.payload)
                    .unwrap()
                    .downcast::<Sum>()
                    .unwrap()
                    .0
            })
            .collect();
        assert_eq!(values, vec![5, 9]);
        assert_eq!(
            report
                .replies
                .iter()
                .map(|r| (r.seq, r.cid))
                .collect::<Vec<_>>(),
            vec![(1, 99), (4, 96)]
        );
        if let Some(prior) = previous.as_ref() {
            assert_eq!(&report, prior);
        }
        previous = Some(report);
        system.shutdown_internal().await.unwrap();
    }
}
#[tokio::test]
async fn tx_selection_never_applies_the_recorded_rx_copy() {
    let (system, lookup) = engine().await;
    let log = ReplayLog {
        records: vec![record(0, "tx", 0x10, 7), record(1, "rx", 0x10, 7)],
    };
    let report = log
        .replay_local(
            &lookup,
            ReplayOptions {
                direction: ReplayDirection::Tx,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(report.delivered, 1);
    assert_eq!(value(&lookup).await, 7);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn preflight_codec_and_path_failures_have_no_actor_side_effects() {
    let (system, lookup) = engine().await;
    for kind in 0..5 {
        let mut bad = record(1, "rx", 0x10, 10);
        match kind {
            0 => bad.type_key = "bin:missing".into(),
            1 => bad.path = "/missing".into(),
            2 => bad.path = "parrot://outside/counter".into(),
            3 => bad.type_key = "bin:replay.Broken".into(),
            _ => bad.payload_hex = "ff".into(),
        }
        let log = ReplayLog {
            records: vec![record(0, "rx", 0x10, 5), bad],
        };
        assert!(matches!(
            log.replay_local(&lookup, ReplayOptions::default()).await,
            Err(ReplayError::Delivery {
                seq: 1,
                completed: 0,
                ..
            })
        ));
        assert_eq!(value(&lookup).await, 0);
    }
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn business_failure_reports_progress_and_stops_following_requests() {
    let (system, lookup) = engine().await;
    let log = ReplayLog {
        records: vec![
            record(0, "rx", 0x10, 2),
            record(1, "rx", 0x10, -1),
            record(2, "rx", 0x10, 100),
        ],
    };
    assert!(matches!(
        log.replay_local(&lookup, ReplayOptions::default()).await,
        Err(ReplayError::Delivery {
            seq: 1,
            completed: 1,
            ..
        })
    ));
    assert_eq!(value(&lookup).await, 2);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn reply_budget_error_preserves_the_fact_that_the_actor_already_executed() {
    let (system, lookup) = engine().await;
    let log = ReplayLog {
        records: vec![record(0, "rx", 0x10, 99), record(1, "rx", 0x10, 100)],
    };
    let mut options = ReplayOptions::default();
    options.limits.max_payload_bytes = 8;
    assert!(matches!(
        log.replay_local(&lookup, options).await,
        Err(ReplayError::Delivery {
            seq: 0,
            completed: 1,
            ..
        })
    ));
    assert_eq!(value(&lookup).await, 99);
    system.shutdown_internal().await.unwrap();
}
struct NeverLookup;
#[async_trait::async_trait]
impl LocalLookup for NeverLookup {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        std::future::pending().await
    }
}

struct SlowCounter {
    effects: Arc<std::sync::atomic::AtomicI64>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl Actor for SlowCounter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let delta = msg.downcast::<Delta>().unwrap().0;
            let sum = self
                .effects
                .fetch_add(delta, std::sync::atomic::Ordering::SeqCst)
                + delta;
            self.entered.notify_one();
            self.release.notified().await;
            Ok(Box::new(Sum(sum)) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
#[tokio::test]
async fn ask_timeout_does_not_claim_rollback_or_deliver_the_next_request() {
    let system = ThreadActorSystem::shared(Default::default());
    let effects = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let actor = system
        .spawn_at(
            SlowCounter {
                effects: effects.clone(),
                entered: entered.clone(),
                release: release.clone(),
            },
            "/counter",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    let lookup = Lookup(Box::new(actor));
    let task = tokio::spawn(async move {
        ReplayLog {
            records: vec![record(0, "rx", 0x10, 2), record(1, "rx", 0x10, 10)],
        }
        .replay_local(
            &lookup,
            ReplayOptions {
                call_timeout: std::time::Duration::from_secs(2),
                ..Default::default()
            },
        )
        .await
    });
    // Observe an actual business effect while the first response is withheld.
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await;
    let result = task.await.unwrap();
    release.notify_one();
    system.shutdown_internal().await.unwrap();
    assert!(started.is_ok(), "actor never received the first request");
    assert!(matches!(
        result,
        Err(ReplayError::Delivery {
            seq: 0,
            completed: 0,
            ..
        })
    ));
    assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn tell_to_a_stopped_real_actor_reports_failed_admission() {
    let (system, lookup) = engine().await;
    system.stop_actor("/counter").await.unwrap();
    let result = ReplayLog {
        records: vec![record(0, "rx", 0x13, 2), record(1, "rx", 0x10, 10)],
    }
    .replay_local(&lookup, ReplayOptions::default())
    .await;
    system.shutdown_internal().await.unwrap();
    assert!(matches!(
        result,
        Err(ReplayError::Delivery {
            seq: 0,
            completed: 0,
            ..
        })
    ));
}
#[tokio::test(start_paused = true)]
async fn lookup_deadlines_and_invalid_timeouts_fail_before_delivery() {
    let log = ReplayLog {
        records: vec![record(0, "rx", 0x10, 1)],
    };
    let options = ReplayOptions {
        call_timeout: std::time::Duration::from_millis(10),
        ..Default::default()
    };
    let before = tokio::time::Instant::now();
    assert!(matches!(
        log.replay_local(&NeverLookup, options.clone()).await,
        Err(ReplayError::Delivery {
            seq: 0,
            completed: 0,
            ..
        })
    ));
    assert_eq!(tokio::time::Instant::now() - before, options.call_timeout);
    for timeout in [std::time::Duration::ZERO, std::time::Duration::MAX] {
        assert!(matches!(
            log.replay_local(
                &NeverLookup,
                ReplayOptions {
                    call_timeout: timeout,
                    ..options.clone()
                }
            )
            .await,
            Err(ReplayError::InvalidTimeout)
        ));
    }
}

async fn recorded_transport(scheme: &str, direct: bool) {
    use parrot_api::address::ActorRef;
    use parrot_app::debug::Recording;
    use parrot_remote::{frame::frame_type, NodeAddr, RemoteActorSystem, RemoteConfig};
    use std::time::Duration;
    let name = format!("capture-{scheme}-{direct}");
    let path = std::env::temp_dir().join(format!("parrot-{name}-{}.jsonl", std::process::id()));
    let recording = Recording::start(&path, Default::default(), Default::default()).unwrap();
    let (system, lookup) = engine().await;
    let lookup = Arc::new(lookup);
    let config =
        |id: String| match scheme {
            "tcp" => RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development(),
            "quic" => RemoteConfig::quic(id, Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development(),
            "mem" => RemoteConfig::mem(id),
            _ => unreachable!(),
        };
    let server_name = format!("{name}-server");
    let client_name = format!("{name}-client");
    let server = RemoteActorSystem::new(
        config(server_name.clone()).with_capture(recording.capture()),
        lookup.clone(),
    )
    .unwrap();
    let client = RemoteActorSystem::new(config(client_name.clone()), lookup.clone()).unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    if direct {
        client.connect_mem_pair(&server).await.unwrap();
    } else {
        let address = match scheme {
            "tcp" => NodeAddr::tcp(&server_name, server.local_addr().unwrap()),
            "quic" => NodeAddr::quic(&server_name, server.local_addr().unwrap()),
            "mem" => NodeAddr::mem(&server_name),
            _ => unreachable!(),
        };
        client.connect(&address).await.unwrap();
    }
    let remote = client
        .remote_ref(&format!("parrot://{server_name}/counter"))
        .unwrap();
    remote.deliver(Box::new(Delta(2))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while value(&lookup).await != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut original = Vec::new();
    for delta in [3, 4] {
        let reply = remote
            .send_with_timeout(Box::new(Delta(delta)), Some(Duration::from_secs(5)))
            .await
            .unwrap();
        original.push(reply.downcast::<Sum>().unwrap().0);
    }
    assert_eq!(original, [5, 9]);
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    system.shutdown_internal().await.unwrap();
    let receipt = tokio::task::spawn_blocking(move || recording.finish())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.stats.lost, 0);
    let log = ReplayLog::load_capture(&path).unwrap();
    assert_eq!(receipt.written, log.records.len());
    assert!(log
        .records
        .iter()
        .any(|r| r.dir == "rx" && r.frame_type == frame_type::HANDSHAKE));
    assert!(log
        .records
        .iter()
        .any(|r| r.dir == "tx" && r.frame_type == frame_type::HANDSHAKE_ACK));
    let requests: Vec<_> = log
        .records
        .iter()
        .filter(|r| r.dir == "rx" && matches!(r.frame_type, frame_type::ASK | frame_type::TELL))
        .collect();
    assert_eq!(requests.len(), 3);
    for record in requests {
        let wire = record.wire.as_ref().unwrap();
        assert_eq!(wire.local_node, server_name);
        assert_eq!(wire.peer_node.as_deref(), Some(client_name.as_str()));
        assert_eq!(wire.scheme, scheme);
        assert_ne!(wire.connection, 0);
    }
    let (fresh, lookup) = engine().await;
    let replay = log
        .replay_local(
            &lookup,
            ReplayOptions {
                target_node: Some(server_name),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(replay.delivered, 3);
    assert_eq!(value(&lookup).await, 9);
    let replies: Vec<_> = replay
        .replies
        .iter()
        .map(|r| {
            CodecRegistry::global()
                .decode_incoming(&r.type_key, &r.payload)
                .unwrap()
                .downcast::<Sum>()
                .unwrap()
                .0
        })
        .collect();
    assert_eq!(replies, original);
    let observed: Vec<_> = log
        .records
        .iter()
        .filter(|r| r.dir == "tx" && r.frame_type == frame_type::REPLY)
        .collect();
    assert_eq!(observed.len(), replay.replies.len());
    for (wire, result) in observed.iter().zip(&replay.replies) {
        assert_eq!(wire.cid, result.cid);
        assert_eq!(wire.payload_bytes().unwrap(), result.payload);
    }
    fresh.shutdown_internal().await.unwrap();
    std::fs::remove_file(path).unwrap();
}
#[tokio::test]
async fn automatic_tcp_capture_replays_into_fresh_actor() {
    recorded_transport("tcp", false).await;
}
#[tokio::test]
async fn automatic_quic_capture_replays_into_fresh_actor() {
    recorded_transport("quic", false).await;
}
#[tokio::test]
async fn automatic_memory_capture_replays_into_fresh_actor() {
    recorded_transport("mem", false).await;
}
#[tokio::test]
async fn automatic_direct_memory_capture_replays_into_fresh_actor() {
    recorded_transport("mem", true).await;
}

#[tokio::test]
async fn actual_tcp_completion_receipts_match_replay_on_a_fresh_actor() {
    use parrot_app::debug::Recording;
    use parrot_remote::{
        frame::{flags, frame_type, TELL_COMPLETION_KEY},
        NodeAddr, RemoteActorSystem, RemoteConfig,
    };
    let path = std::env::temp_dir().join(format!(
        "parrot-completion-capture-{}.jsonl",
        std::process::id()
    ));
    let recording = Recording::start(&path, Default::default(), Default::default()).unwrap();
    let (system, lookup) = engine().await;
    let lookup = Arc::new(lookup);
    let target = RemoteActorSystem::new(
        RemoteConfig::tcp(
            "completion-capture-target",
            Some("127.0.0.1:0".parse().unwrap()),
        )
        .with_capture(recording.capture()),
        lookup.clone(),
    )
    .unwrap();
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp("completion-capture-source", None),
        lookup.clone(),
    )
    .unwrap();
    target.start().await.unwrap();
    source.start().await.unwrap();
    source
        .connect(&NodeAddr::tcp(
            "completion-capture-target",
            target.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let remote = source
        .remote_ref("parrot://completion-capture-target/counter")
        .unwrap();
    let mut total = 0;
    for delta in [2, 3, 4] {
        remote
            .deliver_with_ack(Box::new(Delta(delta)), std::time::Duration::from_secs(3))
            .await
            .unwrap();
        total += delta;
        assert_eq!(value(&lookup).await, total);
    }
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
    system.shutdown_internal().await.unwrap();
    tokio::task::spawn_blocking(move || recording.finish())
        .await
        .unwrap()
        .unwrap();
    let log = ReplayLog::load_capture(&path).unwrap();
    let requests: Vec<_> = log
        .records
        .iter()
        .filter(|r| r.dir == "rx" && r.frame_type == frame_type::TELL)
        .collect();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_ne!(request.cid, 0);
        assert_ne!(request.wire.as_ref().unwrap().flags & flags::TELL_ACK, 0);
        assert_eq!(request.wire.as_ref().unwrap().wire_seq, 0);
    }
    let observed: Vec<_> = log
        .records
        .iter()
        .filter(|r| r.dir == "tx" && r.frame_type == frame_type::REPLY)
        .collect();
    assert_eq!(observed.len(), 3);
    let (fresh, lookup) = engine().await;
    let replay = log
        .replay_local(
            &lookup,
            ReplayOptions {
                target_node: Some("completion-capture-target".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(replay.delivered, 3);
    assert_eq!(value(&lookup).await, 9);
    assert_eq!(replay.replies.len(), 3);
    for (actual, replayed) in observed.iter().zip(&replay.replies) {
        assert_eq!(actual.cid, replayed.cid);
        assert_eq!(actual.type_key, TELL_COMPLETION_KEY);
        assert_eq!(replayed.type_key, TELL_COMPLETION_KEY);
        assert!(actual.payload_bytes().unwrap().is_empty());
        assert!(replayed.payload.is_empty());
    }
    fresh.shutdown_internal().await.unwrap();
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn completion_replay_propagates_business_failure_instead_of_reporting_admission() {
    let (system, lookup) = engine().await;
    let mut failed = record(0, "rx", parrot_remote::frame::frame_type::TELL, -1);
    failed.cid = 17;
    failed.wire = wire_record(0, 0).wire;
    failed.wire.as_mut().unwrap().flags = parrot_remote::frame::flags::TELL_ACK;
    let error = ReplayLog {
        records: vec![failed],
    }
    .replay_local(&lookup, Default::default())
    .await
    .unwrap_err();
    assert!(matches!(error, ReplayError::Delivery { completed: 0, .. }));
    assert_eq!(value(&lookup).await, 0);
    system.shutdown_internal().await.unwrap();
}

fn wire_record(seq: u64, delta: i64) -> JsonlRecord {
    let mut record = record(seq, "rx", 0x10, delta);
    let mut bytes = vec![0; 4];
    bytes.extend(record.payload_bytes().unwrap());
    record.payload_hex = record_sink(0, 0, "rx", 0x10, "", "", &bytes).payload_hex;
    record.wire = Some(parrot_remote::capture::CapturedWire {
        connection: 1,
        local_node: "source".into(),
        peer_node: Some("client".into()),
        scheme: "tcp".into(),
        elapsed_micros: seq,
        version: 1,
        flags: 0,
        hop_count: 0,
        hop_limit: 16,
        wire_seq: 0,
    });
    record
}
#[tokio::test]
async fn unsupported_capture_flags_and_versions_fail_before_business_effects() {
    for (version, flags) in [(2, 0), (1, 1), (1, 32), (1, 0x8000)] {
        let (system, lookup) = engine().await;
        let mut invalid = wire_record(1, 4);
        let wire = invalid.wire.as_mut().unwrap();
        wire.version = version;
        wire.flags = flags;
        let log = ReplayLog {
            records: vec![wire_record(0, 3), invalid],
        };
        assert!(log.replay_local(&lookup, Default::default()).await.is_err());
        assert_eq!(value(&lookup).await, 0);
        system.shutdown_internal().await.unwrap();
    }
}
#[tokio::test]
async fn multiple_capture_nodes_and_malformed_reply_prefix_fail_before_effects() {
    for invalid_hex in [None, Some("0000"), Some("ffffffff"), Some("01000000ff")] {
        let (system, lookup) = engine().await;
        let mut invalid = wire_record(1, 4);
        if let Some(hex) = invalid_hex {
            invalid.payload_hex = hex.into();
        } else {
            invalid.wire.as_mut().unwrap().local_node = "other".into();
        }
        let log = ReplayLog {
            records: vec![wire_record(0, 3), invalid],
        };
        assert!(log.replay_local(&lookup, Default::default()).await.is_err());
        assert_eq!(value(&lookup).await, 0);
        system.shutdown_internal().await.unwrap();
    }
}
#[tokio::test]
async fn captured_ask_strips_nonempty_reply_path_and_accepts_supported_flags() {
    let (system, lookup) = engine().await;
    let payload = record(1, "rx", 0x10, 4).payload_bytes().unwrap();
    let mut record = wire_record(0, 4);
    let frame = parrot_remote::frame::Frame::ask(
        7,
        "/counter",
        "bin:replay.Delta",
        payload.into(),
        Some("/original/reply"),
    );
    record.payload_hex = record_sink(0, 0, "rx", 0x10, "", "", &frame.payload).payload_hex;
    record.wire.as_mut().unwrap().flags =
        parrot_remote::frame::flags::TRACING | parrot_remote::frame::flags::URGENT;
    let log = ReplayLog {
        records: vec![record],
    };
    assert_eq!(
        log.replay_local(&lookup, Default::default())
            .await
            .unwrap()
            .delivered,
        1
    );
    assert_eq!(value(&lookup).await, 4);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn uri_mapping_is_explicit_exact_and_preflighted_for_the_whole_log() {
    for target in [None, Some("different")] {
        let (system, lookup) = engine().await;
        let mut mapped = wire_record(1, 4);
        mapped.path = "parrot://source/counter".into();
        let log = ReplayLog {
            records: vec![wire_record(0, 3), mapped],
        };
        assert!(log
            .replay_local(
                &lookup,
                ReplayOptions {
                    target_node: target.map(String::from),
                    ..Default::default()
                }
            )
            .await
            .is_err());
        assert_eq!(value(&lookup).await, 0);
        system.shutdown_internal().await.unwrap();
    }
}

fn batched_record() -> JsonlRecord {
    use parrot_remote::frame::Frame;
    let frames: Vec<_> = [2, 3, 4]
        .into_iter()
        .enumerate()
        .map(|(index, delta)| {
            let msg = Box::new(Delta(delta)) as BoxedMessage;
            let (key, payload) = CodecRegistry::global().encode_outgoing(&msg).unwrap();
            Frame::ask(
                40 + index as u64,
                "/counter",
                &key,
                payload.into(),
                Some("/replies"),
            )
        })
        .collect();
    let batch = Frame::batch(&frames).unwrap();
    let mut record = record_sink(
        0,
        0,
        "rx",
        batch.header.frame_type,
        &batch.path,
        &batch.type_key,
        &batch.payload,
    );
    let mut wire = wire_record(0, 0).wire.unwrap();
    wire.flags = batch.header.flags;
    record.wire = Some(wire);
    record
}
#[tokio::test]
async fn batch_replay_preserves_member_correlation_and_reply_trajectory() {
    let (system, lookup) = engine().await;
    let log = ReplayLog {
        records: vec![batched_record()],
    };
    let report = log.replay_local(&lookup, Default::default()).await.unwrap();
    assert_eq!(report.delivered, 3);
    for (index, (reply, expected)) in report.replies.iter().zip([2, 5, 9]).enumerate() {
        assert_eq!(
            (reply.seq, reply.cid, reply.batch_index),
            (0, 40 + index as u64, Some(index))
        );
        let sum = CodecRegistry::global()
            .decode_incoming(&reply.type_key, &reply.payload)
            .unwrap()
            .downcast::<Sum>()
            .unwrap()
            .0;
        assert_eq!(sum, expected);
    }
    assert_eq!(value(&lookup).await, 9);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn truncated_batch_and_expanded_count_budget_have_no_business_effects() {
    for truncate in [false, true] {
        let (system, lookup) = engine().await;
        let mut record = batched_record();
        let mut options = ReplayOptions::default();
        if truncate {
            record.payload_hex.truncate(record.payload_hex.len() - 2);
        } else {
            options.limits.max_records = 2;
        }
        let log = ReplayLog {
            records: vec![record],
        };
        assert!(log.replay_local(&lookup, options).await.is_err());
        assert_eq!(value(&lookup).await, 0);
        system.shutdown_internal().await.unwrap();
    }
}
#[tokio::test]
async fn real_batch_capture_replays_each_business_message_once() {
    use parrot_app::debug::Recording;
    use parrot_remote::{frame::Frame, RemoteActorSystem, RemoteConfig};
    let path =
        std::env::temp_dir().join(format!("parrot-batch-capture-{}.jsonl", std::process::id()));
    let recording = Recording::start(&path, Default::default(), Default::default()).unwrap();
    let (system, lookup) = engine().await;
    let lookup = Arc::new(lookup);
    let a = RemoteActorSystem::new(RemoteConfig::mem("batch-source"), lookup.clone()).unwrap();
    let b = RemoteActorSystem::new(
        RemoteConfig::mem("batch-target").with_capture(recording.capture()),
        lookup.clone(),
    )
    .unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();
    let frames: Vec<_> = [2, 3, 4]
        .into_iter()
        .map(|delta| {
            let (key, payload) = CodecRegistry::global()
                .encode_outgoing(&(Box::new(Delta(delta)) as BoxedMessage))
                .unwrap();
            Frame::tell("/counter", &key, payload.into())
        })
        .collect();
    a.links_snapshot().await[0]
        .1
        .send(Frame::batch(&frames).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while value(&lookup).await != 9 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    system.shutdown_internal().await.unwrap();
    tokio::task::spawn_blocking(move || recording.finish())
        .await
        .unwrap()
        .unwrap();
    let log = ReplayLog::load_capture(&path).unwrap();
    let (fresh, lookup) = engine().await;
    assert_eq!(
        log.replay_local(&lookup, Default::default())
            .await
            .unwrap()
            .delivered,
        3
    );
    assert_eq!(value(&lookup).await, 9);
    fresh.shutdown_internal().await.unwrap();
    std::fs::remove_file(path).unwrap();
}

struct AnyPath(BoxedActorRef);
#[async_trait::async_trait]
impl LocalLookup for AnyPath {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        Some(self.0.clone_boxed())
    }
}
#[tokio::test]
async fn batch_expansion_cannot_bypass_the_total_replay_byte_budget() {
    use parrot_remote::frame::Frame;
    let (system, lookup) = engine().await;
    let path = format!("/{}", "x".repeat(1000));
    let (key, payload) = CodecRegistry::global()
        .encode_outgoing(&(Box::new(Delta(1)) as BoxedMessage))
        .unwrap();
    let frame = Frame::ask(1, &path, &key, payload.into(), None);
    let batch = Frame::batch(&vec![frame; 1024]).unwrap();
    let mut record = record_sink(
        0,
        0,
        "rx",
        batch.header.frame_type,
        &batch.path,
        &batch.type_key,
        &batch.payload,
    );
    let mut wire = wire_record(0, 0).wire.unwrap();
    wire.flags = batch.header.flags;
    record.wire = Some(wire);
    let mut options = ReplayOptions::default();
    options.limits.max_file_bytes = serde_json::to_vec(&record).unwrap().len() + 1;
    let log = ReplayLog {
        records: vec![record],
    };
    let error = log
        .replay_local(&AnyPath(lookup.0.clone_boxed()), options)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("expanded request byte budget"),
        "{error}"
    );
    assert_eq!(value(&lookup).await, 0);
    system.shutdown_internal().await.unwrap();
}
