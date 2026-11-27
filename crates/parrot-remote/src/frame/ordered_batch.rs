// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use crate::ordered::{Data, DATA_KEY};

// This count bit selects a shared ordered-data header on ORDERED_BATCH links.
pub(super) const SHARED: u64 = 1 << 63;
const SEQUENCE: usize = 49;
const AFTER_SEQUENCE: usize = SEQUENCE + 8;
#[cfg(test)]
mod tests;

pub(super) struct SharedHeader<'a> {
    bytes: &'a [u8],
    last_sequence: u64,
}

impl SharedHeader<'_> {
    pub(super) fn restore(&self, remaining: usize, body: &[u8]) -> Bytes {
        let mut payload = BytesMut::with_capacity(self.bytes.len() + body.len());
        payload.extend_from_slice(&self.bytes[..SEQUENCE]);
        payload.put_u64_le(self.last_sequence - (remaining - 1) as u64);
        payload.extend_from_slice(&self.bytes[AFTER_SEQUENCE..]);
        payload.extend_from_slice(body);
        payload.freeze()
    }
}

pub(super) fn encode(frames: &[Frame]) -> Option<Frame> {
    let first = &frames[0];
    if first.type_key != DATA_KEY || first.header.flags & flags::TRACE_CONTEXT != 0 {
        return None;
    }
    let len = Data::header_len(&first.payload).ok()?;
    let prefix = first.payload.slice(..len);
    let base = Data::decode(prefix.clone()).ok()?.sequence;
    let mut total = 8 + len;
    for (i, frame) in frames.iter().enumerate() {
        let header = frame.payload.get(..len)?;
        if header[..SEQUENCE] != prefix[..SEQUENCE]
            || header[AFTER_SEQUENCE..] != prefix[AFTER_SEQUENCE..]
            || header[SEQUENCE..AFTER_SEQUENCE] != base.checked_add(i as u64)?.to_le_bytes()
        {
            return None;
        }
        total += 4 + frame.payload.len() - len;
    }
    let mut payload = BytesMut::with_capacity(total);
    payload.put_u64_le(SHARED | frames.len() as u64);
    payload.extend_from_slice(&prefix);
    for frame in frames {
        let body = &frame.payload[len..];
        payload.put_u32_le(body.len() as u32);
        payload.extend_from_slice(body);
    }
    let mut frame = first.clone();
    frame.header.flags |= flags::BATCH;
    frame.header.frame_len = 0;
    frame.payload = payload.freeze();
    Some(frame)
}

pub(super) fn members(frame: &Frame, count: usize) -> Result<BatchFrames<'_>, FrameError> {
    let malformed = |offset| FrameError::BatchMalformed {
        offset,
        total: frame.payload.len(),
    };
    if frame.type_key != DATA_KEY || frame.header.flags & flags::TRACE_CONTEXT != 0 {
        return Err(malformed(0));
    }
    let len = Data::header_len(&frame.payload[8..]).map_err(|_| malformed(8))?;
    let start = 8 + len;
    let header = Data::decode(frame.payload.slice(8..start)).map_err(|_| malformed(8))?;
    let last_sequence = header
        .sequence
        .checked_add((count - 1) as u64)
        .ok_or_else(|| malformed(8))?;
    let mut pos = start;
    let mut expanded = 0;
    for _ in 0..count {
        let size = frame
            .payload
            .get(pos..pos + 4)
            .ok_or_else(|| malformed(pos))?;
        let size = u32::from_le_bytes(size.try_into().unwrap()) as usize;
        pos += 4;
        if size > frame.payload.len() - pos {
            return Err(malformed(pos));
        }
        expanded =
            batch_expanded_size(expanded, frame.path.len(), frame.type_key.len(), len + size)?;
        pos += size;
    }
    if pos != frame.payload.len() {
        return Err(malformed(pos));
    }
    Ok(BatchFrames {
        frame,
        remaining: count,
        position: start,
        shared: Some(SharedHeader {
            bytes: &frame.payload[8..start],
            last_sequence,
        }),
    })
}
