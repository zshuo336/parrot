use parrot_remote::frame::{flags, Frame, DEFAULT_HOP_LIMIT};
fn tell() -> Frame {
    Frame::tell("/sink", "bin:test", vec![1].into())
}
#[test]
fn mixed_operation_types_are_never_silently_converted() {
    let ask = Frame::ask(1, "/sink", "bin:test", vec![1].into(), None);
    assert!(Frame::batch(&[tell(), ask]).is_err());
}
#[test]
fn batch_preserves_the_strictest_hop_budget() {
    let mut a = tell();
    a.header.hop_limit = 3;
    a.header.hop_count = 1;
    let mut b = tell();
    b.header.hop_limit = 5;
    b.header.hop_count = 2;
    let frame = Frame::batch(&[a, b]).unwrap();
    assert_eq!((frame.header.hop_count, frame.header.hop_limit), (2, 3));
    assert!(frame.header.hop_limit < DEFAULT_HOP_LIMIT);
}
#[test]
fn per_message_sequence_cannot_be_silently_erased() {
    let mut frame = tell();
    frame.header.seq = 7;
    assert!(Frame::batch(&[frame]).is_err());
}
#[test]
fn incompatible_flags_and_versions_are_rejected() {
    let mut b = tell();
    b.header.flags = flags::URGENT;
    assert!(Frame::batch(&[tell(), b]).is_err());
    let mut b = tell();
    b.header.version = 2;
    assert!(Frame::batch(&[tell(), b]).is_err());
}
#[test]
fn nested_batch_cannot_be_flattened_by_clearing_one_flag() {
    let batch = Frame::batch(&[tell()]).unwrap();
    assert!(Frame::batch(&[batch]).is_err());
}
#[test]
fn batch_reader_requires_exact_consumption_and_nonzero_count() {
    let mut frame = Frame::batch(&[tell()]).unwrap();
    let mut bytes = frame.payload.to_vec();
    bytes.push(7);
    frame.payload = bytes.into();
    assert!(frame.iter_batch().is_err());
    frame.payload = vec![0; 8].into();
    assert!(frame.iter_batch().is_err());
}
#[test]
fn batch_rejects_excessive_count_before_allocating_expanded_metadata() {
    let frames = vec![tell(); parrot_remote::frame::MAX_BATCH_ITEMS + 1];
    assert!(Frame::batch(&frames).is_err());
}

#[test]
fn expanded_metadata_has_an_exact_budget_even_when_encoded_frame_is_small() {
    use parrot_remote::frame::{MAX_BATCH_ITEMS, MAX_FRAME_LEN};
    let mut item = tell();
    let size = MAX_FRAME_LEN as usize / MAX_BATCH_ITEMS;
    let path_len = size - std::mem::size_of::<Frame>() - item.type_key.len() - item.payload.len();
    item.path = "x".repeat(path_len);
    let frames = vec![item; MAX_BATCH_ITEMS];
    let mut batch = Frame::batch(&frames).unwrap();
    assert_eq!(batch.iter_batch().unwrap().len(), MAX_BATCH_ITEMS);
    batch.path.push('x');
    assert!(batch.iter_batch().is_err());
    let mut frames = frames;
    frames[0].path.push('x');
    for frame in frames.iter_mut().skip(1) {
        frame.path.push('x');
    }
    assert!(Frame::batch(&frames).is_err());
}
#[test]
fn hostile_batch_counts_lengths_and_outer_semantics_fail_in_preflight() {
    let good = Frame::batch(&[tell()]).unwrap();
    for payload in [u64::MAX.to_le_bytes().to_vec(), {
        let mut bytes = 1u64.to_le_bytes().to_vec();
        bytes.extend(1u64.to_le_bytes());
        bytes.extend(u32::MAX.to_le_bytes());
        bytes
    }] {
        let mut bad = good.clone();
        bad.payload = payload.into();
        assert!(bad.batch_frames().is_err());
    }
    let mut bad = good.clone();
    bad.header.seq = 1;
    assert!(bad.batch_frames().is_err());
    let mut bad = good.clone();
    bad.header.frame_type = parrot_remote::frame::frame_type::STOP;
    assert!(bad.batch_frames().is_err());
    let mut bad = good;
    bad.header.hop_limit = 0;
    assert!(bad.batch_frames().is_err());
}

#[test]
fn urgent_messages_must_not_enter_a_batch() {
    let mut item = tell();
    item.header.flags = flags::URGENT;
    assert!(Frame::batch(&[item]).is_err());
    let mut batch = Frame::batch(&[tell()]).unwrap();
    batch.header.flags |= flags::URGENT;
    assert!(batch.batch_frames().is_err());
}

fn decoded_batch(items: &[Frame]) -> Frame {
    let mut encoded = bytes::BytesMut::new();
    Frame::batch(items).unwrap().encode(&mut encoded).unwrap();
    let batch = Frame::decode(&mut encoded).unwrap().unwrap();
    assert!(encoded.is_empty());
    batch
}

#[test]
fn batch_iterator_tracks_every_member_and_remains_exhausted() {
    use parrot_remote::frame::{frame_type, MAX_BATCH_ITEMS, PROTOCOL_VERSION};

    fn require_iterator_contract<I: ExactSizeIterator<Item = Frame> + std::iter::FusedIterator>(
        _: &I,
    ) {
    }

    for operation in [frame_type::ASK, frame_type::TELL, frame_type::REPLY] {
        for count in [1, 3, MAX_BATCH_ITEMS] {
            let originals: Vec<_> = (0..count)
                .map(|index| {
                    let cid = 900 + index as u64;
                    let payload = vec![index as u8; index % 5].into();
                    let mut item = match operation {
                        frame_type::ASK => Frame::ask(cid, "/sink", "bin:test", payload, None),
                        frame_type::TELL => {
                            let mut item = Frame::tell("/sink", "bin:test", payload);
                            item.header.correlation_id = cid;
                            item
                        }
                        frame_type::REPLY => Frame::reply(cid, "/sink", "bin:test", payload),
                        _ => unreachable!(),
                    };
                    item.header.flags = flags::TRACING;
                    item.header.hop_count = 2;
                    item.header.hop_limit = 5;
                    item
                })
                .collect();
            let batch = decoded_batch(&originals);
            let mut iter = batch.batch_frames().unwrap();
            require_iterator_contract(&iter);
            for (index, original) in originals.iter().enumerate() {
                let remaining = count - index;
                assert_eq!(iter.len(), remaining);
                assert_eq!(iter.size_hint(), (remaining, Some(remaining)));
                let item = iter.next().expect("every declared member must be yielded");
                assert_eq!(item, *original);
                assert_eq!(item.header.version, PROTOCOL_VERSION);
                assert_eq!(item.header.frame_len, 0);
                assert_eq!(iter.len(), remaining - 1);
                assert_eq!(iter.size_hint(), (remaining - 1, Some(remaining - 1)));
            }
            for _ in 0..4 {
                assert_eq!(iter.next(), None);
                assert_eq!(iter.len(), 0);
                assert_eq!(iter.size_hint(), (0, Some(0)));
            }
        }
    }
}

#[test]
fn batch_member_payloads_share_the_wire_buffer_and_outlive_it() {
    let originals: Vec<_> = [3, 129, 8193]
        .into_iter()
        .enumerate()
        .map(|(index, len)| {
            Frame::reply(
                71 + index as u64,
                "/sink",
                "bin:test",
                vec![0xA0 + index as u8; len].into(),
            )
        })
        .collect();
    let batch = decoded_batch(&originals);
    let members: Vec<_> = batch.batch_frames().unwrap().collect();
    // Wire layout: count:u64, then each cid:u64, length:u32 and payload.
    let mut payload_offset = 8;
    for (member, original) in members.iter().zip(&originals) {
        payload_offset += 12;
        let expected_slice =
            &batch.payload[payload_offset..payload_offset + original.payload.len()];
        assert_eq!(member.payload.as_ref(), original.payload.as_ref());
        assert_eq!(member.payload.as_ptr(), expected_slice.as_ptr());
        payload_offset += original.payload.len();
    }
    assert_eq!(payload_offset, batch.payload.len());
    drop(batch);
    for (member, original) in members.iter().zip(&originals) {
        assert_eq!(member.payload, original.payload);
    }
}

#[test]
fn batch_member_metadata_and_collected_results_are_independent() {
    let originals: Vec<_> = (0..3)
        .map(|index| Frame::reply(11 + index, "/sink", "bin:test", vec![index as u8; 7].into()))
        .collect();
    let batch = decoded_batch(&originals);
    let original_batch = batch.clone();
    let mut iter = batch.batch_frames().unwrap();
    let mut first = iter.next().unwrap();
    first.path.push_str("/changed");
    first.type_key.clear();
    first.header.correlation_id = 999;
    first.header.flags |= flags::URGENT;
    first.payload = vec![0xFF].into();
    for original in originals.iter().skip(1) {
        assert_eq!(iter.next().as_ref(), Some(original));
    }
    assert_eq!(iter.next(), None);
    assert_eq!(batch, original_batch);
    assert_eq!(batch.iter_batch().unwrap(), originals);
    assert_eq!(batch.batch_frames().unwrap().collect::<Vec<_>>(), originals);
}
