//! interop-matrix：Rust ↔ JVM 双语言一致性 + RTT 门禁（DEV_00 §4.2 P2 命令）。
//!
//! 流程：
//! 1. spawn `java -cp <jvm-gw-jar> parrot.protocol.jvm.ParrotGatewayMain <port>`
//! 2. 解析 stdout 的 `PARROT_JVM_PORT=<n>`（随机端口防冲突）
//! 3. Rust RemoteActorSystem（TCP）connect → 握手 → HANDSHAKE_ACK 交互
//! 4. ask echo（载荷 8B）×N 采样 RTT；ask cpu（u64 BE +1）语义验证
//! 5. RTT p50 <300µs 门禁（DEV_02 §6.3——超标 exit 1）
//!
//! jar 路径解析顺序：$PARROT_JVM_JAR / target/scala-* 不适用（Maven）→
//! interop/jvm/target/parrot-protocol-jvm-*.jar（repo 根/工作目录双探测）。

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

use parrot_api::address::ActorRef;
use parrot_api::errors::ActorError;
use parrot_api::types::BoxedMessage;
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};

/// 载荷消息：8B u64 BE——与 JVM cpuAny 的 `payload(i)→(v<<8)|` 语义对齐。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteropEcho(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteropEchoed(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteropCpu(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteropCpuResult(pub u64);

// —— codec 注册（bincode 栈；JVM 侧不反序列化载荷——echo 原样回、cpu 按 BE u64 解）——
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

fn find_jar() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PARROT_JVM_JAR") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    // repo 根 / cwd 双探测：interop/jvm/target/*.jar（shaded 或普通皆可）
    for base in ["..", "."] {
        let dir = PathBuf::from(base).join("interop/jvm/target");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut best: Option<PathBuf> = None;
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".jar") && name.contains("parrot-protocol-jvm") {
                    // original- 前缀是 shade 前的原始包——优先非 original
                    if !name.starts_with("original-") {
                        best = Some(e.path());
                    }
                }
            }
            if best.is_some() {
                return best;
            }
        }
    }
    None
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let jar = find_jar().unwrap_or_else(|| {
        eprintln!("interop-matrix: interop/jvm jar not found (set PARROT_JVM_JAR or build via mvn package)");
        std::process::exit(2);
    });
    println!("jvm jar: {}", jar.display());

    // 起 JVM 网关（随机端口）。classpath：优先 $PARROT_JVM_CP；否则 jar+cp.txt
    let jar_dir = jar.parent().unwrap().to_path_buf();
    let cp = std::env::var("PARROT_JVM_CP")
        .ok()
        .or_else(|| {
            let cp_file = jar_dir.join("cp.txt");
            std::fs::read_to_string(cp_file)
                .ok()
                .map(|deps| format!("{}:{}", jar.display(), deps.trim()))
        })
        .unwrap_or_else(|| jar.display().to_string());
    let mut child = Command::new("java")
        .arg("-cp")
        .arg(&cp)
        .arg("parrot.protocol.jvm.ParrotGatewayMain")
        .arg("0")
        .arg("300")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn java");

    let port = {
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).expect("read jvm stdout");
            if n == 0 {
                eprintln!("interop-matrix: jvm gateway exited before printing port");
                std::process::exit(2);
            }
            if let Some(p) = line.strip_prefix("PARROT_JVM_PORT=") {
                break p.trim().parse::<u16>().expect("parse port");
            }
        }
        // 注意：reader drop 后 child.stdout 变 None——后续 stdout 缓冲不读不阻塞（Netty 不再输出）
    };
    println!("jvm gateway port: {port}");

    // Rust 客户端连接
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("interop-rust", None),
        Arc::new(NoopLookup),
    )
    .unwrap();
    client.start().await.unwrap();
    client
        .connect(&parrot_remote::NodeAddr::tcp(
            "jvm-gw-1",
            format!("127.0.0.1:{port}").parse().unwrap(),
        ))
        .await
        .unwrap();

    let echo = client
        .remote_ref("parrot://jvm-gw-1/jvm/user/echo")
        .unwrap();
    let cpu = client.remote_ref("parrot://jvm-gw-1/jvm/user/cpu").unwrap();

    // —— 语义验证：cpu 41→42（BE u64 往返）——
    let r = cpu.send(Box::new(InteropCpu(41))).await.unwrap();
    let got = r.downcast_ref::<InteropCpuResult>().unwrap().0;
    assert_eq!(got, 42, "cpu 41→42 semantic check");

    // —— RTT 采样：echo ×2000（预热 500 不计）——
    let mut lat: Vec<u64> = Vec::with_capacity(2000);
    for i in 0..2500u64 {
        let t0 = Instant::now();
        let r = echo.send(Box::new(InteropEcho(i))).await.unwrap();
        assert_eq!(r.downcast_ref::<InteropEchoed>().unwrap().0, i);
        if i >= 500 {
            lat.push(t0.elapsed().as_nanos() as u64);
        }
    }
    lat.sort_unstable();
    let p50 = lat[lat.len() / 2];
    let p99 = lat[(lat.len() * 99) / 100];
    println!(
        "rust→jvm ask echo : n={} p50={}µs p99={}µs",
        lat.len(),
        p50 / 1000,
        p99 / 1000
    );

    // —— 门禁：RTT p50 <300µs（DEV_02 §6.3）——
    let ok = p50 < 300_000;
    println!("gate p2 (RTT<300µs): {}", if ok { "PASS" } else { "FAIL" });
    let _ = child.kill();
    let _ = child.wait();
    client.shutdown().await.ok();
    if !ok {
        eprintln!("interop RTT gate FAILED —— 阻塞发布");
        std::process::exit(1);
    }
    println!("interop-matrix PASS");
    // 保留类型引用（避免未使用告警——cfg(test) 外不可达路径）
    let _ = ActorError::MessageHandlingError(String::new());
}
