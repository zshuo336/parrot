//! remote-bench：远程性能基准（DEV_01 §6）。
//!
//! 在同进程的两个节点之间使用真实 TCP 回环，测量顺序 ASK 和分批 TELL。
//! `--gate p1` 按 DEV_01 §6 检查 ASK p50 及每条已核对 TELL 的平均耗时。

use std::sync::Arc;
use std::time::{Duration, Instant};

use parrot_api::address::ActorRef;
use parrot_api::errors::ActorError;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};
use std::any::Any;
use tokio::sync::mpsc;

/// 基准消息（bincode 载荷 8B u64——LAN RTT 主导场景的标准体量）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BenchPing(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BenchPong(pub u64);

// bench 消息注册（宏路径——与 parrot-api remote feature 联动）
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot_remote::BenchPing#v1",
        type_id: std::any::TypeId::of::<BenchPing>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<BenchPing>().ok_or("downcast BenchPing")?;
            parrot_api::message::serde_remote_serialize(&m)
        },
        decode: |b: &[u8]| {
            let v: BenchPing = parrot_api::message::serde_remote_deserialize(b)?;
            Ok(Box::new(v) as BoxedMessage)
        },
    }
}

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot_remote::BenchPong#v1",
        type_id: std::any::TypeId::of::<BenchPong>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<BenchPong>().ok_or("downcast BenchPong")?;
            parrot_api::message::serde_remote_serialize(&m)
        },
        decode: |b: &[u8]| {
            let v: BenchPong = parrot_api::message::serde_remote_deserialize(b)?;
            Ok(Box::new(v) as BoxedMessage)
        },
    }
}

/// 回声 actor 桩（bench 不需要完整引擎——直接 ActorRef 桩避免引入 parrot）。
#[derive(Clone, Debug)]
struct EchoStub(mpsc::Sender<u64>);

#[async_trait::async_trait]
impl ActorRef for EchoStub {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send_with_timeout(msg, None)
    }

    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _timeout: Option<std::time::Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(p) = msg.downcast_ref::<BenchPing>() {
                return Ok(Box::new(BenchPong(p.0)) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError("unexpected type".into()))
        })
    }
    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let ping = msg
                .downcast::<BenchPing>()
                .map_err(|_| ActorError::MessageHandlingError("unexpected TELL payload".into()))?;
            self.0.send(ping.0).await.map_err(|_| ActorError::Stopped)
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { Ok(()) })
    }
    fn path(&self) -> String {
        "/user/echo".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[async_trait::async_trait]
impl LocalLookup for EchoStub {
    async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
        (path == "/user/echo").then(|| self.clone_boxed())
    }
}

struct ClientLookup;

#[async_trait::async_trait]
impl LocalLookup for ClientLookup {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}

fn stats(samples: &mut [u64]) -> (u64, u64, u64) {
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p99 = samples[(samples.len() * 99) / 100];
    let max = samples[samples.len() - 1];
    (p50, p99, max)
}

const TELL_BATCH: usize = 1000;

async fn verify_batch(receiver: &mut mpsc::Receiver<u64>, first: u64) -> Result<(), String> {
    let mut seen = [false; TELL_BATCH];
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..TELL_BATCH {
            let value = receiver.recv().await.ok_or("TELL receiver closed")?;
            let slot = value
                .checked_sub(first)
                .filter(|n| *n < TELL_BATCH as u64)
                .ok_or_else(|| format!("unexpected TELL {value} in batch starting at {first}"))?;
            if std::mem::replace(&mut seen[slot as usize], true) {
                return Err(format!("duplicate TELL {value}"));
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| format!("TELL batch starting at {first} timed out"))?
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let gate = std::env::args().any(|a| a == "--gate" || a.starts_with("--gate"));

    // 双节点：server（echo）+ client（bench 驱动），真实 TCP 回环
    let (received, mut receipts) = mpsc::channel(TELL_BATCH);
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("bench-server", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(EchoStub(received)),
    )
    .unwrap();
    server.start().await.unwrap();
    let address = server.local_addr().expect("TCP listener has started");

    // Only the server owns the sink that certifies target deliveries.
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("bench-client", None),
        Arc::new(ClientLookup),
    )
    .unwrap();
    client.start().await.unwrap();
    client
        .connect(&parrot_remote::NodeAddr::tcp("bench-server", address))
        .await
        .unwrap();

    let echo = client
        .remote_ref("parrot://bench-server/user/echo")
        .unwrap();

    // 预热 3s
    let warm_deadline = Instant::now() + Duration::from_secs(3);
    let mut i = 0u64;
    while Instant::now() < warm_deadline {
        let r = echo.send(Box::new(BenchPing(i))).await.unwrap();
        assert_eq!(r.downcast_ref::<BenchPong>().unwrap().0, i);
        i += 1;
    }

    // seq-ask（采样 30s 或 200k 次取先到）
    let mut lat = Vec::with_capacity(200_000);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut n = 0u64;
    while Instant::now() < deadline && lat.len() < 200_000 {
        let t0 = Instant::now();
        let r = echo.send(Box::new(BenchPing(n))).await.unwrap();
        let dt = t0.elapsed().as_nanos() as u64;
        assert_eq!(r.downcast_ref::<BenchPong>().unwrap().0, n);
        lat.push(dt);
        n += 1;
    }
    let (p50, p99, mx) = stats(&mut lat);
    let ask_thruput = (lat.len() as f64) / (lat.iter().map(|v| *v as f64).sum::<f64>() / 1e9);

    // tell-throughput
    let t0 = Instant::now();
    let dur = Duration::from_secs(5);
    let mut sent = 0u64;
    while t0.elapsed() < dur {
        let first = sent;
        for _ in 0..TELL_BATCH {
            echo.deliver(Box::new(BenchPing(sent))).await.unwrap();
            sent += 1;
        }
        verify_batch(&mut receipts, first)
            .await
            .expect("receiver must verify every TELL");
    }
    let tell_rt = t0.elapsed().as_nanos() as f64 / sent as f64;

    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    assert!(receipts.try_recv().is_err(), "unexpected extra TELL");
    println!("== remote-bench（同进程 127.0.0.1 TCP 回环）==");
    println!(
        "seq-ask    : n={} p50={}µs p99={}µs max={}µs thru={:.0}/s",
        lat.len(),
        p50 / 1000,
        p99 / 1000,
        mx / 1000,
        ask_thruput
    );
    println!(
        "tell       : n={} mean={:.1}µs thru={:.0}/s (receiver verified)",
        sent,
        tell_rt / 1000.0,
        1e9 / tell_rt
    );
    println!(
        "remote-tax : p50 - local(~3µs) ≈ {:.1}µs",
        p50 as f64 / 1000.0 - 3.0
    );

    if gate {
        let ask_ok = p50 < 150_000;
        let tell_ok = tell_rt < 60_000.0;
        println!(
            "gate p1    : ask p50<150µs {} | tell<60µs {}",
            if ask_ok { "PASS" } else { "FAIL" },
            if tell_ok { "PASS" } else { "FAIL" }
        );
        if !(ask_ok && tell_ok) {
            eprintln!("E1.9 gate FAILED —— 阻塞发布（超标专项评审）");
            std::process::exit(1);
        }
        println!("E1.9 gate PASS");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn incomplete_batch_fails_with_sender_still_alive() {
        let (sender, mut receiver) = mpsc::channel(TELL_BATCH);
        for value in 0..TELL_BATCH as u64 - 1 {
            sender.send(value).await.unwrap();
        }
        assert!(verify_batch(&mut receiver, 0)
            .await
            .unwrap_err()
            .contains("timed out"));
    }

    #[tokio::test]
    async fn only_distinct_values_from_the_current_batch_count() {
        for bad in [0, 1, TELL_BATCH as u64 + 1] {
            let (sender, mut receiver) = mpsc::channel(TELL_BATCH);
            sender.send(1).await.unwrap();
            sender.send(bad).await.unwrap();
            drop(sender);
            let error = verify_batch(&mut receiver, 1).await.unwrap_err();
            assert!(
                error.contains(if bad == 1 { "duplicate" } else { "unexpected" }),
                "{error}"
            );
        }
        let (sender, mut receiver) = mpsc::channel(TELL_BATCH);
        for value in (1..=TELL_BATCH as u64).rev() {
            sender.send(value).await.unwrap();
        }
        verify_batch(&mut receiver, 1).await.unwrap();
    }
}
