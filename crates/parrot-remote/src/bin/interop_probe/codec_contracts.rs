//! External wire vectors and typed identity, independent of encode/decode symmetry.
use super::*;
use parrot_api::message::CodecRegistration;
use std::any::TypeId;

fn registration<T: 'static>() -> &'static CodecRegistration {
    inventory::iter::<CodecRegistration>
        .into_iter()
        .find(|entry| entry.type_id == TypeId::of::<T>())
        .unwrap()
}

const ADD: [u8; 16] = [
    8, 7, 6, 5, 4, 3, 2, 1, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
];
const LEFT: u64 = 0x0102_0304_0506_0708;
const RIGHT: u64 = 0x1122_3344_5566_7788;

#[test]
fn add_encode_matches_external_little_endian_vector() {
    let value: BoxedMessage = Box::new(UAdd(LEFT, RIGHT));
    assert_eq!((registration::<UAdd>().encode)(&value).unwrap(), ADD);
}

#[test]
fn add_decode_preserves_both_values_and_request_type() {
    let value = (registration::<UAdd>().decode)(&ADD).unwrap();
    let request = value
        .downcast_ref::<UAdd>()
        .expect("Add must decode to its registered request type");
    assert_eq!((request.0, request.1), (LEFT, RIGHT));
    assert!(value.downcast_ref::<UAddR>().is_none());
}

#[test]
fn add_rejects_all_truncated_lengths_without_panicking() {
    for length in 0..16 {
        assert!(
            (registration::<UAdd>().decode)(&ADD[..length]).is_err(),
            "accepted {length} bytes"
        );
    }
}

#[test]
fn add_rejects_unconsumed_suffix() {
    for suffix in [vec![0], vec![0xff; 16], vec![0; 1024]] {
        let mut bytes = ADD.to_vec();
        bytes.extend(suffix);
        assert!((registration::<UAdd>().decode)(&bytes).is_err());
    }
}

#[test]
fn add_roundtrip_retains_unsigned_boundaries_and_operand_order() {
    for (left, right) in [
        (0, 0),
        (0, u64::MAX),
        (u64::MAX, 0),
        (u64::MAX, u64::MAX),
        (251, 65536),
    ] {
        let input: BoxedMessage = Box::new(UAdd(left, right));
        let bytes = (registration::<UAdd>().encode)(&input).unwrap();
        let value = (registration::<UAdd>().decode)(&bytes).unwrap();
        let request = value.downcast_ref::<UAdd>().expect("request identity");
        assert_eq!((request.0, request.1), (left, right));
    }
}

#[test]
fn unary_codecs_use_exact_eight_byte_width_and_correct_concrete_types() {
    for n in [0, 251, 65536, u64::MAX] {
        for value in [
            Box::new(UPing(n)) as BoxedMessage,
            Box::new(UPong(n)),
            Box::new(UAddR(n)),
        ] {
            let entry = inventory::iter::<CodecRegistration>
                .into_iter()
                .find(|entry| entry.type_id == value.as_ref().type_id())
                .unwrap();
            assert_eq!((entry.encode)(&value).unwrap(), n.to_le_bytes());
            let decoded = (entry.decode)(&n.to_le_bytes()).unwrap();
            assert_eq!(decoded.as_ref().type_id(), value.as_ref().type_id());
            let actual = if let Some(v) = decoded.downcast_ref::<UPing>() {
                v.0
            } else if let Some(v) = decoded.downcast_ref::<UPong>() {
                v.0
            } else {
                decoded.downcast_ref::<UAddR>().unwrap().0
            };
            assert_eq!(actual, n);
            for length in [0, 1, 7, 9, 16] {
                assert!((entry.decode)(&vec![0; length]).is_err());
            }
        }
    }
}

#[test]
fn encoders_reject_foreign_concrete_values() {
    let foreign: BoxedMessage = Box::new(String::from("not a probe"));
    for entry in [
        registration::<UPing>(),
        registration::<UPong>(),
        registration::<UAdd>(),
        registration::<UAddR>(),
    ] {
        assert!((entry.encode)(&foreign).is_err());
    }
}
