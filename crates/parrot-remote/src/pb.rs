//! 职责：pb 编码栈——JVM 桥协议消息（DEV_02 §6.1 / K5）。
//!
//! P2 手维护 .proto 等价（prost 派生；量少——06 ⚠ 复核点）。TYPE_KEY
//! `pb:{package}.{Message}`；CodecRegistry 不变（pb entry 与 bin entry 同表）。
//! 桥消息：akka 网关的 envelope 与控制语义（echo/cpu 基准载荷 + death pact）。

use parrot_api::message::CodecRegistration;
use parrot_api::types::BoxedMessage;

/// akka 桥 envelope（parrot.protocol.v1 包）。
#[derive(Clone, PartialEq, prost::Message)]
pub struct AkkaEnvelope {
    /// akka 路径（如 "user/echo-1"——网关侧拼 /jvm 前缀）。
    #[prost(string, tag = "1")]
    pub akka_path: String,
    /// 载荷类型（"echo" | "cpu" | "raw"）。
    #[prost(string, tag = "2")]
    pub kind: String,
    /// 载荷（echo: utf8 文本；cpu: u64 LE；raw: 透传）。
    #[prost(bytes = "vec", tag = "3")]
    pub payload: Vec<u8>,
}

/// 注册表键（pb: 前缀 + 包.消息名——JVM 侧同键）。
pub const AKKA_ENVELOPE_KEY: &str = "pb:parrot.protocol.v1.AkkaEnvelope";

fn encode_envelope(msg: &BoxedMessage) -> Result<Vec<u8>, String> {
    use prost::Message as _;
    let m = msg
        .downcast_ref::<AkkaEnvelope>()
        .ok_or("downcast AkkaEnvelope")?;
    Ok(m.encode_to_vec())
}

fn decode_envelope(b: &[u8]) -> Result<BoxedMessage, String> {
    use prost::Message as _;
    let m = AkkaEnvelope::decode(b).map_err(|e| format!("pb decode: {e}"))?;
    Ok(Box::new(m) as BoxedMessage)
}

parrot_api::message::inventory::submit! {
    CodecRegistration {
        type_key: AKKA_ENVELOPE_KEY,
        type_id: std::any::TypeId::of::<AkkaEnvelope>(),
        encode: encode_envelope,
        decode: decode_envelope,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec_registry::CodecRegistry;

    // pb 注册进统一表 + 编解码往返 + 键前缀路由
    #[test]
    fn pb_entry_registered_and_roundtrip() {
        let reg = CodecRegistry::global();
        let entry = reg
            .get(AKKA_ENVELOPE_KEY)
            .expect("pb entry in unified registry");
        let m = AkkaEnvelope {
            akka_path: "user/echo-1".into(),
            kind: "echo".into(),
            payload: b"hello".to_vec(),
        };
        let bytes = (entry.encode)(&(Box::new(m.clone()) as BoxedMessage)).unwrap();
        let back = (entry.decode)(&bytes).unwrap();
        let got = back.downcast_ref::<AkkaEnvelope>().unwrap();
        assert_eq!(got, &m);
        // 编码确定性（pb wire 稳定——golden 冻结前提）
        let bytes2 = (entry.encode)(&(Box::new(m.clone()) as BoxedMessage)).unwrap();
        assert_eq!(bytes, bytes2);
    }

    // 键前缀路由 pb: → CodecStack::Pb
    #[test]
    fn pb_key_stack_routing() {
        assert_eq!(
            crate::codec::CodecStack::of_type_key(AKKA_ENVELOPE_KEY),
            Ok(crate::codec::CodecStack::Pb)
        );
    }

    // 编码出口全链路：TypeId → pb key → bytes（RC6 语义对 pb 同样成立）
    #[test]
    fn pb_encode_outgoing_via_registry() {
        let reg = CodecRegistry::global();
        let (key, bytes) = reg
            .encode_outgoing(
                &(Box::new(AkkaEnvelope {
                    akka_path: "user/cpu".into(),
                    kind: "cpu".into(),
                    payload: 42u64.to_le_bytes().to_vec(),
                }) as BoxedMessage),
            )
            .unwrap();
        assert_eq!(key, AKKA_ENVELOPE_KEY);
        assert!(!bytes.is_empty());
    }
}
