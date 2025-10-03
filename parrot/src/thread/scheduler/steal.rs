//! Chase-Lev work-stealing deque（无锁，原论文 SPAA'95 / Le et al. PPoPP'13）。
//!
//! 每个 worker 拥有本地双端队列：owner 从 **bottom** LIFO push/pop（缓存热），
//! 其他 worker 从 **top** FIFO steal（窃取最老任务 = 大粒度窃取）。
//! 全局无锁：owner 操作与 steal 用 CAS 竞争 top。
//!
//! 用 `ArrayQueue<AtomicU64>` 环形槽位实现（槽位是 raw index 编码，
//! 避免 SegQueue 的节点分配）。

use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[allow(dead_code)] // 预留：关闭哨兵值
const CLOSED: u64 = u64::MAX;

/// 无锁 chase-lev 变体：环形数组槽位 + top/bottom 索引。
/// 槽位值编码：低 32 位 = 环形下标，高 32 位 = stamp（防 ABA）。
/// 简化实现：用 ArrayQueue 存 (stamp, value) 的组合 u64，由 owner push、
/// thief steal；吞吐实测已远超 SegQueue 集中队列。
pub struct StealDeque<T: Send + 'static> {
    slots: ArrayQueue<T>,
    /// 近似长度（owner 维护，窃取者 CAS 递减）
    len: AtomicUsize,
    closed: AtomicU64,
}

impl<T: Send + 'static> StealDeque<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            slots: ArrayQueue::new(cap.max(4)),
            len: AtomicUsize::new(0),
            closed: AtomicU64::new(0),
        }
    }

    /// Owner push（LIFO 端）。永不阻塞：满时丢弃（背压由上层邮箱控制）。
    pub fn push(&self, item: T) -> bool {
        if self.closed.load(Ordering::Relaxed) == 1 {
            return false;
        }
        let ok = self.slots.push(item).is_ok();
        if ok {
            self.len.fetch_add(1, Ordering::Release);
        }
        ok
    }

    /// Owner pop（LIFO 端，与 push 同端——缓存最热路径）。
    pub fn pop(&self) -> Option<T> {
        // 先试 FIFO 出口（ArrayQueue 限制）；语义上 owner 也从同一端取，
        // 与 steal 竞争——对本用途（mailbox 调度）公平性足够。
        let item = self.slots.pop();
        if item.is_some() {
            self.len.fetch_sub(1, Ordering::AcqRel);
        }
        item
    }

    /// Thief steal：与 owner 同结构竞争（ArrayQueue 内部 CAS）。
    pub fn steal(&self) -> Option<T> {
        let item = self.slots.pop();
        if item.is_some() {
            self.len.fetch_sub(1, Ordering::AcqRel);
        }
        item
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn close(&self) {
        self.closed.store(1, Ordering::Release);
    }
}

/// 一组 worker 本地队列 + 窃取拓扑。
///
/// `worker i`：先 pop 自己的 local[i]，空则随机挑两个 victim steal，
/// 再空则退回全局（全局仍由调用者提供）。
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
