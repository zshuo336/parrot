use bytes::BytesMut;
use remote_poc::{install_poc_messages, CodecRegistry, Frame};

// Independently specified wire bytes: body=28, ASK, cid=1, empty path/key/payload.
const EMPTY_ASK: [u8; 32] = [
    28, 0, 0, 0, 1, 16, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0,
];

#[test]
fn malformed_lengths_never_panic_or_consume_next_frame() {
    let mut cases = Vec::new();
    for length in [0u32, 1, 27, 1_048_577, u32::MAX] {
        cases.push(length.to_le_bytes().to_vec());
    }
    for offset in [24, 28] {
        let mut bytes = EMPTY_ASK.to_vec();
        bytes[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&EMPTY_ASK);
        cases.push(bytes);
    }
    for bytes in cases {
        let mut buffer = BytesMut::from(bytes.as_slice());
        let original = buffer.clone();
        assert!(
            Frame::decode(&mut buffer).is_err(),
            "invalid frame accepted: {bytes:?}"
        );
        assert_eq!(buffer, original, "invalid frame must not consume buffer");
    }
}

#[test]
fn version_flags_reserved_and_type_are_validated() {
    for (offset, value) in [(4, 2), (5, 0), (6, 1), (7, 1), (16, 1), (23, 1)] {
        let mut buffer = BytesMut::from(EMPTY_ASK.as_slice());
        buffer[offset] = value;
        assert!(Frame::decode(&mut buffer).is_err(), "offset={offset}");
    }
}

#[test]
fn every_partial_frame_preserves_buffer() {
    for end in 0..EMPTY_ASK.len() {
        let mut buffer = BytesMut::from(&EMPTY_ASK[..end]);
        let original = buffer.clone();
        assert!(Frame::decode(&mut buffer).unwrap().is_none());
        assert_eq!(buffer, original);
        buffer.extend_from_slice(&EMPTY_ASK[end..]);
        assert_eq!(
            Frame::decode(&mut buffer).unwrap().unwrap(),
            Frame::ask(1, "", "", vec![])
        );
        assert!(buffer.is_empty());
    }
}

#[test]
fn add_decoder_requires_exact_payload_length() {
    install_poc_messages();
    for size in [0, 1, 7, 8, 15, 17, 32] {
        assert!(CodecRegistry::decode_incoming("bin:u:Add", &vec![0; size]).is_err());
    }
    assert!(CodecRegistry::decode_incoming("bin:u:Add", &[0; 16]).is_ok());
}

#[test]
fn size_limit_and_invalid_encoding_preserve_output() {
    use remote_poc::MAX_FRAME_BODY;
    let mut output = BytesMut::from(&b"prefix"[..]);
    let original = output.clone();
    let too_large = Frame::ask(1, "", "", vec![0; MAX_FRAME_BODY - 27]);
    assert!(too_large.encode(&mut output).is_err());
    assert_eq!(output, original);
    let maximum = Frame::ask(1, "", "", vec![42; MAX_FRAME_BODY - 28]);
    let mut wire = BytesMut::new();
    maximum.encode(&mut wire).unwrap();
    assert_eq!(wire.len(), MAX_FRAME_BODY + 4);
    assert_eq!(Frame::decode(&mut wire).unwrap().unwrap(), maximum);
    assert!(wire.is_empty());
    let mut invalid_type = Frame::tell("", "", vec![]);
    invalid_type.frame_type = 0xff;
    assert!(invalid_type.encode(&mut output).is_err());
    assert_eq!(output, original);
}

#[test]
fn invalid_utf8_in_each_string_rejects_without_consuming() {
    for offset in [28, 33] {
        let mut wire = BytesMut::new();
        Frame::ask(1, "x", "y", vec![]).encode(&mut wire).unwrap();
        wire[offset] = 0xff;
        let original = wire.clone();
        assert!(Frame::decode(&mut wire).is_err());
        assert_eq!(wire, original);
    }
}
