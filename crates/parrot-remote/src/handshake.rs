//! 职责：TLV 握手体编解码 + 能力协商规则（07 §2.4，X3 裁定：一次定死）。
//!
//! payload = TLV 序列 `[u8 tag][u16 len][bytes]`。未知 tag **跳过不报错**
//! （前向兼容，包括重复未知字段）；已知重复 tag / 必填缺失（node_id/capabilities/max_frame_len/
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
    pub const CHOSEN_CODEC: u8 = 9;
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
    /// Completion-acknowledged TELL, including bounded relay return mapping.
    pub const TELL_COMPLETION: u32 = 1 << 8;
    /// Versioned identity envelope and durable receiver completion.
    pub const DURABLE_DELIVERY: u32 = 1 << 9;
    /// Availability-aware measured metrics, admin command 5 / reply 6.
    pub const MEASURED_METRICS: u32 = 1 << 10;
    /// Runtime SWIM membership gossip/control frames. Peers that do not
    /// advertise this bit still receive ordinary transport heartbeats and
    /// application traffic, but are never sent unsolicited gossip frames.
    pub const MEMBERSHIP_GOSSIP: u32 = 1 << 11;
    /// Correlated direct and indirect membership probes on authorized cluster links.
    pub const MEMBERSHIP_PROBES: u32 = 1 << 12;
    /// Versioned causal parent prefix on business ASK/TELL payloads.
    pub const TRACE_CONTEXT: u32 = 1 << 13;
    /// Generation-owned remote Mirror sessions and retirement receipts.
    pub const MIRROR_CONTROL: u32 = 1 << 14;
    /// Fenced compare-and-create mirror allocation, appended protocol variants.
    pub const MIRROR_ALLOCATION: u32 = 1 << 15;
    /// Direct, non-forwardable shadow TELL with raw mailbox admission receipt.
    pub const MIRROR_SHADOW: u32 = 1 << 16;
    /// Owner-bound receptionist snapshots on authorized direct cluster links.
    pub const RECEPTIONIST_SYNC: u32 = 1 << 17;
    /// Lossless native Actix service paths inside receptionist snapshots.
    pub const RECEPTIONIST_NATIVE_PATHS: u32 = 1 << 18;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    #[error("tag {0} has an invalid value")]
    InvalidValue(u8),
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

fn put_tlv(buf: &mut BytesMut, tag: u8, bytes: &[u8]) -> Result<(), HandshakeError> {
    let len = u16::try_from(bytes.len()).map_err(|_| HandshakeError::BadLen(tag, bytes.len()))?;
    buf.put_u8(tag);
    buf.put_u16_le(len);
    buf.put_slice(bytes);
    Ok(())
}

fn validate_common(
    node_id: &str,
    realm: Option<&str>,
    cluster: Option<&str>,
    direct_addr: Option<&str>,
    max_frame_len: u32,
    hop_limit: u8,
) -> Result<(), HandshakeError> {
    for (tag, value) in [
        (tlv_tag::NODE_ID, Some(node_id)),
        (tlv_tag::REALM, realm),
        (tlv_tag::CLUSTER, cluster),
        (tlv_tag::DIRECT_ADDR, direct_addr),
    ] {
        if let Some(value) = value {
            if value.len() > u16::MAX as usize {
                return Err(HandshakeError::BadLen(tag, value.len()));
            }
        }
    }
    if node_id.is_empty() || node_id.contains('\0') {
        return Err(HandshakeError::InvalidValue(tlv_tag::NODE_ID));
    }
    if max_frame_len < crate::frame::BODY_FIXED_OVERHEAD as u32 {
        return Err(HandshakeError::InvalidValue(tlv_tag::MAX_FRAME_LEN));
    }
    if hop_limit == 0 {
        return Err(HandshakeError::InvalidValue(tlv_tag::HOP_LIMIT));
    }
    Ok(())
}

fn validate_chosen(codec: &str, capabilities: u32) -> Result<(), HandshakeError> {
    if codec.len() > u16::MAX as usize {
        return Err(HandshakeError::BadLen(tlv_tag::CHOSEN_CODEC, codec.len()));
    }
    let chosen_cap = match codec {
        "bin" => caps::BIN,
        "pb" => caps::PB,
        _ => return Err(HandshakeError::InvalidValue(tlv_tag::CHOSEN_CODEC)),
    };
    if capabilities & chosen_cap == 0 {
        return Err(HandshakeError::InvalidValue(tlv_tag::CHOSEN_CODEC));
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct TlvField<'a> {
    value: Option<&'a [u8]>,
    duplicate: bool,
}

// Only tags 1..=9 are defined. Storage is independent of untrusted TLV count;
// unknown fields are validated and skipped without copying their payloads.
type TlvFields<'a> = [TlvField<'a>; tlv_tag::CHOSEN_CODEC as usize + 1];

fn read_tlv(payload: &[u8]) -> Result<TlvFields<'_>, HandshakeError> {
    let mut out = [TlvField::default(); tlv_tag::CHOSEN_CODEC as usize + 1];
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
        if (tlv_tag::NODE_ID..=tlv_tag::CHOSEN_CODEC).contains(&tag) {
            let field = &mut out[tag as usize];
            if field.value.is_some() {
                field.duplicate = true;
            } else {
                field.value = Some(&payload[start..start + len]);
            }
        }
        i = start + len;
    }
    Ok(out)
}

// Defer duplicate reporting until after the full structural scan, preserving
// truncated-tail precedence and the decoder's established field-check order.
fn take_one<'a>(tlvs: &TlvFields<'a>, tag: u8) -> Result<Option<&'a [u8]>, HandshakeError> {
    let field = &tlvs[tag as usize];
    if field.duplicate {
        return Err(HandshakeError::DuplicateTag(tag));
    }
    Ok(field.value)
}

fn decode_string(bytes: &[u8], tag: u8) -> Result<String, HandshakeError> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| HandshakeError::Utf8(tag))
}

fn decode_common(tlvs: &TlvFields<'_>) -> Result<HandshakeBody, HandshakeError> {
    let node_id = take_one(tlvs, tlv_tag::NODE_ID)?
        .ok_or(HandshakeError::MissingRequired(tlv_tag::NODE_ID))?;
    let node_id = decode_string(node_id, tlv_tag::NODE_ID)?;
    if node_id.is_empty() || node_id.contains('\0') {
        return Err(HandshakeError::InvalidValue(tlv_tag::NODE_ID));
    }
    let capabilities = take_one(tlvs, tlv_tag::CAPABILITIES)?
        .ok_or(HandshakeError::MissingRequired(tlv_tag::CAPABILITIES))?;
    if capabilities.len() != 4 {
        return Err(HandshakeError::BadLen(
            tlv_tag::CAPABILITIES,
            capabilities.len(),
        ));
    }
    let capabilities = u32::from_le_bytes([
        capabilities[0],
        capabilities[1],
        capabilities[2],
        capabilities[3],
    ]);
    let max_frame_len = take_one(tlvs, tlv_tag::MAX_FRAME_LEN)?
        .ok_or(HandshakeError::MissingRequired(tlv_tag::MAX_FRAME_LEN))?;
    if max_frame_len.len() != 4 {
        return Err(HandshakeError::BadLen(
            tlv_tag::MAX_FRAME_LEN,
            max_frame_len.len(),
        ));
    }
    let max_frame_len = u32::from_le_bytes([
        max_frame_len[0],
        max_frame_len[1],
        max_frame_len[2],
        max_frame_len[3],
    ]);
    if max_frame_len < crate::frame::BODY_FIXED_OVERHEAD as u32 {
        return Err(HandshakeError::InvalidValue(tlv_tag::MAX_FRAME_LEN));
    }
    let role = take_one(tlvs, tlv_tag::TOPOLOGY_ROLE)?
        .ok_or(HandshakeError::MissingRequired(tlv_tag::TOPOLOGY_ROLE))?;
    if role.len() != 1 {
        return Err(HandshakeError::BadLen(tlv_tag::TOPOLOGY_ROLE, role.len()));
    }
    let topology_role =
        TopologyRole::from_u8(role[0]).ok_or(HandshakeError::BadLen(tlv_tag::TOPOLOGY_ROLE, 99))?;
    let realm = match take_one(tlvs, tlv_tag::REALM)? {
        Some(v) => Some(decode_string(v, tlv_tag::REALM)?),
        None => None,
    };
    let cluster = match take_one(tlvs, tlv_tag::CLUSTER)? {
        Some(v) => Some(decode_string(v, tlv_tag::CLUSTER)?),
        None => None,
    };
    let hop_limit = match take_one(tlvs, tlv_tag::HOP_LIMIT)? {
        Some(v) if v.len() == 1 => v[0],
        Some(v) => return Err(HandshakeError::BadLen(tlv_tag::HOP_LIMIT, v.len())),
        None => crate::frame::DEFAULT_HOP_LIMIT,
    };
    if hop_limit == 0 {
        return Err(HandshakeError::InvalidValue(tlv_tag::HOP_LIMIT));
    }
    let direct_addr = match take_one(tlvs, tlv_tag::DIRECT_ADDR)? {
        Some(v) if !v.is_empty() => Some(decode_string(v, tlv_tag::DIRECT_ADDR)?),
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

fn encode_common(b: &HandshakeBody, buf: &mut BytesMut) -> Result<(), HandshakeError> {
    put_tlv(buf, tlv_tag::NODE_ID, b.node_id.as_bytes())?;
    if let Some(r) = &b.realm {
        put_tlv(buf, tlv_tag::REALM, r.as_bytes())?;
    }
    if let Some(c) = &b.cluster {
        put_tlv(buf, tlv_tag::CLUSTER, c.as_bytes())?;
    }
    put_tlv(buf, tlv_tag::CAPABILITIES, &b.capabilities.to_le_bytes())?;
    put_tlv(buf, tlv_tag::MAX_FRAME_LEN, &b.max_frame_len.to_le_bytes())?;
    put_tlv(buf, tlv_tag::TOPOLOGY_ROLE, &[b.topology_role as u8])?;
    put_tlv(buf, tlv_tag::HOP_LIMIT, &[b.hop_limit])?;
    if let Some(d) = &b.direct_addr {
        put_tlv(buf, tlv_tag::DIRECT_ADDR, d.as_bytes())?;
    }
    Ok(())
}

impl HandshakeBody {
    /// Validate the local HELLO before connection I/O or output allocation.
    pub fn validate(&self) -> Result<(), HandshakeError> {
        validate_common(
            &self.node_id,
            self.realm.as_deref(),
            self.cluster.as_deref(),
            self.direct_addr.as_deref(),
            self.max_frame_len,
            self.hop_limit,
        )
    }

    /// Append a complete HELLO. On validation failure `buf` is unchanged.
    pub fn encode_tlv(&self, buf: &mut BytesMut) -> Result<(), HandshakeError> {
        self.validate()?;
        encode_common(self, buf)
    }

    pub fn decode_tlv(payload: &[u8]) -> Result<Self, HandshakeError> {
        let tlvs = read_tlv(payload)?;
        if take_one(&tlvs, tlv_tag::CHOSEN_CODEC)?.is_some() {
            return Err(HandshakeError::InvalidValue(tlv_tag::CHOSEN_CODEC));
        }
        decode_common(&tlvs)
    }
}

impl HandshakeAckBody {
    /// Append a complete ACK. All fields are checked before modifying `buf`.
    pub fn encode_tlv(&self, buf: &mut BytesMut) -> Result<(), HandshakeError> {
        validate_common(
            &self.node_id,
            self.realm.as_deref(),
            self.cluster.as_deref(),
            self.direct_addr.as_deref(),
            self.max_frame_len,
            self.hop_limit,
        )?;
        validate_chosen(&self.chosen_codec, self.capabilities)?;
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
        )?;
        // ACK 专有：chosen_codec（tag 9——8 已被 DIRECT_ADDR 占用）
        put_tlv(buf, tlv_tag::CHOSEN_CODEC, self.chosen_codec.as_bytes())
    }

    pub fn decode_tlv(payload: &[u8]) -> Result<Self, HandshakeError> {
        let tlvs = read_tlv(payload)?;
        let common = decode_common(&tlvs)?;
        let chosen = match take_one(&tlvs, tlv_tag::CHOSEN_CODEC)? {
            Some(v) => decode_string(v, tlv_tag::CHOSEN_CODEC)?,
            None => "bin".to_owned(),
        };
        validate_chosen(&chosen, common.capabilities)?;
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
        b.encode_tlv(&mut buf).unwrap();
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
        ack.encode_tlv(&mut buf2).unwrap();
        assert_eq!(HandshakeAckBody::decode_tlv(&buf2).unwrap(), ack);
    }

    #[test]
    fn handshake_unknown_tag_skipped() {
        let b = sample();
        let mut buf = BytesMut::new();
        b.encode_tlv(&mut buf).unwrap();
        // 追加未知 tag 99（前向兼容：跳过）
        put_tlv(&mut buf, 99, b"future").unwrap();
        let decoded = HandshakeBody::decode_tlv(&buf).unwrap();
        assert_eq!(decoded, b);
    }

    #[test]
    fn handshake_missing_required() {
        // 只有 node_id——缺 capabilities/max_frame_len/topology_role
        let mut buf = BytesMut::new();
        put_tlv(&mut buf, tlv_tag::NODE_ID, b"x").unwrap();
        assert!(matches!(
            HandshakeBody::decode_tlv(&buf),
            Err(HandshakeError::MissingRequired(tlv_tag::CAPABILITIES))
        ));
    }

    #[test]
    fn handshake_duplicate_rejected() {
        let b = sample();
        let mut buf = BytesMut::new();
        b.encode_tlv(&mut buf).unwrap();
        put_tlv(&mut buf, tlv_tag::NODE_ID, b"again").unwrap();
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
