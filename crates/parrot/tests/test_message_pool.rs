//! 消息池化（ADR-15）微基准：池化 acquire/drop vs 全局 allocator Box。
//!
//! P1 串行分配/释放吞吐
//! P2 并发分配/释放吞吐（4 线程，thread-local 池零争用 vs 全局 malloc 争用）
//! P3 引擎集成：池化消息走 ask 往返 vs 普通 Box 消息

mod engine_stress_common;

use parrot::thread::message_pool::{self, Pooled};
use std::time::Instant;

#[derive(Clone)]
struct PooledEcho {
    value: u64,
    _pad: [u8; 64],
}
impl Default for PooledEcho {
    fn default() -> Self {
        Self {
            value: 0,
            _pad: [0; 64],
        }
    }
}

#[test]
#[ignore]
fn p1_p2_pool_vs_allocator() {
    // P1: 串行 1M 次 acquire(写值+drop) vs Box::new(写值+drop)
    message_pool::clear_local();
    const N: u64 = 1_000_000;

    // allocator 基线（black_box 防止 LLVM 消除死分配）
    let t0 = Instant::now();
    let mut sink = 0u64;
    for i in 0..N {
        let mut b: Box<PooledEcho> = std::hint::black_box(Box::new(PooledEcho::default()));
        b.value = i;
        sink = sink.wrapping_add(std::hint::black_box(&b).value);
    }
    let alloc_wall = t0.elapsed();

    // 池化（首次填充后进入稳态复用）
    let t1 = Instant::now();
    for i in 0..N {
        let mut p: Pooled<PooledEcho> = message_pool::acquire();
        p.value = i;
        sink = sink.wrapping_add(std::hint::black_box(&p).value);
    }
    let pool_wall = t1.elapsed();
    let st = message_pool::stats();

    println!(
        "[P1] serial 1M alloc+drop: allocator={:.1}ms pool={:.1}ms speedup={:.2}x (fresh={} reused={} overflow={}) sink={}",
        alloc_wall.as_secs_f64() * 1000.0,
        pool_wall.as_secs_f64() * 1000.0,
        alloc_wall.as_secs_f64() / pool_wall.as_secs_f64(),
        st.acquired_fresh,
        st.acquired_reused,
        st.dropped_overflow,
        sink % 2,
    );

    // P2: 4 线程并发
    const T: usize = 4;
    const M: u64 = 500_000;
    let t2 = Instant::now();
    let mut hs = Vec::new();
    for t in 0..T {
        hs.push(std::thread::spawn(move || {
            let mut sink = 0u64;
            for i in 0..M {
                let mut b: Box<PooledEcho> = std::hint::black_box(Box::new(PooledEcho::default()));
                b.value = i + t as u64;
                sink = sink.wrapping_add(std::hint::black_box(&b).value);
            }
            sink
        }));
    }
    let mut sink2 = 0u64;
    for h in hs {
        sink2 = sink2.wrapping_add(h.join().unwrap());
    }
    let c_alloc = t2.elapsed();

    message_pool::clear_local();
    let t3 = Instant::now();
    let mut hs = Vec::new();
    for t in 0..T {
        hs.push(std::thread::spawn(move || {
            let mut sink = 0u64;
            for i in 0..M {
                let mut p: Pooled<PooledEcho> = message_pool::acquire();
                p.value = i + t as u64;
                sink = sink.wrapping_add(std::hint::black_box(&p).value);
            }
            sink
        }));
    }
    let mut sink3 = 0u64;
    for h in hs {
        sink3 = sink3.wrapping_add(h.join().unwrap());
    }
    let c_pool = t3.elapsed();
    println!(
        "[P2] 4-thread {} alloc+drop: allocator={:.1}ms pool={:.1}ms speedup={:.2}x sink={}/{}",
        T * M as usize,
        c_alloc.as_secs_f64() * 1000.0,
        c_pool.as_secs_f64() * 1000.0,
        c_alloc.as_secs_f64() / c_pool.as_secs_f64(),
        sink2 % 2,
        sink3 % 2,
    );
}
