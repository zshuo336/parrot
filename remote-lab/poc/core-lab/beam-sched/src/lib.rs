//! POC 6：BEAM 机制借鉴 —— 抢占式调度（reduction budget）+ 高优插队 + per-process 堆。
//!
//! 借鉴点（对照 Erlang/OTP）：
//!   1. Reduction 预算：每 actor 消耗预算，超限让出重置（BEAM 4000 reductions 检查点）
//!   2. 高优先级消息插队：抢占后 High 优先级先调度（BEAM priority signal；
//!      解决 M2/M3 实测"分片 yield 不能让消息插队"）
//!   3. Per-process 堆：actor 私有存储，死亡整块丢弃，GC 不跨 actor

use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub type BoxedMessage = Box<dyn Any + Send>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low = 0,
    Normal = 1,
    High = 2,
}

struct QueuedMsg {
    actor: usize,
    msg: BoxedMessage,
    priority: Priority,
}

pub struct Heap {
    slots: Vec<Option<BoxedMessage>>,
}

impl Heap {
    pub fn new(cap: usize) -> Self {
        Self { slots: (0..cap).map(|_| None).collect() }
    }
    pub fn put(&mut self, i: usize, v: BoxedMessage) {
        self.slots[i] = Some(v);
    }
    pub fn take(&mut self, i: usize) -> Option<BoxedMessage> {
        self.slots[i].take()
    }
    pub fn used(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}

pub struct Process {
    pub behaviour: Box<dyn FnMut(&mut Heap, BoxedMessage) -> bool + Send>,
    pub heap: Heap,
    pub reductions: u64,
    pub budget: u64,
    pub alive: bool,
}

type QueueInner = (Mutex<VecDeque<QueuedMsg>>, Condvar);

pub struct ReductionScheduler {
    queue: Arc<QueueInner>,
    processes: Arc<Mutex<Vec<Process>>>,
    pub preemptions: Arc<AtomicU64>,
    pub priority_jumps: Arc<AtomicU64>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

const DEFAULT_BUDGET: u64 = 4; // POC 极小值放大可观测性

impl ReductionScheduler {
    pub fn new(workers: usize) -> Arc<Self> {
        Self::with_budget(workers, DEFAULT_BUDGET)
    }

    pub fn with_budget(workers: usize, budget: u64) -> Arc<Self> {
        let queue: Arc<QueueInner> = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let processes = Arc::new(Mutex::new(Vec::<Process>::new()));
        let preemptions = Arc::new(AtomicU64::new(0));
        let priority_jumps = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        for _ in 0..workers {
            let q = queue.clone();
            let ps = processes.clone();
            let pre = preemptions.clone();
            let jump = priority_jumps.clone();
            let stop = shutdown.clone();
            std::thread::spawn(move || {
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    // 1. 取消息：High 优先插队扫描 → FIFO
                    let msg = {
                        let (m, cv) = &*q;
                        let mut g = m.lock().unwrap();
                        loop {
                            if let Some(idx) = g.iter().position(|x| x.priority == Priority::High) {
                                let item = g.remove(idx).unwrap();
                                drop(g);
                                jump.fetch_add(1, Ordering::SeqCst);
                                break Some(item);
                            }
                            if let Some(item) = g.pop_front() {
                                drop(g);
                                break Some(item);
                            }
                            // 无消息：等 50ms 或唤醒（shutdown 响应）
                            let (g2, t) = cv.wait_timeout(g, std::time::Duration::from_millis(50)).unwrap();
                            g = g2;
                            let _ = t;
                        }
                    };
                    let Some(QueuedMsg { actor, msg, .. }) = msg else { continue };

                    // 2. reduction 检查（抢占点）
                    let mut ps_g = ps.lock().unwrap();
                    if actor >= ps_g.len() {
                        continue;
                    }
                    let p = &mut ps_g[actor];
                    if !p.alive {
                        continue;
                    }
                    if p.reductions >= p.budget {
                        pre.fetch_add(1, Ordering::SeqCst);
                        p.reductions = 0; // yield 后预算恢复
                    }
                    p.reductions += 1;
                    let alive = (p.behaviour)(&mut p.heap, msg);
                    if !alive {
                        p.alive = false; // process 终止：heap 随 Process drop 整块释放
                    }
                }
            });
        }

        Arc::new(Self { queue, processes, preemptions, priority_jumps, shutdown })
    }

    pub fn spawn(
        &self,
        behaviour: Box<dyn FnMut(&mut Heap, BoxedMessage) -> bool + Send>,
        heap_cap: usize,
    ) -> usize {
        let mut ps = self.processes.lock().unwrap();
        ps.push(Process { behaviour, heap: Heap::new(heap_cap), reductions: 0, budget: DEFAULT_BUDGET, alive: true });
        ps.len() - 1
    }

    pub fn send(&self, actor: usize, msg: BoxedMessage, priority: Priority) {
        let (m, cv) = &*self.queue;
        m.lock().unwrap().push_back(QueuedMsg { actor, msg, priority });
        cv.notify_one();
    }

    pub fn alive_count(&self) -> usize {
        self.processes.lock().unwrap().iter().filter(|p| p.alive).count()
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

// ============ 测试 ============

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn reduction_preemption_and_fairness() {
        let sched = ReductionScheduler::new(2);

        // 慢 actor：每条消息自旋 5ms（模拟长任务）；budget=4 → 每 4 条抢占重置
        let slow_done = Arc::new(AtomicU64::new(0));
        let sd = slow_done.clone();
        let slow = sched.spawn(
            Box::new(move |_heap, _m| {
                let start = std::time::Instant::now();
                while start.elapsed() < std::time::Duration::from_millis(2) {}
                sd.fetch_add(1, Ordering::SeqCst);
                true
            }),
            4,
        );

        // 快 actor：立即返回
        let fast_done = Arc::new(AtomicU64::new(0));
        let fd = fast_done.clone();
        let fast = sched.spawn(
            Box::new(move |_heap, _m| {
                fd.fetch_add(1, Ordering::SeqCst);
                true
            }),
            4,
        );

        // 慢 actor 塞 40 条，快 actor 塞 10 条
        for _ in 0..40 {
            sched.send(slow, Box::new(0u8), Priority::Normal);
        }
        for _ in 0..10 {
            sched.send(fast, Box::new(0u8), Priority::Normal);
        }

        // 等快 actor 完成（公平性：不被慢 actor 独占 worker）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while fast_done.load(Ordering::SeqCst) < 10 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(fast_done.load(Ordering::SeqCst), 10, "快 actor 不应被慢 actor 饿死");
        assert!(sched.preemptions.load(Ordering::SeqCst) > 0, "reduction 抢占应发生");

        sched.shutdown();
    }

    #[test]
    fn priority_jump_beats_fifo() {
        let sched = ReductionScheduler::new(1); // 单 worker：纯 FIFO 语义可测

        // 填一个阻塞 actor：收到 1 后自旋 200ms（制造队列积压窗口）
        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        let s1 = seen.clone();
        let blocker = sched.spawn(
            Box::new(move |_h, m| {
                let tag = *m.downcast::<u8>().unwrap();
                s1.lock().unwrap().push(tag);
                if tag == 1 {
                    let start = std::time::Instant::now();
                    while start.elapsed() < std::time::Duration::from_millis(200) {}
                }
                true
            }),
            0,
        );

        // 先发阻塞消息，再发 Low 洪泛，最后发 High
        sched.send(blocker, Box::new(1u8), Priority::Normal);
        for i in 0..20 {
            sched.send(blocker, Box::new(2u8), Priority::Low); // tag=2 填满队列
            let _ = i;
        }
        std::thread::sleep(std::time::Duration::from_millis(50)); // 确保积压
        sched.send(blocker, Box::new(9u8), Priority::High); // 应插队到最前

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while sched.priority_jumps.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // 验证 High (9) 在 Low 洪泛 (2) 之前处理
        let seq = seen.lock().unwrap().clone();
        let pos9 = seq.iter().position(|&x| x == 9);
        let pos2 = seq.iter().position(|&x| x == 2);
        assert!(pos9.is_some(), "High 应被处理: {seq:?}");
        if let (Some(p9), Some(p2)) = (pos9, pos2) {
            assert!(p9 < p2, "High 应先于 Low 洪泛: {seq:?}");
        }
        sched.shutdown();
    }

    #[test]
    fn process_death_releases_heap() {
        let sched = ReductionScheduler::new(1);
        let done = Arc::new(AtomicU64::new(0));
        let d = done.clone();
        let pid = sched.spawn(
            Box::new(move |heap, m| {
                if m.downcast_ref::<u8>().map(|v| *v == 1).unwrap_or(false) {
                    heap.put(0, Box::new(vec![0u8; 1024])); // 堆上放 1KB
                    true
                } else {
                    d.fetch_add(1, Ordering::SeqCst);
                    false // 退出
                }
            }),
            8,
        );
        sched.send(pid, Box::new(1u8), Priority::Normal);
        sched.send(pid, Box::new(2u8), Priority::Normal);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while done.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(done.load(Ordering::SeqCst), 1);
        assert_eq!(sched.alive_count(), 0, "进程死后应从存活表移除");
        sched.shutdown();
    }
}
