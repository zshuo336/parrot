//! Versioned, receiver-fenced node-to-node TELL sessions; no WAL or retry.
use bytes::{BufMut, Bytes, BytesMut};

pub(crate) mod outbound;
pub(crate) mod recovery;
pub(crate) mod route;
pub(crate) use outbound::Outbound;

pub const CONTROL_KEY: &str = "parrot:ordered-control#v2";
pub const RECEIPT_KEY: &str = "parrot:ordered-control-receipt#v2";
pub const ENDPOINT_KEY: &str = "parrot:ordered-endpoint#v1";
pub const ROUTE_RECEIPT_KEY: &str = "parrot:ordered-route-receipt#v1";
pub(crate) const OPEN_ALIAS: &str = "ordered endpoint resolution required (v1)";
pub(crate) const ACTOR_UNSUPPORTED: &str = "next hop does not support actor sessions";
pub const DATA_KEY: &str = "parrot:ordered-tell#v2";
pub const ADMIT_KEY: &str = "parrot:ordered-admit#v1";
pub const ADMISSION_RECEIPT_KEY: &str = "parrot:ordered-admission-receipt#v1";
pub(crate) const LEASE_MS: u32 = 30_000;
pub(crate) const CHALLENGE_MS: u32 = 5_000;
pub(crate) type Identity = [u8; 16];

pub(crate) fn identity() -> Identity {
    *uuid::Uuid::new_v4().as_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Open,
    OpenAdmission,
    ResolveEndpoint,
    ResolveRoute,
    Commit,
    Close,
    OpenActor,
    CommitActor,
    CloseActor,
    Recover { receiver: Identity, epoch: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Control {
    pub operation: Operation,
    pub source: String,
    pub incarnation: Identity,
    pub stream: Identity,
    pub ticket: Identity,
}

impl Operation {
    pub fn actor_scoped(self) -> bool {
        matches!(self, Self::OpenActor | Self::CommitActor | Self::CloseActor)
    }
}

pub(crate) fn actor_scoped_data(key: &str, payload: &[u8]) -> bool {
    let offset = if key == ADMIT_KEY { 16 } else { 0 };
    payload.get(offset) == Some(&3)
}

pub(crate) fn requires_actor_sessions(frame: &crate::frame::Frame) -> bool {
    match frame.type_key.as_str() {
        CONTROL_KEY => frame
            .borrowed_ask_payload()
            .is_ok_and(|p| matches!(p, [2, 7..=9, ..])),
        ADMIT_KEY | DATA_KEY => frame
            .split_trace_parent()
            .is_ok_and(|(_, p)| actor_scoped_data(&frame.type_key, &p)),
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub active: bool,
    pub receiver: Identity,
    pub ticket: Identity,
    pub generation: u64,
    pub lease_ms: u32,
    pub epoch: Option<u64>,
}

/// The qualified actor path and receiving node incarnation observed by an
/// authenticated lookup. The route may change after the observation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct OrderedEndpoint {
    pub path: String,
    pub receiver: [u8; 16],
}

impl OrderedEndpoint {
    pub(crate) fn encode(&self, challenge: Identity) -> Bytes {
        let mut output = BytesMut::with_capacity(32 + self.path.len());
        output.extend_from_slice(&challenge);
        output.extend_from_slice(&self.receiver);
        output.extend_from_slice(self.path.as_bytes());
        output.freeze()
    }

    pub(crate) fn decode(bytes: &[u8], challenge: Identity) -> Option<Self> {
        if bytes.len() > 32 + 4096 {
            return None;
        }
        let (receiver, path) = Self::fields(bytes, challenge)?;
        crate::actor_path::split(path)?;
        Some(Self {
            receiver,
            path: path.to_owned(),
        })
    }

    fn fields(bytes: &[u8], challenge: Identity) -> Option<(Identity, &str)> {
        if bytes.get(..16)? != challenge {
            return None;
        }
        let receiver = bytes.get(16..32)?.try_into().ok()?;
        nonzero(receiver).ok()?;
        let path = std::str::from_utf8(bytes.get(32..)?).ok()?;
        Some((receiver, path))
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdmissionId {
    pub receiver: Identity,
    pub ticket: Identity,
    pub sequence: u64,
}

impl AdmissionId {
    /// Keep the standard Overloaded error readable by v1 callers. Its bound detail
    /// lets the sender distinguish receiver pre-admission from a generic relay error.
    pub fn capacity_error(&self) -> String {
        self.bound_error("ordered admission capacity exhausted")
    }

    pub fn alias_error(&self) -> String {
        self.bound_error("ordered admission requires the final endpoint")
    }

    pub fn endpoint_error(&self) -> String {
        self.bound_error("ordered endpoint was not acquired")
    }

    fn bound_error(&self, reason: &str) -> String {
        use sha2::{Digest, Sha256};
        let binding = Sha256::digest(self.receipt(false));
        format!("{reason} (v1/{binding:x})")
    }

    pub fn receipt(&self, admitted: bool) -> Bytes {
        let mut bytes = BytesMut::with_capacity(41);
        bytes.put_u8(u8::from(admitted));
        bytes.extend_from_slice(&self.receiver);
        bytes.extend_from_slice(&self.ticket);
        bytes.put_u64_le(self.sequence);
        bytes.freeze()
    }

    pub fn outcome(&self, bytes: &[u8]) -> Option<bool> {
        if bytes.len() != 41
            || bytes[0] > 1
            || bytes[1..17] != self.receiver
            || bytes[17..33] != self.ticket
            || bytes[33..] != self.sequence.to_le_bytes()
        {
            return None;
        }
        Some(bytes[0] == 1)
    }
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
        if matches!(
            self.operation,
            Operation::Open
                | Operation::OpenAdmission
                | Operation::ResolveEndpoint
                | Operation::ResolveRoute
                | Operation::OpenActor
        ) != (self.ticket == [0; 16])
        {
            return Err("ordered control ticket disagrees with operation".into());
        }
        let length = u16::try_from(self.source.len()).map_err(|_| "ordered source too long")?;
        let recovery = if let Operation::Recover { receiver, epoch } = self.operation {
            nonzero(receiver)?;
            if epoch == 0 {
                return Err("zero ordered recovery epoch".into());
            }
            Some((receiver, epoch))
        } else {
            None
        };
        let mut output =
            BytesMut::with_capacity(52 + self.source.len() + recovery.map_or(0, |_| 24));
        output.put_u8(2);
        output.put_u8(match self.operation {
            Operation::Open => 0,
            Operation::Commit => 1,
            Operation::Close => 2,
            Operation::Recover { .. } => 3,
            Operation::OpenAdmission => 4,
            Operation::ResolveEndpoint => 5,
            Operation::ResolveRoute => 6,
            Operation::OpenActor => 7,
            Operation::CommitActor => 8,
            Operation::CloseActor => 9,
        });
        output.extend_from_slice(&self.incarnation);
        output.extend_from_slice(&self.stream);
        output.extend_from_slice(&self.ticket);
        output.put_u16_le(length);
        output.extend_from_slice(self.source.as_bytes());
        if let Some((receiver, epoch)) = recovery {
            output.extend_from_slice(&receiver);
            output.put_u64_le(epoch);
        }
        Ok(output.freeze())
    }

    pub fn decode(input: &[u8]) -> Result<Self, String> {
        if input.len() < 52 || input[0] != 2 {
            return Err("invalid ordered control header".into());
        }
        let length = u16::from_le_bytes(input[50..52].try_into().unwrap()) as usize;
        let recovery_length = if input[1] == 3 { 24 } else { 0 };
        if length > 1024 || input.len() != 52 + length + recovery_length {
            return Err("invalid ordered control length".into());
        }
        let value = Self {
            operation: match input[1] {
                0 => Operation::Open,
                1 => Operation::Commit,
                2 => Operation::Close,
                4 => Operation::OpenAdmission,
                5 => Operation::ResolveEndpoint,
                6 => Operation::ResolveRoute,
                7 => Operation::OpenActor,
                8 => Operation::CommitActor,
                9 => Operation::CloseActor,
                3 => Operation::Recover {
                    receiver: input[52 + length..68 + length].try_into().unwrap(),
                    epoch: u64::from_le_bytes(input[68 + length..].try_into().unwrap()),
                },
                _ => return Err("unknown ordered control operation".into()),
            },
            source: std::str::from_utf8(&input[52..52 + length])
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
        let mut output = BytesMut::with_capacity(46 + self.epoch.map_or(0, |_| 8));
        output.put_u8(2);
        output.put_u8(u8::from(self.active));
        output.extend_from_slice(&self.ticket);
        output.put_u64_le(self.generation);
        output.put_u32_le(self.lease_ms);
        output.extend_from_slice(&self.receiver);
        if let Some(epoch) = self.epoch {
            output.put_u64_le(epoch);
        }
        output.freeze()
    }

    pub fn decode(input: &[u8]) -> Result<Self, String> {
        if !matches!(input.len(), 46 | 54) || input[0] != 2 || input[1] > 1 {
            return Err("invalid ordered receipt header".into());
        }
        let value = Self {
            active: input[1] == 1,
            ticket: input[2..18].try_into().unwrap(),
            generation: u64::from_le_bytes(input[18..26].try_into().unwrap()),
            lease_ms: u32::from_le_bytes(input[26..30].try_into().unwrap()),
            receiver: input[30..46].try_into().unwrap(),
            epoch: (input.len() == 54)
                .then(|| u64::from_le_bytes(input[46..54].try_into().unwrap())),
        };
        nonzero(value.receiver)?;
        if value.epoch == Some(0) {
            return Err("zero ordered receipt epoch".into());
        }
        nonzero(value.ticket)?;
        if value.generation == 0 || !(1..=LEASE_MS).contains(&value.lease_ms) {
            return Err("invalid ordered receipt generation or deadline".into());
        }
        Ok(value)
    }
}

impl Data {
    #[cfg(test)]
    pub fn encode(&self) -> Result<Bytes, String> {
        self.encode_for(None)
    }

    #[cfg(test)]
    pub fn encode_for(&self, receiver: Option<Identity>) -> Result<Bytes, String> {
        self.encode_scoped(receiver, false)
    }

    pub fn encode_scoped(&self, receiver: Option<Identity>, actor: bool) -> Result<Bytes, String> {
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
            .and_then(|n| n.checked_add(if receiver.is_some() { 16 } else { 0 }))
            .and_then(|n| n.checked_add(self.key.len()))
            .and_then(|n| n.checked_add(self.payload.len()))
            .filter(|&n| n <= crate::frame::MAX_FRAME_LEN as usize)
            .ok_or("ordered data exceeds frame budget")?;
        let mut output = BytesMut::with_capacity(length);
        if let Some(receiver) = receiver {
            nonzero(receiver)?;
            output.extend_from_slice(&receiver);
        }
        output.put_u8(if actor { 3 } else { 2 });
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
        if input.len() < 61
            || input.len() > crate::frame::MAX_FRAME_LEN as usize
            || !matches!(input[0], 2 | 3)
        {
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

    pub fn decode_frame(key: &str, payload: Bytes) -> Result<(Option<AdmissionId>, Self), String> {
        if key != ADMIT_KEY {
            return Ok((None, Self::decode(payload)?));
        }
        let receiver: Identity = payload
            .get(..16)
            .ok_or("missing ordered receiver")?
            .try_into()
            .unwrap();
        nonzero(receiver)?;
        let data = Self::decode(payload.slice(16..))?;
        Ok((
            Some(AdmissionId {
                receiver,
                ticket: data.ticket,
                sequence: data.sequence,
            }),
            data,
        ))
    }
}

/// Inspect a captured ordered frame without granting origin authority or
/// replaying session control. The caller still owns replay ordering and policy;
/// this is not a receiver admission or proof of original business execution.
pub fn business_frame(
    mut frame: crate::frame::Frame,
) -> Result<Option<crate::frame::Frame>, String> {
    if !matches!(frame.type_key.as_str(), CONTROL_KEY | DATA_KEY | ADMIT_KEY) {
        return Ok(Some(frame));
    }
    frame
        .validate_payload_flags()
        .map_err(|error| error.to_string())?;
    if frame.type_key == CONTROL_KEY {
        let (_, payload) = frame.split_reply_to().map_err(|error| error.to_string())?;
        let control = Control::decode(&payload)?;
        if control.operation == Operation::ResolveRoute {
            route::discovery(&frame.path, frame.header.hop_count).map_err(|e| e.name())?;
        } else if frame.path.contains('#') {
            if control.operation == Operation::ResolveEndpoint {
                return Err("legacy endpoint lookup requires a canonical path".into());
            }
            route::terminal(&frame.path).map_err(|e| e.name())?;
        }
        return Ok(None);
    }
    let parent = frame
        .take_trace_parent()
        .map_err(|error| error.to_string())?;
    let (admission, data) = Data::decode_frame(&frame.type_key, frame.payload)?;
    if frame.path.contains('#') {
        if admission.is_none() {
            return Err("signed route requires correlated ordered admission".into());
        }
        frame.path = route::terminal(&frame.path)
            .map_err(|e| e.name())?
            .to_owned();
    }
    frame.header.flags &= !crate::frame::flags::TELL_ACK;
    frame.header.correlation_id = 0;
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
    fn receiver_capacity_refusal_has_a_stable_v1_binding() {
        let id = AdmissionId {
            receiver: [2; 16],
            ticket: [3; 16],
            sequence: 4,
        };
        assert_eq!(id.capacity_error(), "ordered admission capacity exhausted (v1/7c4ec89798e6dd0e5e4f6198d4c912b016246b358ee2d12609664b419c571465)");
    }

    #[test]
    fn observed_endpoint_requires_the_exact_challenge_and_canonical_path() {
        let endpoint = OrderedEndpoint {
            path: "parrot://node/target".into(),
            receiver: [1; 16],
        };
        let encoded = endpoint.encode([2; 16]);
        assert_eq!(
            OrderedEndpoint::decode(&encoded, [2; 16]),
            Some(endpoint.clone())
        );
        assert!(OrderedEndpoint::decode(&encoded, [3; 16]).is_none());
        for len in 0..32 {
            assert!(OrderedEndpoint::decode(&encoded[..len], [2; 16]).is_none());
        }
        for path in [
            "/local".to_string(),
            "parrot://node/../x".into(),
            format!("parrot://node/{}", "x".repeat(4096)),
        ] {
            let bytes = OrderedEndpoint {
                path,
                ..endpoint.clone()
            }
            .encode([2; 16]);
            assert!(OrderedEndpoint::decode(&bytes, [2; 16]).is_none());
        }
        let bytes = OrderedEndpoint {
            receiver: [0; 16],
            ..endpoint
        }
        .encode([2; 16]);
        assert!(OrderedEndpoint::decode(&bytes, [2; 16]).is_none());
    }

    #[test]
    fn identity_control_and_receipts_are_versioned_and_exact() {
        for operation in [
            Operation::Open,
            Operation::OpenAdmission,
            Operation::ResolveEndpoint,
            Operation::Commit,
            Operation::Close,
            Operation::Recover {
                receiver: [4; 16],
                epoch: 1,
            },
        ] {
            let value = Control {
                operation,
                source: "origin".into(),
                incarnation: [1; 16],
                stream: [2; 16],
                ticket: if matches!(
                    operation,
                    Operation::Open | Operation::OpenAdmission | Operation::ResolveEndpoint
                ) {
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
            epoch: None,
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
        let extended = Receipt {
            epoch: Some(1),
            ..value
        };
        assert_eq!(extended.encode().len(), 54);
        assert_eq!(Receipt::decode(&extended.encode()).unwrap(), extended);
        for end in 47..54 {
            assert!(Receipt::decode(&extended.encode()[..end]).is_err());
        }
        let invalid = Receipt {
            epoch: Some(0),
            ..value
        };
        assert!(Receipt::decode(&invalid.encode()).is_err());
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
        let route = format!("parrot://host/alias#{}#parrot://end/actor", "a".repeat(64));
        let mut routed = Frame::tell(&route, ADMIT_KEY, data.encode_for(Some([3; 16])).unwrap());
        routed.header.flags = flags::TELL_ACK;
        routed.header.correlation_id = 3;
        let decoded = business_frame(routed.clone()).unwrap().unwrap();
        assert_eq!(decoded.path, "parrot://end/actor");
        assert_eq!(decoded.payload, data.payload);
        routed.path.push('#');
        assert!(business_frame(routed).is_err());
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
