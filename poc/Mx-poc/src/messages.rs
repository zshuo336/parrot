//! POC 消息定义 + 编解码注册（跨语言 golden 一致性共用这套 TYPE_KEY）。

use crate::codec::CodecRegistry;
use parrot_api::types::BoxedMessage;

#[derive(Debug, Clone, PartialEq)]
pub struct Ping(pub u64);
#[derive(Debug, Clone, PartialEq)]
pub struct Pong(pub u64);
#[derive(Debug, Clone, PartialEq)]
pub struct Add(pub u64, pub u64);

fn enc_u64(v: u64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}
fn dec_u64(b: &[u8]) -> Result<u64, String> {
    Ok(u64::from_le_bytes(b.try_into().map_err(|_| "u64 len".to_string())?))
}

/// 注册 POC 全套消息。
pub fn install_poc_messages() {
    CodecRegistry::install::<Ping>(
        "bin:u:Ping",
        |m| match m.downcast_ref::<Ping>() {
            Some(Ping(v)) => Ok(enc_u64(*v)),
            None => Err("Ping downcast".into()),
        },
        |b| Ok(Box::new(Ping(dec_u64(b)?))),
    );
    CodecRegistry::install::<Pong>(
        "bin:u:Pong",
        |m| match m.downcast_ref::<Pong>() {
            Some(Pong(v)) => Ok(enc_u64(*v)),
            None => Err("Pong downcast".into()),
        },
        |b| Ok(Box::new(Pong(dec_u64(b)?))),
    );
    CodecRegistry::install::<Add>(
        "bin:u:Add",
        |m| match m.downcast_ref::<Add>() {
            Some(Add(a, b)) => Ok([enc_u64(*a), enc_u64(*b)].concat()),
            None => Err("Add downcast".into()),
        },
        |b| {
            let a = u64::from_le_bytes(b[0..8].try_into().map_err(|_| "Add a")?);
            let c = u64::from_le_bytes(b[8..16].try_into().map_err(|_| "Add b")?);
            Ok(Box::new(Add(a, c)))
        },
    );
    // Add 的回复类型是 u64（ThreadId 唯一，直接注册）
    CodecRegistry::install::<u64>(
        "bin:u:AddR",
        |m| match m.downcast_ref::<u64>() {
            Some(v) => Ok(enc_u64(*v)),
            None => Err("AddR downcast".into()),
        },
        |b| Ok(Box::new(dec_u64(b)?)),
    );
}
