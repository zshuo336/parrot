use parrot_remote::{
    capture::{CaptureDirection, CaptureLimits, CaptureStats, FrameCapture},
    frame::Frame,
};
use std::sync::Arc;

#[test]
fn concurrent_offers_with_reserved_capacity_do_not_drop_on_publication_contention() {
    const WRITERS: usize = 8;
    const ROUNDS: usize = 256;
    const COUNT: usize = WRITERS * ROUNDS;
    let (capture, mut receiver) = FrameCapture::channel(CaptureLimits {
        capacity: COUNT,
        ..Default::default()
    })
    .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(WRITERS));
    std::thread::scope(|scope| {
        for source in 0..WRITERS {
            let barrier = barrier.clone();
            let conn = capture
                .connection(format!("source-{source}"), "mem")
                .unwrap();
            scope.spawn(move || {
                for seq in 0..ROUNDS {
                    let frame = Frame::tell("/sink", "work", format!("{source}:{seq}").into());
                    barrier.wait();
                    conn.observe(CaptureDirection::Rx, &frame);
                }
            });
        }
    });
    capture.close();
    let mut seen = std::collections::BTreeSet::new();
    let mut received = 0;
    while let Some(event) = receiver.blocking_recv() {
        assert_eq!(event.seq, received);
        assert!(seen.insert(event.frame.payload.clone()));
        received += 1;
    }
    assert_eq!(
        received, COUNT as u64,
        "all messages fit both slot and byte budgets"
    );
    assert_eq!(
        receiver.final_stats(),
        CaptureStats {
            observed: COUNT as u64,
            queued: COUNT as u64,
            lost: 0
        }
    );
}
