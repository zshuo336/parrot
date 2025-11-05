//! Owned local erl interoperability probe; see interop/README.md for prerequisites and deadlines.

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
        decode: |b: &[u8]| match b.as_chunks::<8>() {
            ([left, right], []) => Ok(Box::new(UAdd(
                u64::from_le_bytes(*left),
                u64::from_le_bytes(*right),
            )) as BoxedMessage),
            _ => Err("len!=16".into()),
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

#[path = "interop_probe/options.rs"]
mod options;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "interop_probe/owner.rs"]
mod owner;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => {
            println!("interop-erl PASS");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("interop-erl: {error}");
            std::process::ExitCode::from(2)
        }
    }
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn run() -> Result<(), String> {
    Err("verified gateway process ownership requires Linux or macOS".into())
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn run() -> Result<(), String> {
    let options = options::Options::parse(
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(15),
    )?;
    let mut signals = options::Signals::new()?;
    let root = owner::source_root()?;
    let mut gateway = owner::Gateway::spawn(
        owner::GatewaySpec {
            command: vec![
                "escript".into(),
                root.join("interop/erlang/parrot_gw.es").into_os_string(),
                options.port.to_string().into(),
                "listen=127.0.0.1".into(),
            ],
            node: "erl-gw-1",
            marker: "PARROT_ERL_PORT=",
            port: options.port,
            inline_ray: false,
            startup_output_limit: 262144,
        },
        options.startup,
    )?;
    let started = tokio::select! {
        result = gateway.ready() => result,
        _ = signals.cancelled() => Err("interrupted during gateway startup".into()),
    };
    let result = match started {
        Ok(port) => {
            println!("erl gateway port: {port}");
            match RemoteActorSystem::new(
                RemoteConfig::tcp("interop-erl-rust", None),
                Arc::new(NoopLookup),
            ) {
                Ok(client) => {
                    let business = tokio::select! {
                        result = tokio::time::timeout(options.business, probe(&client, port)) => result.unwrap_or_else(|_| Err("business deadline exceeded".into())),
                        _ = signals.cancelled() => Err("interrupted during business verification".into()),
                        result = gateway.ended() => Err(format!("gateway ended during business verification: {result:?}")),
                    };
                    let shutdown =
                        tokio::time::timeout(std::time::Duration::from_secs(10), client.shutdown())
                            .await;
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
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (result, Err(error)) => Err(format!("probe: {result:?}; gateway retirement: {error}")),
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn probe(client: &Arc<RemoteActorSystem>, port: u16) -> Result<(), String> {
    client.start().await.map_err(|e| e.to_string())?;
    client
        .connect(&parrot_remote::NodeAddr::tcp(
            "erl-gw-1",
            ([127, 0, 0, 1], port).into(),
        ))
        .await
        .map_err(|e| e.to_string())?;
    let ping = client
        .remote_ref("parrot://erl-gw-1/erl/user/echo")
        .map_err(|e| e.to_string())?;
    let reply = ping
        .send(Box::new(UPing(100)))
        .await
        .map_err(|e| e.to_string())?;
    let got = reply
        .downcast_ref::<UPong>()
        .ok_or("unexpected Ping response type")?
        .0;
    if got != 103 {
        return Err(format!("Ping(100): expected 103, got {got}"));
    }
    let add = client
        .remote_ref("parrot://erl-gw-1/erl/user/calc")
        .map_err(|e| e.to_string())?;
    let reply = add
        .send(Box::new(UAdd(3, 4)))
        .await
        .map_err(|e| e.to_string())?;
    let got = reply
        .downcast_ref::<UAddR>()
        .ok_or("unexpected Add response type")?
        .0;
    if got != 10007 {
        return Err(format!("Add(3,4): expected 10007, got {got}"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "interop_probe/codec_contracts.rs"]
mod codec_contracts;
