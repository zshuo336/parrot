//! interop-erl：Rust ↔ Erlang 网关一致性验证（DEV_03 §7 / E6 集成门禁）。
//!
//! 流程：
//! 1. spawn `escript interop/erlang/parrot_gw.es 0`
//! 2. 解析 stdout 的 `PARROT_ERL_PORT=<n>`
//! 3. Rust RemoteActorSystem（TCP，caps 加 PB——erl 网关 pb-only）connect
//! 4. ask Ping(100) → Pong(103)（erlang 方言 +3）
//! 5. ask Add(3,4) → AddR(10007)（erlang 方言 +10000）

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

macro_rules! reg_u64 {
    ($t:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &BoxedMessage| {
                    let m = msg.downcast_ref::<$t>().ok_or(concat!("downcast ", $key))?;
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

reg_u64!(UPing, "bin:u:Ping");
reg_u64!(UPong, "bin:u:Pong");

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
        decode: |_b: &[u8]| {
            Ok(Box::new(UAdd(0, 0)) as BoxedMessage) // Add 请求不做 decode 载体
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
    let gw = "interop/erlang/parrot_gw.es";
    let mut child = Command::new("escript")
        .arg(gw)
        .arg("0")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("interop-erl: spawn escript failed ({e})——检查 erlang 安装");
            std::process::exit(2);
        });

    let port = {
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).expect("read erl stdout");
            if n == 0 {
                eprintln!("interop-erl: erlang gateway exited before printing port");
                std::process::exit(2);
            }
            if let Some(p) = line.strip_prefix("PARROT_ERL_PORT=") {
                break p.trim().parse::<u16>().expect("parse port");
            }
        }
    };
    println!("erlang gateway port: {port}");

    let mut cfg = RemoteConfig::tcp("interop-erl-rust", None);
    cfg.extra_caps = parrot_remote::handshake::caps::PB; // erl 网关 pb-only
    let client = RemoteActorSystem::new(cfg, Arc::new(NoopLookup)).unwrap();
    client.start().await.unwrap();
    client
        .connect(&parrot_remote::NodeAddr::tcp(
            "erl-gw-1",
            format!("127.0.0.1:{port}").parse().unwrap(),
        ))
        .await
        .expect("connect+handshake with pb-only peer");

    let ping = client
        .remote_ref("parrot://erl-gw-1/erl/user/echo")
        .unwrap();
    let r = ping.send(Box::new(UPing(100))).await.unwrap();
    let got = r.downcast_ref::<UPong>().unwrap().0;
    assert_eq!(got, 103, "erlang dialect: Ping(100) -> Pong(103)");

    let add = client
        .remote_ref("parrot://erl-gw-1/erl/user/calc")
        .unwrap();
    let r = add.send(Box::new(UAdd(3, 4))).await.unwrap();
    let got = r.downcast_ref::<UAddR>().unwrap().0;
    assert_eq!(got, 10007, "erlang dialect: Add(3,4) -> AddR(10007)");

    let _ = child.kill();
    let _ = child.wait();
    client.shutdown().await.ok();
    println!("interop-erl PASS");
}
