fn main() {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let worker = std::thread::spawn(move || {
        assert_eq!(receiver.blocking_recv().unwrap(), 42_u64);
    });
    sender.send(42_u64).unwrap();
    worker.join().unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    sender.try_send(7_u64).unwrap();
    assert!(matches!(
        sender.try_send(8_u64),
        Err(tokio::sync::mpsc::error::TrySendError::Full(8))
    ));
    assert_eq!(receiver.blocking_recv(), Some(7));
    drop(sender);
    assert_eq!(receiver.blocking_recv(), None);
    println!("PARROT_MINIMAL_SYNC_PARKING_LOT_CONTRACT_PASSED");
}
