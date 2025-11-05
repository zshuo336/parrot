use super::*;
use crate::admin_v2::{AdminCommandV2, AdminReplyV2, ComponentDeploy, ComponentExecutor};
use crate::metrics::{field, MeasuredMetrics};
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}
struct Unmeasured;
#[async_trait::async_trait]
impl ComponentExecutor for Unmeasured {
    async fn deploy(&self, _: u64, _: &ComponentDeploy) -> AdminReplyV2 {
        unreachable!()
    }
    async fn drain(&self, _: u64, _: &str, _: u64) -> AdminReplyV2 {
        unreachable!()
    }
    async fn stop(&self, _: u64, _: &str) -> AdminReplyV2 {
        unreachable!()
    }
    async fn status(&self, _: u64, _: &str) -> AdminReplyV2 {
        unreachable!()
    }
}

#[tokio::test]
async fn unimplemented_executor_does_not_invent_successful_zero_snapshot() {
    assert!(matches!(
        Unmeasured.metrics(7).await,
        AdminReplyV2::Failed { req_id: 7, .. }
    ));
    let sample = Unmeasured.measured_metrics().await;
    assert_eq!(sample.value(field::ASKS_RX), None);
}

#[test]
fn availability_distinguishes_zero_from_unavailable_and_requires_a_process_unit() {
    let mut sample = MeasuredMetrics::default();
    sample.snapshot.asks_rx = 99;
    assert_eq!(sample.value(field::ASKS_RX), None);
    sample.snapshot.asks_rx = 0;
    sample.available = field::ASKS_RX;
    assert_eq!(sample.value(field::ASKS_RX), Some(0));
    sample.available |= field::PROCESSES;
    assert!(sample.validate().is_err());
    sample.process_kind = "parrot_actors".into();
    assert!(sample.validate().is_ok());
}

#[test]
fn measured_wire_is_additive_and_old_command_discriminants_stay_frozen() {
    assert_eq!(
        crate::admin_v2::encode_admin_cmd_v2(&AdminCommandV2::MetricsReport { req_id: 42 })
            .as_ref(),
        &[3, 4, 42]
    );
    let cmd = crate::admin_v2::encode_admin_cmd_v2(&AdminCommandV2::MetricsReportV2 { req_id: 42 });
    assert_eq!(cmd.as_ref(), &[3, 5, 42]);
    let metrics = MeasuredMetrics {
        available: field::ASKS_RX,
        epoch: "epoch-a".into(),
        ..Default::default()
    };
    let reply = AdminReplyV2::MeasuredMetrics {
        req_id: 42,
        metrics: metrics.clone(),
    };
    let wire = crate::admin_v2::encode_admin_reply_v2(&reply);
    assert_eq!(&wire[..3], &[4, 6, 42]);
    let crate::admin::SysEvent::AdminReplyV2(decoded) =
        crate::admin::decode_sys_event(&wire).unwrap()
    else {
        panic!("wrong reply tag")
    };
    assert_eq!(decoded, reply);
    for n in 0..wire.len() {
        assert!(crate::admin::decode_sys_event(&wire[..n]).is_err());
    }
}

async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn measured_metrics_cover_all_owned_transports_and_keep_epochs_across_reconnects() {
    for mode in ["direct", "mem", "tcp", "quic"] {
        let id = uuid::Uuid::new_v4();
        let make = |suffix: &str| {
            let node = format!("metrics-{id}-{suffix}");
            let mut cfg = match mode {
                "tcp" => RemoteConfig::tcp(node, Some("127.0.0.1:0".parse().unwrap())),
                "quic" => RemoteConfig::quic(node, Some("127.0.0.1:0".parse().unwrap())),
                _ => RemoteConfig::mem(node),
            };
            cfg.security = crate::RemoteSecurity::InsecureDevelopment;
            cfg.heartbeat_enabled = false;
            RemoteActorSystem::new(cfg, Arc::new(Empty)).unwrap()
        };
        let a = make("a");
        let b = make("b");
        a.start().await.unwrap();
        b.start().await.unwrap();
        let connect = || async {
            if mode == "direct" {
                a.connect_mem_pair(&b).await.unwrap();
            } else {
                let addr = match mode {
                    "mem" => NodeAddr::mem(&b.config.node_id),
                    "tcp" => NodeAddr::tcp(&b.config.node_id, b.transport.local_addr().unwrap()),
                    "quic" => NodeAddr::quic(&b.config.node_id, b.transport.local_addr().unwrap()),
                    _ => unreachable!(),
                };
                a.connect(&addr).await.unwrap();
            }
        };
        connect().await;
        let before = a.measured_metrics(&b.config.node_id).await.unwrap();
        assert_eq!(before.value(field::CONNECTIONS), Some(1), "{mode}");
        assert_eq!(before.value(field::ASKS_RX), Some(0));
        assert!(!before.epoch.is_empty());
        assert_eq!(before.value(field::COMPONENTS), None); // no component producer installed
        let frame = Frame::ask(
            913,
            "/missing",
            "unknown:key",
            bytes::Bytes::from_static(b"payload"),
            None,
        );
        a.sender_of(&b.config.node_id)
            .unwrap()
            .send(frame)
            .await
            .unwrap();
        wait_until(|| b.metrics.snapshot().reply_errs == 1).await;
        let after = a.measured_metrics(&b.config.node_id).await.unwrap();
        assert_eq!(after.asks_rx - before.asks_rx, 1);
        assert_eq!(after.replies_tx - before.replies_tx, 1);
        assert_eq!(after.reply_errs - before.reply_errs, 1);
        assert_eq!(after.epoch, before.epoch);
        a.sender_of(&b.config.node_id).unwrap().close();
        wait_until(|| {
            b.metrics.snapshot().connections == 0 && a.metrics.snapshot().connections == 0
        })
        .await;
        connect().await;
        let again = a.measured_metrics(&b.config.node_id).await.unwrap();
        assert_eq!(again.connections, 1);
        assert_eq!(again.handshakes_ok, 2);
        assert_eq!(again.asks_rx, 1);
        assert_eq!(again.epoch, before.epoch);
        a.shutdown().await.unwrap();
        b.shutdown().await.unwrap();
        assert_eq!(a.metrics.snapshot().connections, 0);
        assert_eq!(b.metrics.snapshot().connections, 0);
        let restarted = make("b");
        assert_ne!(restarted.metrics.snapshot().epoch, before.epoch);
        restarted.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn old_connection_without_capability_fails_before_registering_or_sending() {
    let sys = RemoteActorSystem::new(
        RemoteConfig::mem(format!("legacy-metrics-{}", uuid::Uuid::new_v4())),
        Arc::new(Empty),
    )
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let sender = FrameSender::new(tx, "old-peer");
    sys.links
        .write()
        .unwrap()
        .push(("old-peer".into(), sender, Arc::new(NodeStatus::default())));
    let error = sys.measured_metrics("old-peer").await.unwrap_err();
    assert!(error
        .to_string()
        .contains("does not advertise measured metrics"));
    assert_eq!(sys.admin_pending_v2.len(), 0);
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    sys.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn measured_metrics_preserve_network_admin_authorization() {
    let id = uuid::Uuid::new_v4();
    let a = RemoteActorSystem::new(
        RemoteConfig::tcp(format!("reader-{id}"), None),
        Arc::new(Empty),
    )
    .unwrap();
    let b = RemoteActorSystem::new(
        RemoteConfig::tcp(
            format!("private-{id}"),
            Some("127.0.0.1:0".parse().unwrap()),
        ),
        Arc::new(Empty),
    )
    .unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect(&NodeAddr::tcp(
        &b.config.node_id,
        b.transport.local_addr().unwrap(),
    ))
    .await
    .unwrap();
    let error = a.measured_metrics(&b.config.node_id).await.unwrap_err();
    assert!(error.to_string().contains("0x000d"), "{error}");
    assert_eq!(a.admin_pending_v2.len(), 0);
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[test]
fn measured_metrics_matches_independent_frozen_vectors() {
    let document: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../docs/vectors/measured_metrics.json"
    ))
    .unwrap();
    let metrics = MeasuredMetrics {
        available: field::ASKS_RX,
        epoch: "owner-a".into(),
        ..Default::default()
    };
    let encoded = [
        crate::admin_v2::encode_admin_cmd_v2(&AdminCommandV2::MetricsReportV2 { req_id: 42 }),
        crate::admin_v2::encode_admin_reply_v2(&AdminReplyV2::MeasuredMetrics {
            req_id: 42,
            metrics,
        }),
    ];
    for (vector, actual) in document["vectors"].as_array().unwrap().iter().zip(encoded) {
        let hex = vector["payload_hex"].as_str().unwrap();
        let expected: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(actual.as_ref(), expected);
    }
}
