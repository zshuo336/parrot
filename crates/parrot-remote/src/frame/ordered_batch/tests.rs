// SPDX-License-Identifier: AGPL-3.0-only
use super::*;

fn data(n: u64, ticket: u8) -> Frame {
    let data = Data {
        source: "source".into(),
        incarnation: [1; 16],
        stream: [2; 16],
        ticket: [ticket; 16],
        sequence: n,
        key: "bin:test".into(),
        payload: Bytes::from(vec![n as u8; 3]),
    };
    Frame::tell("/a", DATA_KEY, data.encode().unwrap())
}

#[test]
fn contiguous_members_roundtrip_including_last_possible_sequence() {
    let original = [data(u64::MAX - 1, 3), data(u64::MAX, 3)];
    let frame = encode(&original).unwrap();
    assert_eq!(frame.iter_batch().unwrap(), original);
    assert!(encode(&[data(4, 3), data(6, 3)]).is_none());
    assert!(encode(&[data(4, 3), data(5, 4)]).is_none());
    for limit in 0..frame.payload.len() {
        let mut short = frame.clone();
        short.payload.truncate(limit);
        assert!(
            short.batch_frames().is_err(),
            "accepted truncated prefix {limit}"
        );
    }
    let mut wrapped = frame;
    let mut bytes = wrapped.payload.to_vec();
    bytes[8 + SEQUENCE..8 + AFTER_SEQUENCE].copy_from_slice(&u64::MAX.to_le_bytes());
    wrapped.payload = bytes.into();
    assert!(wrapped.batch_frames().is_err());
}
