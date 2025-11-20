//! Each integration binary has its own process IO counters.
#![cfg(target_os = "linux")]

use parrot_remote::raft::{DurableRaft, ManualClock, Role};
use std::{sync::Arc, time::Instant};

fn written_bytes() -> u64 {
    let counters = std::fs::read_to_string("/proc/self/io").unwrap();
    counters
        .lines()
        .find_map(|line| line.strip_prefix("wchar: "))
        .expect("Linux process write counter")
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn small_durable_transitions_do_not_rewrite_retained_history() {
    let data = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let open =
        || DurableRaft::open(data.path(), "write-cost", "solo", vec![], clock.clone()).unwrap();
    let mut node = open();
    clock.advance(1400);
    node.tick().unwrap();
    assert_eq!(node.role(), Role::Leader);
    node.propose(vec![37; 1024 * 1024]).unwrap();
    node.committed().unwrap();
    let before = written_bytes();
    let started = Instant::now();
    for value in 0..64u64 {
        assert_eq!(
            node.propose(value.to_le_bytes().to_vec()).unwrap(),
            value + 2
        );
        let applied = node.committed().unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].cmd, value.to_le_bytes());
    }
    let bytes = written_bytes() - before;
    let elapsed = started.elapsed();
    eprintln!("64 synced proposals over 1 MiB history: {bytes} bytes, {elapsed:?}");
    assert!(
        bytes < 4 * 1024 * 1024,
        "small durable transitions rewrote retained history: {bytes} bytes"
    );
    assert_eq!(node.commit_index(), 65);
    drop(node);
    let mut node = open();
    let replay = node.committed().unwrap();
    assert_eq!(replay.len(), 65);
    assert_eq!(replay[0].cmd, vec![37; 1024 * 1024]);
    for (value, entry) in replay[1..].iter().enumerate() {
        assert_eq!(entry.index, value as u64 + 2);
        assert_eq!(entry.cmd, (value as u64).to_le_bytes());
    }
    assert!(node.committed().unwrap().is_empty());
}

#[test]
#[ignore = "run alone to isolate Linux process IO counters"]
fn history_growth_write_cost() {
    for chunks in [0u64, 1, 8] {
        let data = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new());
        let open = || {
            DurableRaft::open(data.path(), "history-cost", "solo", vec![], clock.clone()).unwrap()
        };
        let mut node = open();
        clock.advance(1400);
        node.tick().unwrap();
        for value in 0..chunks {
            node.propose(vec![value as u8 + 1; 1024 * 1024]).unwrap();
            node.committed().unwrap();
        }
        let before = written_bytes();
        let started = Instant::now();
        for value in 0..64u64 {
            assert_eq!(
                node.propose(value.to_le_bytes().to_vec()).unwrap(),
                chunks + value + 1
            );
            let committed = node.committed().unwrap();
            assert_eq!(committed.len(), 1);
            assert_eq!(committed[0].cmd, value.to_le_bytes());
        }
        let elapsed = started.elapsed();
        let bytes = written_bytes() - before;
        eprintln!(
            "RAFT_WRITE_COST history_bytes={} proposals=64 wchar={} elapsed_ns={}",
            chunks * 1024 * 1024,
            bytes,
            elapsed.as_nanos()
        );
        drop(node);
        let mut node = open();
        let replay = node.committed().unwrap();
        assert_eq!(replay.len() as u64, chunks + 64);
        for (value, entry) in replay[..chunks as usize].iter().enumerate() {
            assert_eq!(entry.cmd, vec![value as u8 + 1; 1024 * 1024]);
        }
        for (value, entry) in replay[chunks as usize..].iter().enumerate() {
            assert_eq!(entry.index, chunks + value as u64 + 1);
            assert_eq!(entry.cmd, (value as u64).to_le_bytes());
        }
        assert!(node.committed().unwrap().is_empty());
    }
}
