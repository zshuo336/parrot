//! Versioned durable delivery identity. Authentication is enforced by ingress.
use crate::frame::{BODY_FIXED_OVERHEAD, MAX_FRAME_LEN};
use bytes::Bytes;

pub const DURABLE_KEY: &str = "parrot:durable#v1";
pub const DURABLE_RECEIPT_KEY: &str = "parrot:durable-completed#v1";

/// One receiver-scoped stream. Source is the originating node, session is a
/// persistent caller-chosen stream ID. Sequence starts at one for this receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableEnvelope {
    pub source: String,
    pub session: String,
    pub receiver: String,
    pub path: String,
    pub sequence: u64,
    pub type_key: String,
    pub payload: Bytes,
}
impl DurableEnvelope {
    pub fn validate(&self) -> Result<(), String> {
        for node in [&self.source, &self.receiver] {
            if node.len() > 1024 {
                return Err("durable node identity too long".into());
            }
            parrot_config::endpoint::validate_node_id(node)?;
        }
        if self.sequence == 0
            || self.session.is_empty()
            || self.session.len() > 1024
            || self.session.contains('\0')
            || !self.path.starts_with('/')
            || self.path.len() > 4096
            || self.path.contains('\0')
            || self.type_key.is_empty()
            || self.type_key.len() > 1024
            || matches!(
                self.type_key.as_str(),
                DURABLE_KEY | DURABLE_RECEIPT_KEY | "#ack"
            )
        {
            return Err("invalid durable identity, sequence, path or type key".into());
        }
        let total = self
            .encoded_len()?
            .checked_add(self.target_path().len())
            .and_then(|n| n.checked_add(DURABLE_KEY.len() + BODY_FIXED_OVERHEAD));
        if total.is_none_or(|n| n > MAX_FRAME_LEN as usize) {
            return Err("durable frame budget exceeded".into());
        }
        Ok(())
    }
    pub fn target_path(&self) -> String {
        format!("parrot://{}{}", self.receiver, self.path)
    }
    pub(crate) fn encoded_len(&self) -> Result<usize, String> {
        [
            self.source.len(),
            self.session.len(),
            self.receiver.len(),
            self.path.len(),
            self.type_key.len(),
            self.payload.len(),
        ]
        .into_iter()
        .try_fold(19usize, usize::checked_add)
        .ok_or("durable size overflow".into())
    }
    /// `[version:u8][seq:u64 LE][five u16 LE lengths][UTF-8 fields][payload]`.
    pub fn encode(&self) -> Result<Bytes, String> {
        self.validate()?;
        let fields = [
            &self.source,
            &self.session,
            &self.receiver,
            &self.path,
            &self.type_key,
        ];
        let mut bytes = Vec::with_capacity(self.encoded_len()?);
        bytes.push(1);
        bytes.extend_from_slice(&self.sequence.to_le_bytes());
        for value in fields {
            bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        for value in fields {
            bytes.extend_from_slice(value.as_bytes());
        }
        bytes.extend_from_slice(&self.payload);
        Ok(bytes.into())
    }
    /// Validates every length before allocating strings; shares bounded payload.
    pub fn decode(bytes: Bytes) -> Result<Self, String> {
        if bytes.len() < 19 || bytes.len() > MAX_FRAME_LEN as usize || bytes[0] != 1 {
            return Err("invalid durable envelope header".into());
        }
        let sequence = u64::from_le_bytes(bytes[1..9].try_into().unwrap());
        let mut cursor = 19usize;
        let mut fields = Vec::with_capacity(5);
        for (i, limit) in [1024, 1024, 1024, 4096, 1024].into_iter().enumerate() {
            let length =
                u16::from_le_bytes(bytes[9 + i * 2..11 + i * 2].try_into().unwrap()) as usize;
            let end = cursor
                .checked_add(length)
                .filter(|n| *n <= bytes.len())
                .ok_or("truncated durable identity")?;
            if length == 0 || length > limit {
                return Err("durable field length budget".into());
            }
            fields.push(
                std::str::from_utf8(&bytes[cursor..end])
                    .map_err(|_| "invalid durable UTF-8")?
                    .to_owned(),
            );
            cursor = end;
        }
        let [source, session, receiver, path, type_key]: [String; 5] = fields.try_into().unwrap();
        let envelope = Self {
            source,
            session,
            receiver,
            path,
            type_key,
            sequence,
            payload: bytes.slice(cursor..),
        };
        envelope.validate()?;
        Ok(envelope)
    }
}
