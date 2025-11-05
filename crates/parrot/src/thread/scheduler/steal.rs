//! Bounded FIFO queues for local consumption and stealing.
//!
//! ArrayQueue supports concurrent producers and consumers; both `pop` and
//! `steal` remove from the FIFO head. This is not a Chase-Lev owner-only LIFO
//! deque. The ring is a public utility, not the current SharedPool scheduler's
//! production work-distribution path.

use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[allow(dead_code)] // 预留：关闭哨兵值
const CLOSED: u64 = u64::MAX;

/// Bounded concurrent FIFO; producer rejection drops the submitted item.
/// Length and emptiness are snapshots from the same underlying queue, not
/// an independently updated counter. Concurrent operations can change them
/// immediately after a call returns.
pub struct StealDeque<T: Send + 'static> {
    slots: ArrayQueue<T>,
    closed: AtomicU64,
}

impl<T: Send + 'static> StealDeque<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            slots: ArrayQueue::new(cap.max(4)),
            closed: AtomicU64::new(0),
        }
    }

    /// Push at the FIFO tail. Full or closed queues reject and drop the item.
    pub fn push(&self, item: T) -> bool {
        if self.closed.load(Ordering::Relaxed) == 1 {
            return false;
        }
        self.slots.push(item).is_ok()
    }

    /// Remove the oldest accepted item, competing with other consumers.
    pub fn pop(&self) -> Option<T> {
        self.slots.pop()
    }

    /// Steal uses the same FIFO head as local consumption.
    pub fn steal(&self) -> Option<T> {
        self.slots.pop()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn close(&self) {
        self.closed.store(1, Ordering::Release);
    }
}

/// 一组 worker 本地队列 + 窃取拓扑。
///
/// `worker i` can consume `local[i]` and scan other queues from a clock-derived
/// starting index. Any global fallback is supplied by its caller.
pub struct StealRing {
    deques: Vec<Arc<StealDeque<Arc<dyn crate::thread::mailbox::Mailbox + Send + Sync>>>>,
}

pub type MailboxRef = Arc<dyn crate::thread::mailbox::Mailbox + Send + Sync>;

impl StealRing {
    pub fn new(workers: usize, cap: usize) -> Self {
        Self {
            deques: (0..workers)
                .map(|_| Arc::new(StealDeque::new(cap)))
                .collect(),
        }
    }

    pub fn local(&self, i: usize) -> &Arc<StealDeque<MailboxRef>> {
        &self.deques[i % self.deques.len()]
    }

    /// 尝试从 other 的本地队列偷一个（跳过 self）。
    pub fn steal_from_others(&self, self_idx: usize) -> Option<MailboxRef> {
        let n = self.deques.len();
        // 随机起点：读一个便宜时钟做伪随机
        let start = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as usize)
            .unwrap_or(0))
            % n;
        for k in 0..n {
            let idx = (start + k) % n;
            if idx == self_idx {
                continue;
            }
            if let Some(m) = self.deques[idx].steal() {
                return Some(m);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_steal_basic() {
        let d = StealDeque::new(8);
        assert!(d.push(1));
        assert!(d.push(2));
        assert_eq!(d.pop(), Some(1));
        assert_eq!(d.steal(), Some(2));
        assert!(d.is_empty());
    }

    #[test]
    fn concurrent_steal_no_loss_no_dup() {
        const ITEMS: usize = 50_000;
        let d = Arc::new(StealDeque::new(ITEMS + 16));
        for i in 0..ITEMS {
            assert!(d.push(i));
        }
        let mut hs = Vec::new();
        for t in 0..4 {
            let d = d.clone();
            hs.push(std::thread::spawn(move || {
                let mut got = Vec::new();
                while let Some(v) = if t == 0 { d.pop() } else { d.steal() } {
                    got.push(v);
                }
                got
            }));
        }
        let mut all = Vec::new();
        for h in hs {
            all.extend(h.join().unwrap());
        }
        // 无丢失、无重复
        all.sort_unstable();
        let expected: Vec<usize> = (0..ITEMS).collect();
        assert_eq!(all, expected);
    }
}
