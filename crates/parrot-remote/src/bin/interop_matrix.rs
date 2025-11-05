//! interop-matrix：Rust ↔ JVM 双语言一致性 + RTT 门禁（DEV_00 §4.2 P2 命令）。
//!
//! 流程：
//! 1. spawn `java -cp <jvm-gw-jar> parrot.protocol.jvm.ParrotGatewayMain <port>`
//! 2. 解析 stdout 的 `PARROT_JVM_PORT=<n>`（随机端口防冲突）
//! 3. Rust RemoteActorSystem（TCP）connect → 握手 → HANDSHAKE_ACK 交互
//! 4. ask echo ×N 采样 RTT；ask cpu（bincode standard u64 +1）宽度边界验证
//! 5. RTT p50 <300µs 门禁（DEV_02 §6.3——超标 exit 1）
//!
//! classpath: PARROT_JVM_CP, otherwise PARROT_JVM_JAR plus sibling cp.txt;
//! default jar is resolved from the source workspace. Startup and cleanup are owned.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parrot_api::address::ActorRef;
use parrot_api::types::BoxedMessage;
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};

/// Newtype messages use bincode standard u64 encoding (one to nine bytes).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteropEcho(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteropEchoed(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteropCpu(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteropCpuResult(pub u64);

// —— bincode registration: echo preserves bytes; CPU decodes standard u64. ——
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot.interop.Echo#v1",
        type_id: std::any::TypeId::of::<InteropEcho>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<InteropEcho>().ok_or("downcast InteropEcho")?;
            parrot_api::message::serde_remote_serialize(&m)
        },
        decode: |b: &[u8]| {
            let v: InteropEcho = parrot_api::message::serde_remote_deserialize(b)?;
            Ok(Box::new(v) as BoxedMessage)
        },
    }
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot.interop.Echoed#v1",
        type_id: std::any::TypeId::of::<InteropEchoed>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<InteropEchoed>().ok_or("downcast InteropEchoed")?;
            parrot_api::message::serde_remote_serialize(&m)
        },
        decode: |b: &[u8]| {
            let v: InteropEchoed = parrot_api::message::serde_remote_deserialize(b)?;
            Ok(Box::new(v) as BoxedMessage)
        },
    }
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot.interop.Cpu#v1",
        type_id: std::any::TypeId::of::<InteropCpu>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<InteropCpu>().ok_or("downcast InteropCpu")?;
            parrot_api::message::serde_remote_serialize(&m)
        },
        decode: |b: &[u8]| {
            let v: InteropCpu = parrot_api::message::serde_remote_deserialize(b)?;
            Ok(Box::new(v) as BoxedMessage)
        },
    }
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot.interop.CpuResult#v1",
        type_id: std::any::TypeId::of::<InteropCpuResult>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<InteropCpuResult>().ok_or("downcast InteropCpuResult")?;
            parrot_api::message::serde_remote_serialize(&m)
        },
        decode: |b: &[u8]| {
            let v: InteropCpuResult = parrot_api::message::serde_remote_deserialize(b)?;
            Ok(Box::new(v) as BoxedMessage)
        },
    }
}

/// 本地空 lookup（interop 客户端无需本地 actor）。
struct NoopLookup;

#[async_trait::async_trait]
impl LocalLookup for NoopLookup {
    async fn lookup(&self, _path: &str) -> Option<Box<dyn ActorRef>> {
        None
    }
}

type Result<T> = std::result::Result<T, String>;

fn classpath() -> Result<std::ffi::OsString> {
    if let Some(cp) = std::env::var_os("PARROT_JVM_CP") {
        return if cp.is_empty() {
            Err("PARROT_JVM_CP is empty".into())
        } else {
            Ok(cp)
        };
    }
    let jar = std::env::var_os("PARROT_JVM_JAR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../interop/jvm/target/parrot-protocol-jvm-0.1.0.jar")
        });
    if !jar.is_file() {
        return Err(format!(
            "JVM jar missing: {}; run make build-jvm",
            jar.display()
        ));
    }
    let deps = std::fs::read_to_string(jar.parent().ok_or("JVM jar has no parent")?.join("cp.txt"))
        .map_err(|e| format!("JVM dependency classpath: {e}"))?;
    let mut paths = vec![jar];
    paths.extend(std::env::split_paths(deps.trim()));
    std::env::join_paths(paths).map_err(|e| e.to_string())
}

#[path = "interop_probe/options.rs"]
mod options;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "interop_probe/owner.rs"]
mod owner;

async fn exercise(client: &Arc<RemoteActorSystem>, port: u16) -> Result<()> {
    client.start().await.map_err(|e| e.to_string())?;
    client
        .connect(&parrot_remote::NodeAddr::tcp(
            "jvm-gw-1",
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        ))
        .await
        .map_err(|e| e.to_string())?;
    let echo = client
        .remote_ref("parrot://jvm-gw-1/jvm/user/echo")
        .map_err(|e| e.to_string())?;
    let cpu = client
        .remote_ref("parrot://jvm-gw-1/jvm/user/cpu")
        .map_err(|e| e.to_string())?;
    let values = [
        0,
        41,
        126,
        127,
        128,
        249,
        250,
        251,
        65534,
        65535,
        65536,
        u32::MAX as u64 - 1,
        u32::MAX as u64,
        u32::MAX as u64 + 1,
        i64::MAX as u64,
        i64::MAX as u64 + 1,
        u64::MAX - 1,
    ];
    for value in values {
        let reply = cpu
            .send(Box::new(InteropCpu(value)))
            .await
            .map_err(|e| e.to_string())?;
        let actual = reply
            .downcast_ref::<InteropCpuResult>()
            .ok_or("unexpected CPU reply type")?
            .0;
        if actual != value + 1 {
            return Err(format!(
                "CPU value {value}: expected {}, got {actual}",
                value + 1
            ));
        }
    }
    if !matches!(cpu.send(Box::new(InteropCpu(u64::MAX))).await,
        Err(parrot_api::errors::ActorError::MessageHandlingError(ref detail)) if detail.starts_with("codec:"))
    {
        return Err("CPU overflow did not return CodecError".into());
    }
    // A rejected request must not terminate the production CPU actor.
    let reply = cpu
        .send(Box::new(InteropCpu(41)))
        .await
        .map_err(|e| e.to_string())?;
    if reply.downcast_ref::<InteropCpuResult>().map(|v| v.0) != Some(42) {
        return Err("CPU did not recover after rejection".into());
    }
    println!("Rust/JVM bincode CPU: 17 boundaries, overflow rejection and recovery PASS");
    let mut lat = Vec::with_capacity(2000);
    for i in 0..2500u64 {
        let start = Instant::now();
        let reply = echo
            .send(Box::new(InteropEcho(i)))
            .await
            .map_err(|e| e.to_string())?;
        if reply.downcast_ref::<InteropEchoed>().map(|v| v.0) != Some(i) {
            return Err(format!("echo payload mismatch at {i}"));
        }
        if i >= 500 {
            lat.push(start.elapsed().as_nanos() as u64);
        }
    }
    lat.sort_unstable();
    let p50 = lat[lat.len() / 2];
    let p99 = lat[lat.len() * 99 / 100];
    println!(
        "rust→jvm ask echo : n={} p50={:.3}µs p99={:.3}µs",
        lat.len(),
        p50 as f64 / 1000.0,
        p99 as f64 / 1000.0
    );
    let ok = p50 < 300_000;
    println!("gate p2 (RTT<300µs): {}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        return Err("interop RTT gate FAILED".into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn run() -> Result<()> {
    Err("verified gateway process ownership requires Linux or macOS".into())
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn run() -> Result<()> {
    let options = options::Options::parse(Duration::from_secs(15), Duration::from_secs(60))?;
    let cp = classpath()?;
    let mut signals = options::Signals::new()?;
    let mut gateway = owner::Gateway::spawn(
        owner::GatewaySpec {
            command: vec![
                "java".into(),
                "-cp".into(),
                cp,
                "parrot.protocol.jvm.ParrotGatewayMain".into(),
                options.port.to_string().into(),
                "120".into(),
            ],
            node: "jvm-gw-1",
            marker: "PARROT_JVM_PORT=",
            port: options.port,
            inline_ray: false,
            startup_output_limit: 64 * 1024,
        },
        options.startup,
    )?;
    let started = tokio::select! {
        result = gateway.ready() => result,
        _ = signals.cancelled() => Err("interrupted during gateway startup".into()),
    };
    let result = match started {
        Ok(port) => {
            println!("jvm gateway port: {port}");
            match RemoteActorSystem::new(
                RemoteConfig::tcp("interop-rust", None),
                Arc::new(NoopLookup),
            ) {
                Ok(client) => {
                    let business = tokio::select! {
                        result = tokio::time::timeout(options.business, exercise(&client, port)) => result.unwrap_or_else(|_| Err("interop matrix deadline exceeded".into())),
                        _ = signals.cancelled() => Err("interrupted during business verification".into()),
                        result = gateway.ended() => Err(format!("gateway ended during business verification: {result:?}")),
                    };
                    let shutdown =
                        tokio::time::timeout(Duration::from_secs(5), client.shutdown()).await;
                    match shutdown {
                        Ok(Ok(())) => business,
                        other => Err(format!(
                            "business: {business:?}; remote shutdown: {other:?}"
                        )),
                    }
                }
                Err(error) => Err(error.to_string()),
            }
        }
        Err(error) => Err(error),
    };
    let retired = gateway.retire().await;
    match (result, retired) {
        (Ok(()), Ok(())) => {
            println!("interop-matrix PASS");
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (result, Err(error)) => Err(format!("probe: {result:?}; gateway retirement: {error}")),
    }
}

// This probe has one in-flight ask; pin its client scheduling cost across hosts.
#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("interop-matrix: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    async fn rejected_start(mode: &str, expected: &str) {
        let directory = tempfile::tempdir().unwrap();
        let record = directory.path().join("port");
        let script = r#"import os,sys,time,subprocess
from pathlib import Path
child=r'''import socket,sys,time
from pathlib import Path
s=socket.socket();s.bind(('127.0.0.1',0));s.listen()
p=Path(sys.argv[1]);t=p.with_suffix('.tmp');t.write_text(str(s.getsockname()[1]));t.replace(p)
while True:time.sleep(.1)
'''
subprocess.Popen([sys.executable,'-c',child,sys.argv[1]])
while not Path(sys.argv[1]).exists():time.sleep(.01)
if sys.argv[2]=='invalid':print('PARROT_JVM_PORT=0',flush=True)
elif sys.argv[2]=='oversized':print('x'*70000,flush=True)
while True:time.sleep(.1)
"#;
        let mut gateway = owner::Gateway::spawn(
            owner::GatewaySpec {
                command: vec![
                    "python3".into(),
                    "-u".into(),
                    "-c".into(),
                    script.into(),
                    record.as_os_str().to_owned(),
                    mode.into(),
                ],
                node: "jvm-gw-1",
                marker: "PARROT_JVM_PORT=",
                port: 0,
                inline_ray: false,
                startup_output_limit: 64 * 1024,
            },
            Duration::from_secs(1),
        )
        .unwrap();
        let observed = gateway.ready().await;
        let retired = gateway.retire().await;
        let error = observed.unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(
            retired.is_err(),
            "startup failure must retain its failure receipt"
        );
        let port = std::fs::read_to_string(record)
            .unwrap()
            .parse::<u16>()
            .unwrap();
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "returned with a listening descendant"
        );
    }
    #[tokio::test]
    async fn startup_timeout_kills_and_reaps_owned_peer() {
        rejected_start("silent", "deadline").await;
    }
    #[tokio::test]
    async fn invalid_port_kills_and_reaps_owned_peer() {
        rejected_start("invalid", "invalid readiness port").await;
    }
    #[tokio::test]
    async fn startup_output_limit_kills_and_reaps_owned_peer() {
        rejected_start("oversized", "output exceeds limit").await;
    }
}
