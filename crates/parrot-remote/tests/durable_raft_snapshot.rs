use parrot_remote::raft::{
    DurableRaft, LogEntry, ManualClock, RaftRpc, RaftSnapshot, RaftStorageError, Role,
    SnapshotError,
};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};

fn open(path: &Path, clock: &Arc<ManualClock>, peers: Vec<String>) -> DurableRaft {
    DurableRaft::open(path, "snapshots", "n", peers, clock.clone()).unwrap()
}
fn leader(path: &Path) -> (Arc<ManualClock>, DurableRaft) {
    let clock = Arc::new(ManualClock::new());
    let mut node = open(path, &clock, vec![]);
    clock.advance(1400);
    node.tick().unwrap();
    assert_eq!(node.role(), Role::Leader);
    (clock, node)
}
fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path.join("raft.state")).unwrap()
}
fn chunks(index: u64, snapshot_term: u64, term: u64, state: &[u8]) -> Vec<RaftRpc> {
    let mut hash = Sha256::new();
    hash.update(b"parrot:raft:snapshot#v1");
    hash.update(index.to_le_bytes());
    hash.update(snapshot_term.to_le_bytes());
    hash.update(state);
    let digest = hash.finalize().into();
    let pieces: Vec<&[u8]> = if state.is_empty() {
        vec![&[]]
    } else {
        state.chunks(64 * 1024).collect()
    };
    pieces
        .into_iter()
        .enumerate()
        .map(|(slot, data)| RaftRpc::InstallSnapshot {
            term,
            leader: "p".into(),
            index,
            snapshot_term,
            digest,
            offset: (slot * 64 * 1024) as u64,
            total: state.len() as u64,
            data: data.to_vec(),
        })
        .collect()
}
fn append(term: u64, entries: Vec<LogEntry>, commit: u64) -> RaftRpc {
    RaftRpc::AppendEntries {
        term,
        leader: "p".into(),
        prev_log_index: 0,
        prev_log_term: 0,
        entries,
        leader_commit: commit,
    }
}

#[test]
fn local_compaction_reopens_snapshot_then_absolute_tail_and_new_term() {
    let data = tempfile::tempdir().unwrap();
    let (clock, mut node) = leader(data.path());
    for value in 1..=6 {
        assert_eq!(node.propose(vec![value]).unwrap(), u64::from(value));
    }
    assert!(matches!(
        node.compact(4, vec![]),
        Err(RaftStorageError::Snapshot(SnapshotError::NotApplied))
    ));
    node.committed().unwrap();
    node.compact(4, b"state-four".to_vec()).unwrap();
    assert!(bytes(data.path()).starts_with(b"PRAFT03\0"));
    assert_eq!(
        node.log().iter().map(|e| e.index).collect::<Vec<_>>(),
        [5, 6]
    );
    assert!(node.pending_snapshot().unwrap().is_none());
    assert_eq!(node.propose(vec![7]).unwrap(), 7);
    drop(node);
    let mut node = open(data.path(), &clock, vec![]);
    assert_eq!(
        node.pending_snapshot().unwrap().unwrap().data,
        b"state-four"
    );
    assert!(node.committed().unwrap().is_empty());
    assert!(node.snapshot_applied(4, 2).is_err());
    node.snapshot_applied(4, 1).unwrap();
    assert_eq!(
        node.committed()
            .unwrap()
            .iter()
            .map(|e| e.index)
            .collect::<Vec<_>>(),
        [5, 6, 7]
    );
    clock.advance(1400);
    node.tick().unwrap();
    assert_eq!(node.propose(vec![8]).unwrap(), 8);
    assert_eq!(node.current_term(), 2);
    node.committed().unwrap();
    node.compact(8, b"state-eight".to_vec()).unwrap();
    drop(node);
    let mut node = open(data.path(), &clock, vec![]);
    assert_eq!(node.pending_snapshot().unwrap().unwrap().term, 2);
    node.snapshot_applied(8, 2).unwrap();
    assert!(node.committed().unwrap().is_empty());
    clock.advance(1400);
    node.tick().unwrap();
    assert_eq!(node.propose(vec![9]).unwrap(), 9);
}

#[test]
fn capacity_preflight_is_nonmutating_and_compaction_reclaims_log_budget() {
    let data = tempfile::tempdir().unwrap();
    let (_, mut node) = leader(data.path());
    for _ in 0..15 {
        node.propose(vec![3; 1024 * 1024]).unwrap();
    }
    node.committed().unwrap();
    assert!(matches!(
        node.propose(vec![4; 1024 * 1024]),
        Err(RaftStorageError::Capacity)
    ));
    let before = bytes(data.path());
    assert!(matches!(
        node.compact(15, vec![0; 16 * 1024 * 1024]),
        Err(RaftStorageError::Capacity)
    ));
    assert_eq!(bytes(data.path()), before);
    assert_eq!(node.snapshot_index(), 0);
    assert!(!node.is_closed());
    node.compact(15, b"exact application state".to_vec())
        .unwrap();
    assert!(bytes(data.path()).len() < 512);
    assert_eq!(node.propose(vec![4; 1024 * 1024]).unwrap(), 16);
}

#[test]
fn interrupted_transfer_restarts_without_acknowledging_unpersisted_state() {
    let data = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut node = open(data.path(), &clock, vec!["p".into()]);
    let state = vec![9; 2 * 64 * 1024 + 17];
    let packets = chunks(10, 2, 3, &state);
    node.step("p".into(), packets[0].clone()).unwrap();
    assert_eq!(node.current_term(), 3);
    assert_eq!(node.snapshot_index(), 0);
    assert!(matches!(
        node.outbox().unwrap().as_slice(),
        [(
            _,
            RaftRpc::InstallSnapshotResp {
                installed: false,
                next_offset: 65536,
                ..
            }
        )]
    ));
    drop(node);
    let mut node = open(data.path(), &clock, vec!["p".into()]);
    node.step("p".into(), packets[1].clone()).unwrap();
    assert!(matches!(
        node.outbox().unwrap().as_slice(),
        [(
            _,
            RaftRpc::InstallSnapshotResp {
                installed: false,
                next_offset: 0,
                ..
            }
        )]
    ));
    for packet in packets {
        node.step("p".into(), packet).unwrap();
    }
    assert_eq!(node.pending_snapshot().unwrap().unwrap().data, state);
    let published = bytes(data.path());
    drop(node);
    let node = open(data.path(), &clock, vec!["p".into()]);
    assert_eq!(node.commit_index(), 10);
    assert_eq!(node.pending_snapshot().unwrap().unwrap().data, state);
    assert_eq!(bytes(data.path()), published);
}

#[test]
fn checkpoint_failure_blocks_local_and_received_snapshot_success() {
    for incoming in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new());
        let mut node = open(
            data.path(),
            &clock,
            if incoming { vec!["p".into()] } else { vec![] },
        );
        if !incoming {
            clock.advance(1400);
            node.tick().unwrap();
            node.propose(b"acknowledged".to_vec()).unwrap();
            node.committed().unwrap();
        }
        let before = bytes(data.path());
        std::fs::write(data.path().join("raft.next"), b"blocked publication").unwrap();
        let result = if incoming {
            node.step("p".into(), chunks(10, 1, 1, b"state").remove(0))
        } else {
            node.compact(1, b"state".to_vec())
        };
        assert!(matches!(result, Err(RaftStorageError::Storage(_))));
        assert!(node.is_closed());
        assert!(node.outbox().is_err());
        assert!(node.committed().is_err());
        assert!(node.pending_snapshot().is_err());
        assert_eq!(node.snapshot_index(), 0);
        assert_eq!(bytes(data.path()), before);
        drop(node);
        let mut node = open(
            data.path(),
            &clock,
            if incoming { vec!["p".into()] } else { vec![] },
        );
        assert_eq!(node.snapshot_index(), 0);
        assert!(node.pending_snapshot().unwrap().is_none());
        assert_eq!(node.committed().unwrap().len(), usize::from(!incoming));
    }
}

#[test]
fn installation_keeps_only_a_term_matching_suffix_and_applies_in_order() {
    for matching in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new());
        let mut node = open(data.path(), &clock, vec!["p".into()]);
        let log = (1..=4)
            .map(|index| LogEntry {
                index,
                term: 1,
                cmd: vec![index as u8],
            })
            .collect();
        node.step("p".into(), append(1, log, 1)).unwrap();
        node.committed().unwrap();
        node.outbox().unwrap();
        let snapshot_term = if matching { 1 } else { 2 };
        node.step("p".into(), chunks(3, snapshot_term, 2, b"three").remove(0))
            .unwrap();
        assert_eq!(node.log().len(), usize::from(matching));
        drop(node);
        let mut node = open(data.path(), &clock, vec!["p".into()]);
        let tail = if matching {
            vec![LogEntry {
                index: 4,
                term: 1,
                cmd: vec![4],
            }]
        } else {
            vec![LogEntry {
                index: 4,
                term: 2,
                cmd: vec![44],
            }]
        };
        node.step(
            "p".into(),
            RaftRpc::AppendEntries {
                term: 2,
                leader: "p".into(),
                prev_log_index: 3,
                prev_log_term: snapshot_term,
                entries: tail,
                leader_commit: 4,
            },
        )
        .unwrap();
        assert!(node.committed().unwrap().is_empty());
        node.snapshot_applied(3, snapshot_term).unwrap();
        let applied = node.committed().unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].index, 4);
        assert_eq!(applied[0].cmd, if matching { vec![4] } else { vec![44] });
    }
}

#[test]
fn partial_journal_tail_recovers_relative_to_snapshot_base() {
    let source = tempfile::tempdir().unwrap();
    let (_, mut node) = leader(source.path());
    node.propose(vec![1]).unwrap();
    node.committed().unwrap();
    node.compact(1, b"one".to_vec()).unwrap();
    node.propose(vec![2]).unwrap();
    let prefix = bytes(source.path());
    node.propose(vec![3]).unwrap();
    let complete = bytes(source.path());
    assert!(complete.starts_with(&prefix));
    for cut in 0..complete.len() - prefix.len() {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(
            data.path().join("raft.state"),
            &complete[..prefix.len() + cut],
        )
        .unwrap();
        std::fs::write(data.path().join("raft.next"), b"abandoned").unwrap();
        let mut recovered = open(data.path(), &Arc::new(ManualClock::new()), vec![]);
        assert_eq!(bytes(data.path()), prefix);
        assert_eq!(recovered.commit_index(), 2);
        assert!(recovered.committed().unwrap().is_empty());
        recovered.snapshot_applied(1, 1).unwrap();
        let entries = recovered.committed().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].index, 2);
        assert_eq!(entries[0].cmd, [2]);
    }
}

fn checkpoint(value: &impl serde::Serialize) -> Vec<u8> {
    let body = bincode::serde::encode_to_vec(value, bincode::config::standard()).unwrap();
    let mut bytes = b"PRAFT03\0".to_vec();
    let length = (body.len() as u64).to_le_bytes();
    let mut hash = Sha256::new();
    hash.update(Sha256::digest(&bytes));
    hash.update(length);
    let header = hash.finalize();
    let mut hash = Sha256::new();
    hash.update(header);
    hash.update(&body);
    bytes.extend_from_slice(&length);
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&hash.finalize());
    bytes.extend_from_slice(&body);
    bytes
}

#[test]
fn checksummed_invalid_compacted_metadata_never_triggers_recovery_writes() {
    let snapshot = RaftSnapshot {
        index: 7,
        term: 2,
        data: b"seven".to_vec(),
    };
    for case in 0..9 {
        let mut snapshot = snapshot.clone();
        let mut commit = 7u64;
        let mut log = vec![];
        match case {
            0 => snapshot.index = 0,
            1 => snapshot.term = 0,
            2 => snapshot.term = 4,
            3 => commit = 6,
            4 => commit = 8,
            5 => log.push(LogEntry {
                index: 7,
                term: 3,
                cmd: vec![],
            }),
            6 => log.push(LogEntry {
                index: 8,
                term: 1,
                cmd: vec![],
            }),
            7 => {
                snapshot.index = u64::MAX;
                commit = u64::MAX;
                log.push(LogEntry {
                    index: 0,
                    term: 3,
                    cmd: vec![],
                });
            }
            _ => {}
        }
        let version = if case == 8 { 1u8 } else { 3u8 };
        let bytes = checkpoint(&(
            version,
            "snapshots",
            "n",
            Vec::<String>::new(),
            3u64,
            None::<String>,
            log,
            commit,
            snapshot,
        ));
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("raft.state"), &bytes).unwrap();
        std::fs::write(data.path().join("raft.next"), b"preserve").unwrap();
        assert!(
            DurableRaft::open(
                data.path(),
                "snapshots",
                "n",
                vec![],
                Arc::new(ManualClock::new())
            )
            .is_err(),
            "case {case}"
        );
        assert_eq!(self::bytes(data.path()), bytes);
        assert_eq!(
            std::fs::read(data.path().join("raft.next")).unwrap(),
            b"preserve"
        );
    }
}

#[test]
fn snapshot_term_cannot_precede_an_existing_committed_term() {
    let data = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut node = open(data.path(), &clock, vec!["p".into()]);
    node.step(
        "p".into(),
        append(
            3,
            vec![LogEntry {
                index: 1,
                term: 3,
                cmd: vec![1],
            }],
            1,
        ),
    )
    .unwrap();
    node.committed().unwrap();
    node.outbox().unwrap();
    assert!(matches!(
        node.step("p".into(), chunks(7, 2, 4, b"impossible history").remove(0)),
        Err(RaftStorageError::Snapshot(SnapshotError::Invalid))
    ));
    assert_eq!(node.current_term(), 4);
    assert_eq!(node.commit_index(), 1);
    assert_eq!(node.snapshot_index(), 0);
    assert!(node.pending_snapshot().unwrap().is_none());
    assert!(matches!(
        node.outbox().unwrap().as_slice(),
        [(
            _,
            RaftRpc::InstallSnapshotResp {
                installed: false,
                ..
            }
        )]
    ));
    drop(node);
    let mut recovered = open(data.path(), &clock, vec!["p".into()]);
    assert_eq!(recovered.current_term(), 4);
    assert_eq!(recovered.committed().unwrap()[0].cmd, [1]);
}

#[cfg(all(
    target_os = "linux",
    target_env = "gnu",
    not(target_feature = "crt-static")
))]
#[test]
fn actual_checkpoint_file_and_directory_sync_failures_fence_all_success_paths() {
    const CHILD: &str = "PARROT_SNAPSHOT_FAULT_CHILD";
    if let Some(path) = std::env::var_os(CHILD) {
        let path = std::path::PathBuf::from(path);
        let incoming = std::env::var("PARROT_SNAPSHOT_INCOMING").unwrap() == "yes";
        let directory_sync = std::env::var("PARROT_SNAPSHOT_FAIL_AT").unwrap() == "directory";
        let clock = Arc::new(ManualClock::new());
        let peers = if incoming { vec!["p".into()] } else { vec![] };
        let mut node = open(&path, &clock, peers.clone());
        if !incoming {
            clock.advance(1400);
            node.tick().unwrap();
            node.propose(b"old".to_vec()).unwrap();
            node.committed().unwrap();
        }
        let before = bytes(&path);
        let result = if incoming {
            node.step("p".into(), chunks(7, 2, 3, b"checkpoint").remove(0))
        } else {
            node.compact(1, b"checkpoint".to_vec())
        };
        assert!(
            matches!(result, Err(RaftStorageError::Storage(_))),
            "actual fsync failure must prevent a successful checkpoint: {result:?}"
        );
        assert!(node.is_closed());
        assert!(node.outbox().is_err());
        assert!(node.committed().is_err());
        assert!(node.pending_snapshot().is_err());
        assert_eq!(node.snapshot_index(), 0);
        if !directory_sync {
            assert_eq!(bytes(&path), before);
        }
        drop(node);
        let mut node = open(&path, &clock, peers);
        if directory_sync {
            let index = if incoming { 7 } else { 1 };
            let term = if incoming { 2 } else { 1 };
            assert_eq!(node.snapshot_index(), index);
            assert_eq!(
                node.pending_snapshot().unwrap().unwrap().data,
                b"checkpoint"
            );
            assert!(node.committed().unwrap().is_empty());
            node.snapshot_applied(index, term).unwrap();
        } else {
            assert_eq!(node.snapshot_index(), 0);
            assert_eq!(node.committed().unwrap().len(), usize::from(!incoming));
        }
        return;
    }
    let tools = tempfile::tempdir().unwrap();
    let library = tools.path().join("snapshot-fault.so");
    let compiled = std::process::Command::new("cc")
        .args([
            "-shared",
            "-fPIC",
            "-O2",
            "-D_GNU_SOURCE",
            "-Wall",
            "-Wextra",
            "-Werror",
        ])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/raft-snapshot-fault.c"
        ))
        .args(["-ldl", "-o"])
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for mode in ["file", "directory"] {
        for incoming in ["no", "yes"] {
            let data = tempfile::tempdir().unwrap();
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "actual_checkpoint_file_and_directory_sync_failures_fence_all_success_paths",
                    "--nocapture",
                ])
                .env(CHILD, data.path())
                .env("PARROT_SNAPSHOT_FAIL_AT", mode)
                .env("PARROT_SNAPSHOT_INCOMING", incoming)
                .env("LD_PRELOAD", &library)
                .output()
                .unwrap();
            assert_eq!(
                child.status.code(),
                Some(0),
                "sync={mode} incoming={incoming}:\n{}\n{}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
        }
    }
}
