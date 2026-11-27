// SPDX-License-Identifier: AGPL-3.0-only
use super::FrameSender;
use crate::frame::{flags, frame_type, BatchLayout, Frame};
use std::time::Duration;
use tokio::{sync::mpsc, time::Instant};

const MAX_MEMBERS: usize = 64;
const WINDOW: Duration = Duration::from_millis(5);

/// Owned by one physical writer. A heartbeat can interrupt recv without losing
/// its partial batch, deadline or the first incompatible application frame.
pub(super) struct TellBatcher {
    enabled: bool,
    ordered: bool,
    max_body: u32,
    members: Vec<Frame>,
    layout: BatchLayout,
    deadline: Instant,
    next: Option<Frame>,
}

impl TellBatcher {
    pub(super) fn new(link: &FrameSender) -> Self {
        Self {
            enabled: link.supports_batch_frames(),
            ordered: link.supports_ordered_batch(),
            max_body: link.max_frame_len,
            members: Vec::new(),
            layout: BatchLayout::new(),
            deadline: Instant::now(),
            next: None,
        }
    }

    fn eligible(&self, frame: &Frame) -> bool {
        if frame.header.frame_type != frame_type::TELL || frame.header.seq != 0 {
            return false;
        }
        let mode = frame.header.flags & !(flags::TRACING | flags::TRACE_CONTEXT);
        match frame.type_key.as_str() {
            crate::ordered::ADMIT_KEY => {
                self.ordered && mode == flags::TELL_ACK && frame.header.correlation_id != 0
            }
            crate::ordered::DATA_KEY if !self.ordered => false,
            _ => mode == 0 && frame.header.correlation_id == 0,
        }
    }

    pub(super) async fn recv(&mut self, rx: &mut mpsc::Receiver<Frame>) -> Option<Frame> {
        if !self.enabled {
            return rx.recv().await;
        }
        if self.members.is_empty() {
            let first = match self.next.take() {
                Some(frame) => frame,
                None => rx.recv().await?,
            };
            if !self.eligible(&first) || self.layout.push(&first, &first, self.max_body).is_err() {
                return Some(first);
            }
            self.deadline = Instant::now() + WINDOW;
            self.members.push(first);
        }
        while self.members.len() < MAX_MEMBERS {
            // A synchronous admission must never incur a batching timer per hop.
            let frame = if self.members[0].header.flags & flags::TELL_ACK != 0 {
                match rx.try_recv() {
                    Ok(frame) => frame,
                    Err(_) => break,
                }
            } else {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(self.deadline) => break,
                    value = rx.recv() => match value { Some(frame) => frame, None => break },
                }
            };
            let first = &self.members[0];
            if !self.eligible(&frame)
                || first.header.hop_count != frame.header.hop_count
                || first.header.hop_limit != frame.header.hop_limit
                || self.layout.push(first, &frame, self.max_body).is_err()
            {
                self.next = Some(frame);
                break;
            }
            self.members.push(frame);
        }
        let frame = if self.members.len() == 1 {
            self.members.pop().expect("one buffered frame")
        } else {
            let frame = self.layout.encode_outbound(&self.members);
            self.members.clear();
            frame
        };
        self.layout = BatchLayout::new();
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::caps;
    use bytes::Bytes;
    use futures::FutureExt;

    fn buffer(
        caps: u32,
        max_body: u32,
    ) -> (TellBatcher, mpsc::Sender<Frame>, mpsc::Receiver<Frame>) {
        let (tx, rx) = mpsc::channel(128);
        let mut link = FrameSender::capability_test_sender(tx.clone(), "peer", caps);
        link.max_frame_len = max_body;
        (TellBatcher::new(&link), tx, rx)
    }
    fn sample(n: u8) -> Frame {
        Frame::tell("/a", "t", Bytes::from(vec![n]))
    }
    #[tokio::test(start_paused = true)]
    async fn cancelled_poll_keeps_first_deadline_and_sixty_four_flush_without_wait() {
        let (mut batch, tx, mut rx) = buffer(caps::BATCH_FRAMES, 2048);
        tx.try_send(sample(1)).unwrap();
        assert!(batch.recv(&mut rx).now_or_never().is_none());
        tokio::time::advance(Duration::from_millis(4)).await;
        tx.try_send(sample(2)).unwrap();
        assert!(batch.recv(&mut rx).now_or_never().is_none());
        tokio::time::advance(Duration::from_millis(1)).await;
        let frame = batch.recv(&mut rx).now_or_never().unwrap().unwrap();
        assert_eq!(frame.iter_batch().unwrap(), [sample(1), sample(2)]);
        for n in 0..65 {
            tx.try_send(sample(n)).unwrap();
        }
        let frame = batch.recv(&mut rx).now_or_never().unwrap().unwrap();
        assert_eq!(
            frame.iter_batch().unwrap(),
            (0..64).map(sample).collect::<Vec<_>>()
        );
        drop(tx);
        assert_eq!(batch.recv(&mut rx).await, Some(sample(64)));
        assert!(batch.recv(&mut rx).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn negotiated_size_and_incompatible_frames_split_without_overtaking() {
        // 28-byte header, 3 bytes of path/key, 8-byte count, 13 bytes per member.
        let (mut batch, tx, mut rx) = buffer(caps::BATCH_FRAMES, 65);
        let mut urgent = sample(4);
        urgent.header.flags = flags::URGENT;
        let mut hopped = sample(7);
        hopped.header.hop_count = 1;
        for f in [
            sample(1),
            sample(2),
            sample(3),
            urgent.clone(),
            sample(5),
            sample(6),
            hopped.clone(),
        ] {
            tx.try_send(f).unwrap();
        }
        drop(tx);
        assert_eq!(
            batch.recv(&mut rx).await.unwrap().iter_batch().unwrap(),
            [sample(1), sample(2)]
        );
        assert_eq!(batch.recv(&mut rx).await, Some(sample(3)));
        assert_eq!(batch.recv(&mut rx).await, Some(urgent));
        assert_eq!(
            batch.recv(&mut rx).await.unwrap().iter_batch().unwrap(),
            [sample(5), sample(6)]
        );
        assert_eq!(batch.recv(&mut rx).await, Some(hopped));
    }

    #[tokio::test]
    async fn ordered_members_require_each_feature_even_inside_negotiated_batch() {
        let (tx, _) = mpsc::channel(1);
        let old = caps::ORDERED_TELL
            | caps::BATCH_FRAMES
            | caps::TELL_COMPLETION
            | caps::ORDERED_ADMISSION;
        let ordinary = Frame::tell("/a", crate::ordered::DATA_KEY, Bytes::from_static(&[2]));
        let actor = Frame::tell("/a", crate::ordered::DATA_KEY, Bytes::from_static(&[4]));
        let mixed = Frame::batch(&[ordinary.clone(), actor]).unwrap();
        for (caps, plain, actors) in [
            (old, false, false),
            (old | caps::ORDERED_BATCH, true, false),
            (
                old | caps::ORDERED_BATCH | caps::ORDERED_ACTOR | caps::ORDERED_SENDER,
                true,
                true,
            ),
        ] {
            let link = FrameSender::capability_test_sender(tx.clone(), "peer", caps);
            let mut batch = TellBatcher::new(&link);
            let (queue, mut rx) = mpsc::channel(4);
            queue.try_send(ordinary.clone()).unwrap();
            queue.try_send(ordinary.clone()).unwrap();
            drop(queue);
            let first = batch.recv(&mut rx).await.unwrap();
            assert_eq!(first.is_ordered_batch(), plain);
            assert_eq!(link.check_negotiated_features(&mixed).is_ok(), actors);
        }
    }
}
