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
