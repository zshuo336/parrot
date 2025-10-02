//! 调度队列方案对比微基准（ADR-16 论证数据）。
//!
//! Q1 集中 SegQueue（现状）：8 worker 全员争抢一个队列
//! Q2 本地队列 + 随机两跳窃取（新）：稳态零争用，仅在本地空时窃取
//!
//! 模拟真实调度负载：8 生产者按泊松间隔投递轻任务，8 worker 消费，
//! 测吞吐与 worker 间均衡度（max/min 消费数）。

use crossbeam_queue::SegQueue;
use parrot::thread::scheduler::steal::StealDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
#[ignore]
fn q_scheduler_queue_showdown() {
    const WORKERS: usize = 8;
    const PRODUCERS: usize = 8;
    const RUN: Duration = Duration::from_secs(3);

    // ---------- 方案 A：集中 SegQueue（现状） ----------
    {
        let q: Arc<SegQueue<u64>> = Arc::new(SegQueue::new());
        let done = Arc::new(AtomicU64::new(0));
        let counts: Vec<Arc<AtomicU64>> = (0..WORKERS).map(|_| Arc::new(AtomicU64::new(0))).collect();
        let mut hs = Vec::new();
        for c in &counts {
            let q = q.clone(); let d = done.clone(); let c = c.clone();
            hs.push(std::thread::spawn(move || {
                while d.load(Ordering::Relaxed) == 0 {
                    if let Some(v) = q.pop() {
                        c.fetch_add(1, Ordering::Relaxed);
                        std::hint::black_box(v.wrapping_mul(3));
                    } else {
                        std::thread::yield_now();
                    }
                }
            }));
        }
        let mut phs = Vec::new();
        for p in 0..PRODUCERS {
            let q = q.clone(); let d = done.clone();
            phs.push(std::thread::spawn(move || {
                let mut i = p as u64;
                while d.load(Ordering::Relaxed) == 0 {
                    i = i.wrapping_add(1);
                    q.push(std::hint::black_box(i));
                    // 模拟消息到达间隔（轻度突发）
                    if i % 3 == 0 { std::thread::sleep(Duration::from_micros(1)); }
                }
            }));
        }
        std::thread::sleep(RUN);
        done.store(1, Ordering::Relaxed);
        for h in phs { let _ = h.join(); }
        for h in hs { let _ = h.join(); }
        let total: u64 = counts.iter().map(|c| c.load(Ordering::Relaxed)).sum();
        let maxc = counts.iter().map(|c| c.load(Ordering::Relaxed)).max().unwrap();
        let minc = counts.iter().map(|c| c.load(Ordering::Relaxed)).min().unwrap();
        println!(
            "[Q-A] central SegQueue: {:.0}k pops/s | balance max/min = {}/{} ({:.2}x)",
            total as f64 / RUN.as_secs_f64() / 1000.0, maxc, minc,
            maxc as f64 / minc.max(1) as f64
        );
    }

    // ---------- 方案 B：本地队列 + 窃取 ----------
    {
        let ring: Arc<Vec<Arc<StealDeque<u64>>>> = Arc::new(
            (0..WORKERS).map(|_| Arc::new(StealDeque::new(4096))).collect());
        let done = Arc::new(AtomicU64::new(0));
        let counts: Vec<Arc<AtomicU64>> = (0..WORKERS).map(|_| Arc::new(AtomicU64::new(0))).collect();
        let mut hs = Vec::new();
        for (i, c) in counts.iter().enumerate() {
            let ring = ring.clone(); let d = done.clone(); let c = c.clone();
            hs.push(std::thread::spawn(move || {
                while d.load(Ordering::Relaxed) == 0 {
                    // 1) 本地 pop（零争用热路径）
                    if let Some(v) = ring[i].pop() {
                        c.fetch_add(1, Ordering::Relaxed);
                        std::hint::black_box(v.wrapping_mul(3));
                    } else {
                        // 2) 窃取（仅在本地空时）：随机两跳
                        let mut stolen = false;
                        for _ in 0..2 {
                            let vidx = (std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|dd| dd.subsec_nanos() as usize).unwrap_or(0)) % WORKERS;
                            if vidx != i {
                                if let Some(v) = ring[vidx].steal() {
                                    c.fetch_add(1, Ordering::Relaxed);
                                    std::hint::black_box(v.wrapping_mul(3));
                                    stolen = true;
                                    break;
                                }
                            }
                        }
                        if !stolen { std::thread::yield_now(); }
                    }
                }
            }));
        }
        let mut phs = Vec::new();
        for p in 0..PRODUCERS {
            let ring = ring.clone(); let d = done.clone();
            phs.push(std::thread::spawn(move || {
                let mut i = p as u64;
                while d.load(Ordering::Relaxed) == 0 {
                    i = i.wrapping_add(1);
                    // 生产者轮询投到各 worker 本地队列（模拟亲和投递）
                    let target = (i as usize) % WORKERS;
                    ring[target].push(std::hint::black_box(i));
                    if i % 3 == 0 { std::thread::sleep(Duration::from_micros(1)); }
                }
            }));
        }
        std::thread::sleep(RUN);
        done.store(1, Ordering::Relaxed);
        for h in phs { let _ = h.join(); }
        for h in hs { let _ = h.join(); }
        let total: u64 = counts.iter().map(|c| c.load(Ordering::Relaxed)).sum();
        let maxc = counts.iter().map(|c| c.load(Ordering::Relaxed)).max().unwrap();
        let minc = counts.iter().map(|c| c.load(Ordering::Relaxed)).min().unwrap();
        println!(
            "[Q-B] local-deque + steal: {:.0}k pops/s | balance max/min = {}/{} ({:.2}x)",
            total as f64 / RUN.as_secs_f64() / 1000.0, maxc, minc,
            maxc as f64 / minc.max(1) as f64
        );
    }
}
