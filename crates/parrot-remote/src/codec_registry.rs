//! 职责：TYPE_KEY → 编解码函数表（inventory 收集；DEV_01 §3.4 / 05 §3.2）。
//!
//! 全局单例（OnceLock 首访收集）：TYPE_KEY 全进程唯一（重复注册 panic——
//! strict 模式，两类型撞键=协议级事故）。双索引：key→entry / TypeId→key。

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::OnceLock;

use parrot_api::message::CodecRegistration;
use parrot_api::types::BoxedMessage;

use crate::codec::CodecStack;
use crate::error::ErrCode;

pub use parrot_api::message::{serde_remote_deserialize, serde_remote_serialize};

#[derive(Clone)]
pub struct CodecEntry {
    pub type_key: &'static str,
    pub encode: fn(&BoxedMessage) -> Result<Vec<u8>, String>,
    pub decode: fn(&[u8]) -> Result<BoxedMessage, String>,
}

pub struct CodecRegistry {
    by_key: HashMap<&'static str, CodecEntry>,
    by_type_id: HashMap<TypeId, &'static str>,
}

impl CodecRegistry {
    /// 全局单例：首次访问 collect inventory（derive 宏 submit 的注册项）。
    pub fn global() -> &'static Self {
        static G: OnceLock<CodecRegistry> = OnceLock::new();
        G.get_or_init(|| Self::from_registrations(inventory::iter::<CodecRegistration>))
    }

    fn from_registrations<'a>(
        registrations: impl IntoIterator<Item = &'a CodecRegistration>,
    ) -> Self {
        let mut by_key = HashMap::new();
        let mut by_type_id = HashMap::new();
        for reg in registrations {
            assert!(
                CodecStack::of_type_key(reg.type_key).is_ok(),
                "invalid TYPE_KEY registered: {:?}",
                reg.type_key
            );
            assert!(
                !by_key.contains_key(reg.type_key),
                "duplicate TYPE_KEY registered: {}",
                reg.type_key
            );
            if let Some(previous) = by_type_id.insert(reg.type_id, reg.type_key) {
                panic!("duplicate TypeId registered under {previous} and {}; use distinct message wrapper types for distinct wire identities", reg.type_key);
            }
            by_key.insert(
                reg.type_key,
                CodecEntry {
                    type_key: reg.type_key,
                    encode: reg.encode,
                    decode: reg.decode,
                },
            );
        }
        Self { by_key, by_type_id }
    }

    /// 测试/手写注册（重复 TYPE_KEY panic——strict）。
    pub fn register_manual(&self, _entry: CodecEntry) -> ! {
        panic!("global registry is frozen after first access（inventory 是唯一注册通道）")
    }

    pub fn key_of(&self, tid: TypeId) -> Option<&'static str> {
        self.by_type_id.get(&tid).copied()
    }

    pub fn get(&self, key: &str) -> Option<&CodecEntry> {
        self.by_key.get(key)
    }

    /// schema-diff 工具钩子（P2 CLI 消费）。
    pub fn dump(&self) -> Vec<(String, CodecStack)> {
        let mut entries: Vec<_> = self
            .by_key
            .keys()
            .map(|key| {
                (
                    key.to_string(),
                    CodecStack::of_type_key(key).expect("validated registry key"),
                )
            })
            .collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// 编码出口：TypeId → TYPE_KEY → encode。未注册 → NotRemotable（RC6：不发帧）。
    pub fn encode_outgoing(&self, msg: &BoxedMessage) -> Result<(String, Vec<u8>), ErrCode> {
        let key = self.key_of((*msg).type_id()).ok_or(ErrCode::NotRemotable)?;
        let entry = self.get(key).expect("key→entry invariant");
        let stack = CodecStack::of_type_key(key)?;
        stack.assert_available()?;
        let payload = (entry.encode)(msg).map_err(|_| ErrCode::CodecError)?;
        Ok((key.to_string(), payload))
    }

    /// 解码入口：type_key → decode。未知键 → UnknownTypeKey（两端不一致）。
    pub fn decode_incoming(&self, type_key: &str, payload: &[u8]) -> Result<BoxedMessage, ErrCode> {
        let entry = self.get(type_key).ok_or(ErrCode::UnknownTypeKey)?;
        CodecStack::of_type_key(type_key)?.assert_available()?;
        (entry.decode)(payload).map_err(|_| ErrCode::CodecError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 测试消息（不走 derive——单元测试聚焦 registry 本身；derive 链路在
    // parrot-api-derive-tests / 集成测试覆盖）。
    struct TestMsg(u64);

    fn manual_registrations() -> Vec<CodecRegistration> {
        vec![CodecRegistration {
            type_key: "bin:parrot_remote_test::TestMsg#v1",
            type_id: TypeId::of::<TestMsg>(),
            encode: |msg: &BoxedMessage| {
                let m = msg.downcast_ref::<TestMsg>().ok_or("downcast")?;
                serde_remote_serialize(&m.0)
            },
            decode: |b: &[u8]| {
                let v: u64 = serde_remote_deserialize(b)?;
                Ok(Box::new(TestMsg(v)) as BoxedMessage)
            },
        }]
    }

    fn registry_with_test_msgs() -> &'static CodecRegistry {
        static R: OnceLock<CodecRegistry> = OnceLock::new();
        R.get_or_init(|| CodecRegistry::from_registrations(&manual_registrations()))
    }

    #[test]
    #[should_panic(expected = "duplicate TYPE_KEY")]
    fn duplicate_key_is_rejected() {
        let mut entries = manual_registrations();
        let mut duplicate = manual_registrations().remove(0);
        duplicate.type_id = TypeId::of::<String>();
        entries.push(duplicate);
        CodecRegistry::from_registrations(&entries);
    }

    #[test]
    #[should_panic(expected = "duplicate TypeId")]
    fn ambiguous_outgoing_identity_is_rejected() {
        let mut entries = manual_registrations();
        let mut duplicate = manual_registrations().remove(0);
        duplicate.type_key = "bin:other::TestMsg#v2";
        entries.push(duplicate);
        CodecRegistry::from_registrations(&entries);
    }

    #[test]
    #[should_panic(expected = "invalid TYPE_KEY")]
    fn invalid_key_cannot_be_dumped_as_bincode() {
        let mut entries = manual_registrations();
        entries[0].type_key = "json:TestMsg";
        CodecRegistry::from_registrations(&entries);
    }

    #[test]
    fn schema_dump_is_sorted_independent_of_registration_order() {
        let mut entries = manual_registrations();
        let mut second = manual_registrations().remove(0);
        second.type_key = "bin:aaa::Other#v1";
        second.type_id = TypeId::of::<String>();
        entries.push(second);
        let forward = CodecRegistry::from_registrations(&entries).dump();
        entries.reverse();
        assert_eq!(forward, CodecRegistry::from_registrations(&entries).dump());
        assert_eq!(forward[0].0, "bin:aaa::Other#v1");
    }

    #[test]
    fn codec_registry_register_lookup() {
        let r = registry_with_test_msgs();
        assert_eq!(
            r.key_of(TypeId::of::<TestMsg>()),
            Some("bin:parrot_remote_test::TestMsg#v1")
        );
        let msg: BoxedMessage = Box::new(TestMsg(7));
        let (key, payload) = r.encode_outgoing(&msg).unwrap();
        assert_eq!(key, "bin:parrot_remote_test::TestMsg#v1");
        let back = r.decode_incoming(&key, &payload).unwrap();
        assert_eq!(back.downcast_ref::<TestMsg>().unwrap().0, 7);
    }

    #[test]
    fn codec_unknown_type_notremotable() {
        let r = registry_with_test_msgs();
        #[derive(Debug)]
        struct Secret;
        let msg2: BoxedMessage = Box::new(Secret);
        let err = r.encode_outgoing(&msg2).unwrap_err();
        assert_eq!(err, ErrCode::NotRemotable);
        // 未知键
        assert!(matches!(
            r.decode_incoming("bin:nowhere::Ghost#v1", b""),
            Err(ErrCode::UnknownTypeKey)
        ));
    }

    #[test]
    fn bin_stack_roundtrip_nested() {
        // bincode 嵌套结构（serde 层能力验证——宏路径在集成测试）
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        enum Inner {
            A(Vec<String>),
            B { x: u64 },
        }
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Nested {
            list: Vec<Inner>,
            big: String,
        }
        let n = Nested {
            list: vec![Inner::A(vec!["x".into(), "y".into()]), Inner::B { x: 42 }],
            big: "z".repeat(4096),
        };
        let bytes = serde_remote_serialize(&n).unwrap();
        let back: Nested = serde_remote_deserialize(&bytes).unwrap();
        assert_eq!(back, n);
    }
}
