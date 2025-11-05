//! Preflight reference outputs. Computed before actors or measurement timers
//! start, with joined OS threads. Timed workloads only compare returned values.
use std::collections::HashMap;
use std::sync::Arc;

type Results = HashMap<(u64, u64), u64>;
pub fn reference(iterations: u64, salt: u64) -> u64 {
    let mut value = salt | 1;
    let mut index = 0;
    while index < iterations {
        value = value.wrapping_mul(6_364_136_223_846_793_005);
        value = value.wrapping_add(1_442_695_040_888_963_407);
        value ^= index;
        index += 1;
    }
    value
}
pub fn prepare(workers: usize) -> Arc<Results> {
    let mut keys = Vec::new();
    keys.extend((0..4000).map(|salt| (1000, salt)));
    keys.extend((0..200).map(|salt| (200_000, salt)));
    for actor in 0..64 {
        keys.extend((0..25).map(|k| (200_000, actor * 1000 + k)));
    }
    keys.extend([
        (2_000_000_000, 7),
        (1_000_000_000, 7),
        (30_000_000_000, 1),
        (30_000_000_000, 3),
    ]);
    keys.extend((0..8).map(|i| (57_200_000_000, 0xD00D + i)));
    for actor in 0..4 {
        keys.extend((0..10).map(|k| (2_400_000_000, 0xBACC + actor * 100 + k)));
    }
    keys.extend((0..6).map(|k| (2_400_000_000, 100 + k)));
    for actor in 0..8 {
        keys.extend((0..5000).map(|k| (170_000, actor * 10_000 + k)));
    }
    keys.sort_unstable();
    keys.dedup();
    // Largest first to avoid assigning all long inputs to the final worker.
    keys.sort_unstable_by_key(|(iters, _)| std::cmp::Reverse(*iters));
    let mut assignments = vec![Vec::new(); workers];
    for (i, key) in keys.into_iter().enumerate() {
        assignments[i % workers].push(key);
    }
    let handles: Vec<_> = assignments
        .into_iter()
        .map(|keys| {
            std::thread::spawn(move || {
                keys.into_iter()
                    .map(|(iters, salt)| ((iters, salt), reference(iters, salt)))
                    .collect::<Results>()
            })
        })
        .collect();
    let mut all = Results::new();
    for handle in handles {
        all.extend(handle.join().expect("preflight reference worker"));
    }
    Arc::new(all)
}
