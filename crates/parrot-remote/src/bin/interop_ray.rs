//! interop-ray：Rust ↔ Python(ray) 网关一致性验证（DEV_03 §5 / E4 集成门禁）。
//!
//! 流程：
//! 1. spawn `python3 -m parrot_protocol.ray_gw <port>`
//! 2. 解析 stdout 的 `RAY_GW_PORT=<n>`
//! 3. Rust RemoteActorSystem（TCP）connect → 握手（TLV，ray 侧 pb-only caps）
//! 4. ask Ping(100) → Pong(102)（ray 方言 +2）语义验证
//! 5. ask Add(3,4) → AddR(1007)（ray 方言 +1000）
//!
//! 注意：ray 网关是 pb 栈 caps（bit1），Rust 客户端握手协商须兼容（1.0 固定
//! bin 优先 + pb 回退——caps 声明是声明，载荷语义经 type_key 路由透传）。

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;

use parrot_api::address::ActorRef;
use parrot_api::types::BoxedMessage;
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UPing(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UPong(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UAdd(pub u64, pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UAddR(pub u64);

macro_rules! reg {
    ($t:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &BoxedMessage| {
                    let m = msg.downcast_ref::<$t>().ok_or(concat!("downcast ", $key))?;
                    // 网关语义：裸 u64 LE 8B（与 ray_gw 的 struct.unpack("<Q") 对齐）
                    use parrot_remote::bytes::BufMut;
                    let mut b = parrot_remote::bytes::BytesMut::new();
                    b.put_u64_le(m.0);
                    Ok(b.freeze().to_vec())
                },
                decode: |b: &[u8]| {
                    let v = u64::from_le_bytes(b.try_into().map_err(|_| "len!=8")?);
                    Ok(Box::new(<$t>::from_u64(v)) as BoxedMessage)
                },
            }
        }
    };
}

trait FromU64 {
    fn from_u64(v: u64) -> Self;
}
impl FromU64 for UPing {
    fn from_u64(v: u64) -> Self {
        UPing(v)
    }
}
impl FromU64 for UPong {
    fn from_u64(v: u64) -> Self {
        UPong(v)
    }
}

reg!(UPing, "bin:u:Ping");
reg!(UPong, "bin:u:Pong");

// Add 是双 u64 —— 单独注册
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:u:Add",
        type_id: std::any::TypeId::of::<UAdd>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<UAdd>().ok_or("downcast UAdd")?;
            use parrot_remote::bytes::BufMut;
            let mut b = parrot_remote::bytes::BytesMut::new();
            b.put_u64_le(m.0);
            b.put_u64_le(m.1);
            Ok(b.freeze().to_vec())
        },
        decode: |b: &[u8]| {
            Ok(Box::new(UAddR(u64::from_le_bytes(b[8..16].try_into().map_err(|_| "len")?))) as BoxedMessage)
        },
    }
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:u:AddR",
        type_id: std::any::TypeId::of::<UAddR>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<UAddR>().ok_or("downcast UAddR")?;
            use parrot_remote::bytes::BufMut;
            let mut b = parrot_remote::bytes::BytesMut::new();
            b.put_u64_le(m.0);
            Ok(b.freeze().to_vec())
        },
        decode: |b: &[u8]| {
            let v = u64::from_le_bytes(b.try_into().map_err(|_| "len!=8")?);
            Ok(Box::new(UAddR(v)) as BoxedMessage)
        },
    }
}

struct NoopLookup;

#[async_trait::async_trait]
impl LocalLookup for NoopLookup {
    async fn lookup(&self, _path: &str) -> Option<Box<dyn ActorRef>> {
        None
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let port_arg = std::env::args().nth(1).unwrap_or_else(|| "0".into());
    let mut child = Command::new("python3")
        .arg("-m")
        .arg("parrot_protocol.ray_gw")
        .arg(&port_arg)
        .env("PYTHONPATH", "interop/python")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn ray gw");

    let port = {
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).expect("read ray stdout");
            if n == 0 {
                eprintln!("interop-ray: ray gateway exited before printing port");
                std::process::exit(2);
            }
            if let Some(p) = line.strip_prefix("RAY_GW_PORT=") {
                break p.trim().parse::<u16>().expect("parse port");
            }
        }
    };
    println!("ray gateway port: {port}");

    let mut cfg = RemoteConfig::tcp("interop-ray-rust", None);
    cfg.extra_caps = parrot_remote::handshake::caps::PB; // ray 网关 pb-only（07 §8.1）
    let client = RemoteActorSystem::new(cfg, Arc::new(NoopLookup)).unwrap();
    client.start().await.unwrap();
    client
        .connect(&parrot_remote::NodeAddr::tcp(
            "ray-gw-1",
            format!("127.0.0.1:{port}").parse().unwrap(),
        ))
        .await
        .expect("connect+handshake with pb-only peer");

    let ping = client.remote_ref("parrot://ray-gw-1/ray/user/echo").unwrap();

    // 语义：Ping(100) → Pong(102)（ray 方言 +2）
    let r = ping.send(Box::new(UPing(100))).await.unwrap();
    let got = r.downcast_ref::<UPong>().unwrap().0;
    assert_eq!(got, 102, "ray dialect: Ping(100) -> Pong(102)");

    // 语义：Add(3,4) → AddR(1007)（ray 方言 +1000）
    let add = client.remote_ref("parrot://ray-gw-1/ray/user/calc").unwrap();
    let r = add.send(Box::new(UAdd(3, 4))).await.unwrap();
    let got = r.downcast_ref::<UAddR>().unwrap().0;
    assert_eq!(got, 1007, "ray dialect: Add(3,4) -> AddR(1007)");

    let _ = child.kill();
    let _ = child.wait();
    client.shutdown().await.ok();
    println!("interop-ray PASS");
}
