//! 职责：TLV 握手体编解码 + 能力协商规则（07 §2.4，X3 裁定：一次定死）。
//!
//! payload = TLV 序列 `[u8 tag][u16 len][bytes]`。未知 tag **跳过不报错**
//! （前向兼容）；重复 tag / 必填缺失（node_id/capabilities/max_frame_len/
//! topology_role）→ HandshakeError。

use bytes::{BufMut, BytesMut};

use crate::error::ErrCode;

pub mod tlv_tag {
    pub const NODE_ID: u8 = 1;
    pub const REALM: u8 = 2;
    pub const CLUSTER: u8 = 3;
    /// u32 LE 位域：bit0 bincode 栈 / bit1 pb 栈 / bit2 zstd / bit3 quic / bit4 ws
    pub const CAPABILITIES: u8 = 4;
    /// u32 LE
    pub const MAX_FRAME_LEN: u8 = 5;
    /// u8：0 普通 / 1 hub / 2 border / 3 directory
    pub const TOPOLOGY_ROLE: u8 = 6;
    /// u8，缺省 8
    pub const HOP_LIMIT: u8 = 7;
    /// 可选："host:port"（方案 A 直连学习——spoke 声明可被直拨的监听地址；
    /// 缺省/空 = 不可直拨，hub 不注入 ROUTE_HINT）。
    pub const DIRECT_ADDR: u8 = 8;
}

pub mod caps {
    pub const BIN: u32 = 1 << 0;
    pub const PB: u32 = 1 << 1;
    pub const ZSTD: u32 = 1 << 2;
    pub const QUIC: u32 = 1 << 3;
    pub const WS: u32 = 1 << 4;
    /// B2（DEV_09）：节点支持 admin-v2 组件部署（ArtifactChannel + Executor）。
    /// 发起侧预判：未置位节点不发 DeployComponent（老节点回 Unsupported）。
    pub const ARTIFACTS: u32 = 1 << 5;
    /// C 阶段 feature gate：wasmtime 运行时（节点如实上报——未启用不发）。
    pub const WASM: u32 = 1 << 6;
    /// D 阶段 feature gate：cdylib 动态加载（节点如实上报——未启用不发）。
    pub const DYLIB: u32 = 1 << 7;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    #[error("必填 tag {0} 缺失")]
    MissingRequired(u8),
    #[error("tag {0} 重复出现")]
    DuplicateTag(u8),
    #[error("tag {0} 长度非法：{1}")]
    BadLen(u8, usize),
    #[error("tag {0} UTF-8 解码失败")]
    Utf8(u8),
    #[error("TLV 尾部截断（tag {0} 声明 {1} 剩 {2}）")]
    Truncated(u8, usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TopologyRole {
    Normal = 0,
    Hub = 1,
    Border = 2,
    Directory = 3,
}

impl TopologyRole {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Normal,
            1 => Self::Hub,
            2 => Self::Border,
            3 => Self::Directory,
            _ => return None,
        })
    }
}

/// HANDSHAKE 体（必填：node_id/capabilities/max_frame_len/topology_role）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeBody {
    pub node_id: String,
    pub realm: Option<String>,
    pub cluster: Option<String>,
    pub capabilities: u32,
    pub max_frame_len: u32,
    pub topology_role: TopologyRole,
    pub hop_limit: u8,
    /// 可直拨监听地址 "host:port"（方案 A；None = 不可直拨）。
    pub direct_addr: Option<String>,
}

impl Default for HandshakeBody {
    fn default() -> Self {
        Self {
            node_id: String::new(),
            realm: None,
            cluster: None,
            capabilities: caps::BIN,
            max_frame_len: crate::frame::MAX_FRAME_LEN,
            topology_role: TopologyRole::Normal,
            hop_limit: crate::frame::DEFAULT_HOP_LIMIT,
            direct_addr: None,
        }
    }
}

/// HANDSHAKE_ACK 体（同族字段 + chosen_codec）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeAckBody {
    pub node_id: String,
    pub realm: Option<String>,
    pub cluster: Option<String>,
    pub capabilities: u32,
    pub max_frame_len: u32,
    pub topology_role: TopologyRole,
    pub hop_limit: u8,
    /// 可直拨监听地址（方案 A）。
    pub direct_addr: Option<String>,
    /// 协商出的栈（"bin"/"pb"——1.0 固定 bin 优先）
    pub chosen_codec: String,
}

fn put_tlv(buf: &mut BytesMut, tag: u8, bytes: &[u8]) {
    buf.put_u8(tag);
    buf.put_u16_le(bytes.len() as u16);
    buf.put_slice(bytes);
}

fn read_tlv(payload: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, HandshakeError> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < payload.len() {
        let tag = payload[i];
        if i + 3 > payload.len() {
            return Err(HandshakeError::Truncated(tag, 0, payload.len() - i));
        }
        let len = u16::from_le_bytes([payload[i + 1], payload[i + 2]]) as usize;
        let start = i + 3;
        if start + len > payload.len() {
            return Err(HandshakeError::Truncated(tag, len, payload.len() - start));
        }
        out.push((tag, payload[start..start + len].to_vec()));
        i = start + len;
    }
    Ok(out)
}

/// 取 tag 首个出现项；重复出现 → DuplicateTag。
fn take_one(tlvs: &[(u8, Vec<u8>)], tag: u8) -> Result<Option<&(u8, Vec<u8>)>, HandshakeError> {
    let mut it = tlvs.iter().filter(|(t, _)| *t == tag);
    let first = it.next();
    if first.is_some() && it.next().is_some() {
        return Err(HandshakeError::DuplicateTag(tag));
    }
    Ok(first)
}

fn decode_common(tlvs: &[(u8, Vec<u8>)]) -> Result<HandshakeBody, HandshakeError> {
    let node_id = take_one(tlvs, tlv_tag::NODE_ID)?
        .ok_or(HandshakeError::MissingRequired(tlv_tag::NODE_ID))?;
    let node_id =
        String::from_utf8(node_id.1.clone()).map_err(|_| HandshakeError::Utf8(tlv_tag::NODE_ID))?;
    let capabilities = take_one(tlvs, tlv_tag::CAPABILITIES)?
        .ok_or(HandshakeError::MissingRequired(tlv_tag::CAPABILITIES))?;
    if capabilities.1.len() != 4 {
        return Err(HandshakeError::BadLen(
            tlv_tag::CAPABILITIES,
            capabilities.1.len(),
        ));
    }
    let capabilities = u32::from_le_bytes([
        capabilities.1[0],
        capabilities.1[1],
        capabilities.1[2],
        capabilities.1[3],
    ]);
    let max_frame_len = take_one(tlvs, tlv_tag::MAX_FRAME_LEN)?
        .ok_or(HandshakeError::MissingRequired(tlv_tag::MAX_FRAME_LEN))?;
    if max_frame_len.1.len() != 4 {
        return Err(HandshakeError::BadLen(
            tlv_tag::MAX_FRAME_LEN,
            max_frame_len.1.len(),
        ));
    }
    let max_frame_len = u32::from_le_bytes([
        max_frame_len.1[0],
        max_frame_len.1[1],
        max_frame_len.1[2],
        max_frame_len.1[3],
    ]);
    let role = take_one(tlvs, tlv_tag::TOPOLOGY_ROLE)?
        .ok_or(HandshakeError::MissingRequired(tlv_tag::TOPOLOGY_ROLE))?;
    if role.1.len() != 1 {
        return Err(HandshakeError::BadLen(tlv_tag::TOPOLOGY_ROLE, role.1.len()));
    }
    let topology_role = TopologyRole::from_u8(role.1[0])
        .ok_or(HandshakeError::BadLen(tlv_tag::TOPOLOGY_ROLE, 99))?;
    let realm = match take_one(tlvs, tlv_tag::REALM)? {
        Some((_, v)) => {
            Some(String::from_utf8(v.clone()).map_err(|_| HandshakeError::Utf8(tlv_tag::REALM))?)
        }
        None => None,
    };
    let cluster = match take_one(tlvs, tlv_tag::CLUSTER)? {
        Some((_, v)) => {
            Some(String::from_utf8(v.clone()).map_err(|_| HandshakeError::Utf8(tlv_tag::CLUSTER))?)
        }
        None => None,
    };
    let hop_limit = match take_one(tlvs, tlv_tag::HOP_LIMIT)? {
        Some((_, v)) if v.len() == 1 => v[0],
        Some((_, v)) => return Err(HandshakeError::BadLen(tlv_tag::HOP_LIMIT, v.len())),
        None => crate::frame::DEFAULT_HOP_LIMIT,
    };
    let direct_addr = match take_one(tlvs, tlv_tag::DIRECT_ADDR)? {
        Some((_, v)) if !v.is_empty() => Some(
            String::from_utf8(v.clone()).map_err(|_| HandshakeError::Utf8(tlv_tag::DIRECT_ADDR))?,
        ),
        _ => None,
    };
    Ok(HandshakeBody {
        node_id,
        realm,
        cluster,
        capabilities,
        max_frame_len,
        topology_role,
        hop_limit,
        direct_addr,
    })
}

fn encode_common(b: &HandshakeBody, buf: &mut BytesMut) {
    put_tlv(buf, tlv_tag::NODE_ID, b.node_id.as_bytes());
    if let Some(r) = &b.realm {
        put_tlv(buf, tlv_tag::REALM, r.as_bytes());
    }
    if let Some(c) = &b.cluster {
        put_tlv(buf, tlv_tag::CLUSTER, c.as_bytes());
    }
    put_tlv(buf, tlv_tag::CAPABILITIES, &b.capabilities.to_le_bytes());
    put_tlv(buf, tlv_tag::MAX_FRAME_LEN, &b.max_frame_len.to_le_bytes());
    put_tlv(buf, tlv_tag::TOPOLOGY_ROLE, &[b.topology_role as u8]);
    put_tlv(buf, tlv_tag::HOP_LIMIT, &[b.hop_limit]);
    if let Some(d) = &b.direct_addr {
        put_tlv(buf, tlv_tag::DIRECT_ADDR, d.as_bytes());
    }
}

impl HandshakeBody {
    pub fn encode_tlv(&self, buf: &mut BytesMut) {
        encode_common(self, buf);
    }

    pub fn decode_tlv(payload: &[u8]) -> Result<Self, HandshakeError> {
        let tlvs = read_tlv(payload)?;
        decode_common(&tlvs)
    }
}

impl HandshakeAckBody {
    pub fn encode_tlv(&self, buf: &mut BytesMut) {
        encode_common(
            &HandshakeBody {
                node_id: self.node_id.clone(),
                realm: self.realm.clone(),
                cluster: self.cluster.clone(),
                capabilities: self.capabilities,
                max_frame_len: self.max_frame_len,
                topology_role: self.topology_role,
                hop_limit: self.hop_limit,
                direct_addr: self.direct_addr.clone(),
            },
            buf,
        );
        // ACK 专有：chosen_codec（tag 9——8 已被 DIRECT_ADDR 占用）
        put_tlv(buf, 9, self.chosen_codec.as_bytes());
    }

    pub fn decode_tlv(payload: &[u8]) -> Result<Self, HandshakeError> {
        let tlvs = read_tlv(payload)?;
        let common = decode_common(&tlvs)?;
        let mut chosen = "bin".to_string();
        if let Some((_, v)) = tlvs.iter().find(|(t, _)| *t == 9) {
            chosen = String::from_utf8(v.clone()).map_err(|_| HandshakeError::Utf8(9))?;
        }
        Ok(Self {
            node_id: common.node_id,
            realm: common.realm,
            cluster: common.cluster,
            capabilities: common.capabilities,
            max_frame_len: common.max_frame_len,
            topology_role: common.topology_role,
            hop_limit: common.hop_limit,
            direct_addr: common.direct_addr,
            chosen_codec: chosen,
        })
    }
}

/// 能力协商：按位与；无公共栈 → ErrCode::NoCommonCodec（连接层回 ERROR 断连）。
pub fn negotiate_caps(a: u32, b: u32) -> Result<u32, ErrCode> {
    let common = a & b;
    if common & (caps::BIN | caps::PB) == 0 {
        return Err(ErrCode::NoCommonCodec);
    }
    Ok(common)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HandshakeBody {
        HandshakeBody {
            node_id: "node-a".into(),
            realm: Some("r1".into()),
            cluster: Some("c1".into()),
            capabilities: caps::BIN | caps::ZSTD,
            max_frame_len: 1024 * 1024,
            topology_role: TopologyRole::Hub,
            hop_limit: 6,
            direct_addr: Some("10.0.0.1:9000".into()),
        }
    }

    #[test]
    fn handshake_tlv_roundtrip() {
        let b = sample();
        let mut buf = BytesMut::new();
        b.encode_tlv(&mut buf);
        let decoded = HandshakeBody::decode_tlv(&buf).unwrap();
        assert_eq!(decoded, b);
        // ACK
        let ack = HandshakeAckBody {
            node_id: "node-b".into(),
            realm: None,
            cluster: None,
            capabilities: caps::BIN,
            max_frame_len: 512 * 1024,
            topology_role: TopologyRole::Normal,
            hop_limit: 8,
            direct_addr: None,
            chosen_codec: "bin".into(),
        };
        let mut buf2 = BytesMut::new();
        ack.encode_tlv(&mut buf2);
        assert_eq!(HandshakeAckBody::decode_tlv(&buf2).unwrap(), ack);
    }

    #[test]
    fn handshake_unknown_tag_skipped() {
        let b = sample();
        let mut buf = BytesMut::new();
        b.encode_tlv(&mut buf);
        // 追加未知 tag 99（前向兼容：跳过）
        put_tlv(&mut buf, 99, b"future");
        let decoded = HandshakeBody::decode_tlv(&buf).unwrap();
        assert_eq!(decoded, b);
    }

    #[test]
    fn handshake_missing_required() {
        // 只有 node_id——缺 capabilities/max_frame_len/topology_role
        let mut buf = BytesMut::new();
        put_tlv(&mut buf, tlv_tag::NODE_ID, b"x");
        assert!(matches!(
            HandshakeBody::decode_tlv(&buf),
            Err(HandshakeError::MissingRequired(tlv_tag::CAPABILITIES))
        ));
    }

    #[test]
    fn handshake_duplicate_rejected() {
        let b = sample();
        let mut buf = BytesMut::new();
        b.encode_tlv(&mut buf);
        put_tlv(&mut buf, tlv_tag::NODE_ID, b"again");
        assert!(matches!(
            HandshakeBody::decode_tlv(&buf),
            Err(HandshakeError::DuplicateTag(tlv_tag::NODE_ID))
        ));
    }

    #[test]
    fn handshake_negotiation() {
        // 正常：公共 bin
        assert_eq!(
            negotiate_caps(caps::BIN | caps::PB, caps::BIN),
            Ok(caps::BIN)
        );
        // 无公共栈
        assert_eq!(
            negotiate_caps(caps::BIN, caps::PB),
            Err(ErrCode::NoCommonCodec)
        );
        // zstd 不算 codec 栈
        assert_eq!(
            negotiate_caps(caps::ZSTD, caps::ZSTD),
            Err(ErrCode::NoCommonCodec)
        );
    }

    #[test]
    fn handshake_truncated() {
        assert!(matches!(
            HandshakeBody::decode_tlv(&[1, 0x10, 0]),
            Err(HandshakeError::Truncated(1, 0x10, 0))
        ));
    }

    // ── B2（DEV_09）：caps 扩展位 ──────────────────────────
    #[test]
    fn artifact_caps_bit_values() {
        assert_eq!(caps::ARTIFACTS, 1 << 5);
        assert_eq!(caps::WASM, 1 << 6);
        assert_eq!(caps::DYLIB, 1 << 7);
        // 与既有位互不重叠
        let all = caps::BIN | caps::PB | caps::ZSTD | caps::QUIC | caps::WS;
        assert_eq!(all & (caps::ARTIFACTS | caps::WASM | caps::DYLIB), 0);
    }

    #[test]
    fn negotiate_artifacts_is_orthogonal_to_codec() {
        // ARTIFACTS 只在双方都置位时保留在交集里；不影响 codec 协商成败
        let common =
            negotiate_caps(caps::BIN | caps::ARTIFACTS, caps::BIN | caps::ARTIFACTS).unwrap();
        assert_eq!(common & caps::ARTIFACTS, caps::ARTIFACTS);
        // 单侧置位 → 交集无该位，但仍握手成功（BIN 公共）
        let common = negotiate_caps(caps::BIN | caps::ARTIFACTS, caps::BIN).unwrap();
        assert_eq!(common & caps::ARTIFACTS, 0);
    }
}
