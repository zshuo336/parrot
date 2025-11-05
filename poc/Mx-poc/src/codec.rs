//! 编解码 registry（05 §3 POC：显式注册验证模式；正式版 RemoteMessage derive 自注册）。

use parrot_api::types::BoxedMessage;
use std::collections::HashMap;
use std::sync::Mutex;

pub type EncodeFn = fn(&dyn std::any::Any) -> Result<Vec<u8>, String>;
pub type DecodeFn = fn(&[u8]) -> Result<BoxedMessage, String>;

pub struct CodecEntry {
    pub encode: EncodeFn,
    pub decode: DecodeFn,
}

struct CodecInner {
    by_key: Option<HashMap<&'static str, CodecEntry>>,
    by_type_id: Option<HashMap<std::any::TypeId, &'static str>>,
}

const fn empty_const() -> CodecInner {
    CodecInner {
        by_key: None,
        by_type_id: None,
    }
}

static GLOBAL: Mutex<CodecInner> = Mutex::new(empty_const());

impl CodecInner {
    fn key_map(&mut self) -> &mut HashMap<&'static str, CodecEntry> {
        self.by_key.get_or_insert_with(HashMap::new)
    }
    fn type_map(&mut self) -> &mut HashMap<std::any::TypeId, &'static str> {
        self.by_type_id.get_or_insert_with(HashMap::new)
    }
}

pub struct CodecRegistry;

impl CodecRegistry {
    /// 注册 M 的编解码（重复注册覆盖，测试可重置）。
    pub fn install<M: Send + 'static>(
        type_type_key: &'static str,
        encode: EncodeFn,
        decode: DecodeFn,
    ) {
        let mut g = GLOBAL.lock().unwrap();
        g.key_map()
            .insert(type_type_key, CodecEntry { encode, decode });
        g.type_map()
            .insert(std::any::TypeId::of::<M>(), type_type_key);
    }

    /// 测试隔离：清空。
    pub fn reset() {
        let mut g = GLOBAL.lock().unwrap();
        if let Some(m) = g.by_key.as_mut() {
            m.clear();
        }
        if let Some(m) = g.by_type_id.as_mut() {
            m.clear();
        }
    }

    /// 编码出口：TypeId → TYPE_KEY → encode。未注册 → NotRemotable 错误。
    pub fn encode_outgoing(msg: &BoxedMessage) -> Result<(String, Vec<u8>), String> {
        let (key, encode) = {
            let g = GLOBAL.lock().unwrap();
            let key = g
                .by_type_id
                .as_ref()
                .and_then(|m| m.get(&msg.as_ref().type_id()))
                .copied()
                .ok_or_else(|| format!("not remotable: {:?}", msg.as_ref().type_id()))?;
            let encode = g
                .by_key
                .as_ref()
                .and_then(|m| m.get(key))
                .ok_or("codec entry missing")?
                .encode;
            (key, encode)
        };
        // User callbacks may reenter the registry or panic. Never execute one
        // while holding the global lock.
        let payload = encode(msg.as_ref()).map_err(|e| format!("encode: {e}"))?;
        Ok((key.to_string(), payload))
    }

    pub fn decode_incoming(type_key: &str, payload: &[u8]) -> Result<BoxedMessage, String> {
        let decode = {
            let g = GLOBAL.lock().unwrap();
            g.by_key
                .as_ref()
                .and_then(|m| m.get(type_key))
                .ok_or_else(|| format!("unknown TYPE_KEY {type_key:?}"))?
                .decode
        };
        decode(payload).map_err(|e| format!("decode: {e}"))
    }
}
