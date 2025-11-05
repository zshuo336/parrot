use super::*;
use parrot_api::types::BoxedMessage;
#[test]
fn measured_rendering_distinguishes_zero_unknown_units_and_component_counts() {
    use parrot_remote::admin_v2::{ComponentStateReport, MetricsSnapshot};
    let mut measured = MeasuredMetrics {
        snapshot: MetricsSnapshot {
            ts_ms: 2000,
            uptime_start_ms: 1000,
            runtime: "runtime\n\u{1b}[31m".into(),
            processes: 2,
            components: 3,
            component_states: vec![ComponentStateReport {
                path: "/instance".into(),
                state: "running".into(),
                version: "v1".into(),
            }],
            ..Default::default()
        },
        available: field::ALL,
        epoch: "owner-a".into(),
        process_kind: "erlang_processes".into(),
    };
    let status = status_line("node", &measured);
    assert!(status.contains("rss=0 B"));
    assert!(status.contains("processes=2 erlang_processes"));
    assert!(status.contains("0h00m01s"));
    assert!(!status.contains('\n') && !status.contains('\u{1b}'));
    let table = metrics_table(&[("node".into(), measured.clone())]);
    assert!(table.contains("components=3"));
    assert!(table.contains("实例状态 (1)"));
    measured.available = 0;
    let unknown = status_line("node", &measured);
    assert!(
        unknown.contains("rss=n/a")
            && unknown.contains("asks=n/a")
            && unknown.contains("processes=n/a")
    );
    let table = metrics_table(&[("node".into(), measured)]);
    assert!(!table.contains("/instance"));
    assert!(table.contains("component states: n/a"));
}
#[test]
fn distinct_wire_identities_initialize_the_real_registry_and_roundtrip() {
    let registry = parrot_remote::codec_registry::CodecRegistry::global();
    let messages: [(&str, BoxedMessage); 3] = [
        ("bin:u:Ping", Box::new(ProbePing(12))),
        ("bin:u:Pong", Box::new(ProbePong(15))),
        ("bin:parrot.interop.Echoed#v1", Box::new(ProbeEchoed(12))),
    ];
    for (key, message) in messages {
        assert_eq!(registry.key_of(message.as_ref().type_id()), Some(key));
        let codec = registry.get(key).unwrap();
        let bytes = (codec.encode)(&message).unwrap();
        let decoded = (codec.decode)(&bytes).unwrap();
        assert_eq!(decoded.as_ref().type_id(), message.as_ref().type_id());
    }
}
#[test]
fn all_probe_decoders_reject_truncated_and_trailing_payloads() {
    for key in ["bin:u:Ping", "bin:u:Pong", "bin:parrot.interop.Echoed#v1"] {
        let entry = parrot_api::message::inventory::iter::<parrot_api::message::CodecRegistration>
            .into_iter()
            .find(|entry| entry.type_key == key)
            .unwrap();
        for n in [0, 1, 7, 9, 128] {
            assert!(
                (entry.decode)(&vec![0; n]).is_err(),
                "{key} accepted {n} bytes"
            );
        }
    }
}
#[test]
fn probe_checks_dialect_type_and_exact_business_result() {
    for (node, expected) in [("erl-gw-1", 13), ("ray-gw-1", 12), ("jvm-search-1", 10)] {
        let correct: BoxedMessage = if node.starts_with("jvm") {
            Box::new(ProbeEchoed(expected))
        } else {
            Box::new(ProbePong(expected))
        };
        verify_probe_reply(node, 10, &correct).unwrap();
        assert!(verify_probe_reply(node, 11, &correct).is_err());
        assert!(verify_probe_reply(node, 10, &(Box::new(()) as BoxedMessage)).is_err());
    }
}

use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture};
use std::sync::atomic::{AtomicUsize, Ordering};
#[derive(Clone, Debug)]
struct EchoService {
    kind: &'static str,
    bad: Arc<AtomicUsize>,
    seen: Arc<AtomicUsize>,
    gate: Option<Arc<tokio::sync::Semaphore>>,
}
#[async_trait::async_trait]
impl parrot_api::address::ActorRef for EchoService {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = msg
                .downcast_ref::<ProbePing>()
                .expect("wire decoder preserves Ping type")
                .0;
            self.seen.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.acquire().await.unwrap().forget();
            }
            let reply: BoxedMessage = match self.bad.load(Ordering::SeqCst) {
                1 => Box::new(ProbePong(0)),
                2 => Box::new(ProbePing(value)),
                _ => match self.kind {
                    "erl" => Box::new(ProbePong(value + 3)),
                    "ray" => Box::new(ProbePong(value + 2)),
                    _ => Box::new(ProbeEchoed(value)),
                },
            };
            Ok(reply)
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(msg)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        format!("{}/user/echo", self.kind)
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
struct EchoLookup(EchoService);
#[async_trait::async_trait]
impl LocalLookup for EchoLookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == self.0.path() || path == format!("/{}", self.0.path()))
            .then(|| self.0.clone_boxed())
    }
}
async fn tcp_pair(
    kind: &'static str,
    gate: Option<Arc<tokio::sync::Semaphore>>,
) -> (
    Arc<RemoteActorSystem>,
    Arc<RemoteActorSystem>,
    NodeSpec,
    EchoService,
) {
    let name = match kind {
        "erl" => "erl-gw-1",
        "ray" => "ray-gw-1",
        _ => "search-custom",
    };
    let actor = EchoService {
        kind,
        bad: Arc::new(AtomicUsize::new(0)),
        seen: Arc::new(AtomicUsize::new(0)),
        gate,
    };
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap())).with_insecure_development(),
        Arc::new(EchoLookup(actor.clone())),
    )
    .unwrap();
    server.start().await.unwrap();
    let node = NodeSpec {
        name: name.into(),
        dialect: kind,
        addr: server.local_addr().unwrap(),
    };
    let client = connect(std::slice::from_ref(&node), true).await.unwrap();
    (client, server, node, actor)
}
#[tokio::test]
async fn actual_tcp_probe_checks_all_three_wire_dialects_and_invalid_business_replies() {
    for kind in ["erl", "ray", "jvm"] {
        let (client, server, node, actor) = tcp_pair(kind, None).await;
        let good = probe_once(&client, &node, 37).await;
        actor.bad.store(1, Ordering::SeqCst);
        let bad = probe_once(&client, &node, 37).await;
        actor.bad.store(2, Ordering::SeqCst);
        let wrong_type = probe_once(&client, &node, 37).await;
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
        good.unwrap();
        assert!(bad.is_err());
        assert!(wrong_type.is_err());
        assert_eq!(actor.seen.load(Ordering::SeqCst), 3);
    }
}
#[tokio::test]
async fn load_retains_inflight_completion_time_and_finishes_owned_workers() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (client, server, node, actor) = tcp_pair("ray", Some(gate.clone())).await;
    let worker = tokio::spawn(load::run_load(
        client.clone(),
        vec![node],
        Duration::from_millis(200),
        2,
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while actor.seen.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    gate.add_permits(2);
    let result = worker.await.unwrap().unwrap();
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    assert_eq!(result.ok, 2);
    assert_eq!(result.errors, 0);
    assert!(result.elapsed >= Duration::from_millis(250));
    assert_eq!(actor.seen.load(Ordering::SeqCst), 2);
    result.verify().unwrap();
}
#[tokio::test]
async fn wrong_tcp_reply_is_a_failed_load_not_successful_throughput() {
    let (client, server, node, actor) = tcp_pair("erl", None).await;
    actor.bad.store(1, Ordering::SeqCst);
    let result = load::run_load(client.clone(), vec![node], Duration::from_millis(50), 1)
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    assert_eq!(result.ok, 0);
    assert!(result.errors > 0);
    assert_eq!(result.errors, actor.seen.load(Ordering::SeqCst) as u64);
    assert!(result.verify().is_err());
}
