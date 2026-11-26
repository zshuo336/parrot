//! Versioned, receiver-fenced node-to-node TELL sessions; no WAL or retry.
use bytes::{BufMut, Bytes, BytesMut};

pub(crate) mod outbound;
pub(crate) use outbound::Outbound;

pub const CONTROL_KEY: &str = "parrot:ordered-control#v2";
pub const RECEIPT_KEY: &str = "parrot:ordered-control-receipt#v2";
pub const DATA_KEY: &str = "parrot:ordered-tell#v2";
pub(crate) const LEASE_MS: u32 = 30_000;
pub(crate) const CHALLENGE_MS: u32 = 5_000;
pub(crate) type Identity = [u8; 16];

pub(crate) fn identity() -> Identity {
    *uuid::Uuid::new_v4().as_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Operation {
    Open = 0,
    Commit = 1,
    Close = 2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Control {
    pub operation: Operation,
    pub source: String,
    pub incarnation: Identity,
    pub stream: Identity,
    pub ticket: Identity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub active: bool,
    pub receiver: Identity,
    pub ticket: Identity,
    pub generation: u64,
    pub lease_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Data {
    pub source: String,
    pub incarnation: Identity,
    pub stream: Identity,
    pub ticket: Identity,
    pub sequence: u64,
    pub key: String,
    pub payload: Bytes,
}

fn node(source: &str) -> Result<(), String> {
    parrot_config::endpoint::validate_node_id(source)
}

fn nonzero(id: Identity) -> Result<(), String> {
    (id != [0; 16])
        .then_some(())
        .ok_or_else(|| "zero ordered identity".into())
}

fn key(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 1024
        || value.contains('\0')
        || value.starts_with("parrot:")
    {
        return Err("invalid ordered business type key".into());
    }
    Ok(())
}

impl Control {
    pub fn encode(&self) -> Result<Bytes, String> {
        node(&self.source)?;
        nonzero(self.incarnation)?;
        nonzero(self.stream)?;
        if (self.operation == Operation::Open) != (self.ticket == [0; 16]) {
            return Err("ordered control ticket disagrees with operation".into());
        }
        let length = u16::try_from(self.source.len()).map_err(|_| "ordered source too long")?;
        let mut output = BytesMut::with_capacity(52 + self.source.len());
        output.put_u8(2);
        output.put_u8(self.operation as u8);
        output.extend_from_slice(&self.incarnation);
        output.extend_from_slice(&self.stream);
        output.extend_from_slice(&self.ticket);
        output.put_u16_le(length);
        output.extend_from_slice(self.source.as_bytes());
        Ok(output.freeze())
    }

    pub fn decode(input: &[u8]) -> Result<Self, String> {
        if input.len() < 52 || input[0] != 2 {
            return Err("invalid ordered control header".into());
        }
        let length = u16::from_le_bytes(input[50..52].try_into().unwrap()) as usize;
        if length > 1024 || input.len() != 52 + length {
            return Err("invalid ordered control length".into());
        }
        let value = Self {
            operation: match input[1] {
                0 => Operation::Open,
                1 => Operation::Commit,
                2 => Operation::Close,
                _ => return Err("unknown ordered control operation".into()),
            },
            source: std::str::from_utf8(&input[52..])
                .map_err(|_| "invalid ordered source UTF-8")?
                .into(),
            incarnation: input[2..18].try_into().unwrap(),
            stream: input[18..34].try_into().unwrap(),
            ticket: input[34..50].try_into().unwrap(),
        };
        value.encode()?;
        Ok(value)
    }
}

impl Receipt {
    pub fn encode(&self) -> Bytes {
        let mut output = BytesMut::with_capacity(46);
        output.put_u8(2);
        output.put_u8(u8::from(self.active));
        output.extend_from_slice(&self.ticket);
        output.put_u64_le(self.generation);
        output.put_u32_le(self.lease_ms);
        output.extend_from_slice(&self.receiver);
        output.freeze()
    }

    pub fn decode(input: &[u8]) -> Result<Self, String> {
        if input.len() != 46 || input[0] != 2 || input[1] > 1 {
            return Err("invalid ordered receipt header".into());
        }
        let value = Self {
            active: input[1] == 1,
            ticket: input[2..18].try_into().unwrap(),
            generation: u64::from_le_bytes(input[18..26].try_into().unwrap()),
            lease_ms: u32::from_le_bytes(input[26..30].try_into().unwrap()),
            receiver: input[30..46].try_into().unwrap(),
        };
        nonzero(value.receiver)?;
        nonzero(value.ticket)?;
        if value.generation == 0 || !(1..=LEASE_MS).contains(&value.lease_ms) {
            return Err("invalid ordered receipt generation or deadline".into());
        }
        Ok(value)
    }
}

impl Data {
    pub fn encode(&self) -> Result<Bytes, String> {
        node(&self.source)?;
        nonzero(self.incarnation)?;
        nonzero(self.stream)?;
        nonzero(self.ticket)?;
        key(&self.key)?;
        if self.sequence == 0 {
            return Err("zero ordered sequence".into());
        }
        let source_len = u16::try_from(self.source.len()).map_err(|_| "ordered source too long")?;
        let length = 61usize
            .checked_add(self.source.len())
            .and_then(|n| n.checked_add(self.key.len()))
            .and_then(|n| n.checked_add(self.payload.len()))
            .filter(|&n| n <= crate::frame::MAX_FRAME_LEN as usize)
            .ok_or("ordered data exceeds frame budget")?;
        let mut output = BytesMut::with_capacity(length);
        output.put_u8(2);
        output.extend_from_slice(&self.incarnation);
        output.extend_from_slice(&self.stream);
        output.extend_from_slice(&self.ticket);
        output.put_u64_le(self.sequence);
        output.put_u16_le(source_len);
        output.put_u16_le(self.key.len() as u16);
        output.extend_from_slice(self.source.as_bytes());
        output.extend_from_slice(self.key.as_bytes());
        output.extend_from_slice(&self.payload);
        Ok(output.freeze())
    }

    pub fn decode(input: Bytes) -> Result<Self, String> {
        if input.len() < 61 || input.len() > crate::frame::MAX_FRAME_LEN as usize || input[0] != 2 {
            return Err("invalid ordered data header".into());
        }
        let source_len = u16::from_le_bytes(input[57..59].try_into().unwrap()) as usize;
        let key_len = u16::from_le_bytes(input[59..61].try_into().unwrap()) as usize;
        let end = 61 + source_len + key_len;
        if source_len > 1024 || key_len > 1024 || end > input.len() {
            return Err("invalid ordered data field length".into());
        }
        let source = std::str::from_utf8(&input[61..61 + source_len])
            .map_err(|_| "invalid ordered source UTF-8")?;
        let type_key = std::str::from_utf8(&input[61 + source_len..end])
            .map_err(|_| "invalid ordered key UTF-8")?;
        node(source)?;
        key(type_key)?;
        let value = Self {
            source: source.into(),
            incarnation: input[1..17].try_into().unwrap(),
            stream: input[17..33].try_into().unwrap(),
            ticket: input[33..49].try_into().unwrap(),
            sequence: u64::from_le_bytes(input[49..57].try_into().unwrap()),
            key: type_key.into(),
            payload: input.slice(end..),
        };
        for id in [value.incarnation, value.stream, value.ticket] {
            nonzero(id)?;
        }
        if value.sequence == 0 {
            return Err("zero ordered sequence".into());
        }
        Ok(value)
    }
}

/// Inspect a captured ordered frame without granting origin authority or
/// replaying session control. The caller still owns replay ordering and policy;
/// this is not a receiver admission or proof of original business execution.
pub fn business_frame(
    mut frame: crate::frame::Frame,
) -> Result<Option<crate::frame::Frame>, String> {
    if !matches!(frame.type_key.as_str(), CONTROL_KEY | DATA_KEY) {
        return Ok(Some(frame));
    }
    frame
        .validate_payload_flags()
        .map_err(|error| error.to_string())?;
    if frame.type_key == CONTROL_KEY {
        let (_, payload) = frame.split_reply_to().map_err(|error| error.to_string())?;
        Control::decode(&payload)?;
        return Ok(None);
    }
    let parent = frame
        .take_trace_parent()
        .map_err(|error| error.to_string())?;
    let data = Data::decode(frame.payload)?;
    frame.type_key = data.key;
    frame.payload = data.payload;
    if let Some(parent) = parent {
        frame = frame
            .with_trace_parent(parent)
            .map_err(|error| error.to_string())?;
    }
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_control_and_receipts_are_versioned_and_exact() {
        for operation in [Operation::Open, Operation::Commit, Operation::Close] {
            let value = Control {
                operation,
                source: "origin".into(),
                incarnation: [1; 16],
                stream: [2; 16],
                ticket: if operation == Operation::Open {
                    [0; 16]
                } else {
                    [3; 16]
                },
            };
            let encoded = value.encode().unwrap();
            assert_eq!(Control::decode(&encoded).unwrap(), value);
            for end in 0..encoded.len() {
                assert!(Control::decode(&encoded[..end]).is_err());
            }
            let mut extra = encoded.to_vec();
            extra.push(0);
            assert!(Control::decode(&extra).is_err());
        }
        let value = Receipt {
            active: true,
            receiver: [4; 16],
            ticket: [3; 16],
            generation: 7,
            lease_ms: LEASE_MS,
        };
        assert_eq!(Receipt::decode(&value.encode()).unwrap(), value);
        for end in 0..value.encode().len() {
            assert!(Receipt::decode(&value.encode()[..end]).is_err());
        }
        for offset in [2..18, 30..46] {
            let mut invalid = value.encode().to_vec();
            invalid[offset].fill(0);
            assert!(Receipt::decode(&invalid).is_err());
        }
        let mut invalid = value.encode().to_vec();
        invalid[0] = 1;
        assert!(Receipt::decode(&invalid).is_err());
        let mut invalid = value.encode().to_vec();
        invalid.push(0);
        assert!(Receipt::decode(&invalid).is_err());
        let mut invalid = value.encode().to_vec();
        invalid[1] = 2;
        assert!(Receipt::decode(&invalid).is_err());
    }

    #[test]
    fn data_preserves_exact_business_bytes_and_rejects_identity_corruption() {
        let value = Data {
            source: "origin".into(),
            incarnation: [1; 16],
            stream: [2; 16],
            ticket: [3; 16],
            sequence: u64::MAX,
            key: "bin:example#v1".into(),
            payload: Bytes::from_static(&[0, 255, 1]),
        };
        let encoded = value.encode().unwrap();
        assert_eq!(Data::decode(encoded.clone()).unwrap(), value);
        for end in 0..61 + value.source.len() + value.key.len() {
            assert!(Data::decode(encoded.slice(..end)).is_err());
        }
        for offset in [1..17, 17..33, 33..49, 49..57] {
            let mut invalid = encoded.to_vec();
            invalid[offset].fill(0);
            assert!(Data::decode(invalid.into()).is_err());
        }
        let mut reserved = value;
        reserved.key = CONTROL_KEY.into();
        assert!(reserved.encode().is_err());
    }

    #[test]
    fn inspection_preserves_business_trace_and_rejects_invalid_ordered_frames() {
        use crate::frame::{flags, frame_type, Frame};
        let data = Data {
            source: "origin".into(),
            incarnation: [1; 16],
            stream: [2; 16],
            ticket: [3; 16],
            sequence: 1,
            key: "bin:inspection#v1".into(),
            payload: Bytes::from_static(&[0, 255, 1]),
        };
        let parent = parrot_api::trace::TraceParent::new([5; 16], [7; 8]).unwrap();
        let mut wire = Frame::tell("/actor", DATA_KEY, data.encode().unwrap());
        wire.header.flags = flags::TRACING | flags::URGENT;
        wire = wire.with_trace_parent(parent).unwrap();
        let mut business = business_frame(wire.clone()).unwrap().unwrap();
        assert_eq!(business.take_trace_parent().unwrap(), Some(parent));
        assert_eq!(business.type_key, data.key);
        assert_eq!(business.payload, data.payload);
        assert_eq!(business.header.flags, flags::TRACING | flags::URGENT);
        for mutation in 0..5 {
            let mut invalid = wire.clone();
            match mutation {
                0 => invalid.header.frame_type = frame_type::ASK,
                1 => invalid.header.correlation_id = 1,
                2 => invalid.header.seq = 1,
                3 => invalid.header.flags |= flags::TELL_ACK,
                _ => invalid.payload = Bytes::from_static(&[0]),
            }
            assert!(business_frame(invalid).is_err(), "mutation {mutation}");
        }
        let control = Control {
            operation: Operation::Open,
            source: "origin".into(),
            incarnation: [1; 16],
            stream: [2; 16],
            ticket: [0; 16],
        };
        let frame = Frame::ask(1, "/actor", CONTROL_KEY, control.encode().unwrap(), None);
        assert!(business_frame(frame).unwrap().is_none());
    }
}
