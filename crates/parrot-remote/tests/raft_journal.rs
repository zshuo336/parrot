use parrot_remote::{
    raft::{DurableRaft, LogEntry, ManualClock, RaftStorageError, Role},
    roles::directory::{DirCmd, DurableDirectory},
};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};

fn open(path: &Path, clock: &Arc<ManualClock>) -> DurableRaft {
    DurableRaft::open(path, "journal", "n", vec![], clock.clone()).unwrap()
}
fn leader(path: &Path) -> (Arc<ManualClock>, DurableRaft) {
    let clock = Arc::new(ManualClock::new());
    let mut node = open(path, &clock);
    clock.advance(1400);
    node.tick().unwrap();
    assert_eq!(node.role(), Role::Leader);
    (clock, node)
}
fn stored(path: &Path) -> Vec<u8> {
    std::fs::read(path.join("raft.state")).unwrap()
}
fn install(path: &Path, bytes: &[u8]) {
    std::fs::write(path.join("raft.state"), bytes).unwrap();
}
fn last_hash(bytes: &[u8]) -> [u8; 32] {
    assert_eq!(&bytes[..8], b"PRAFT02\0");
    let mut position = 8;
    let mut hash = [0; 32];
    while position < bytes.len() {
        let size = u64::from_le_bytes(bytes[position..position + 8].try_into().unwrap()) as usize;
        hash.copy_from_slice(&bytes[position + 40..position + 72]);
        position += 72 + size;
    }
    assert_eq!(position, bytes.len());
    hash
}
fn append_frame(bytes: &mut Vec<u8>, value: &impl serde::Serialize) {
    let body = bincode::serde::encode_to_vec(value, bincode::config::standard()).unwrap();
    let length = (body.len() as u64).to_le_bytes();
    let mut header = Sha256::new();
    header.update(last_hash(bytes));
    header.update(length);
    let header = header.finalize();
    let mut digest = Sha256::new();
    digest.update(header);
    digest.update(&body);
    bytes.extend_from_slice(&length);
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&digest.finalize());
    bytes.extend_from_slice(&body);
}

#[test]
fn legacy_upgrade_keeps_evidence_and_requires_atomic_publication() {
    let data = tempfile::tempdir().unwrap();
    let history = vec![LogEntry {
        term: 1,
        index: 1,
        cmd: b"old".to_vec(),
    }];
    let body = bincode::serde::encode_to_vec(
        (
            1u8,
            "journal",
            "n",
            Vec::<String>::new(),
            1u64,
            Some("n"),
            &history,
            1u64,
        ),
        bincode::config::standard(),
    )
    .unwrap();
    let mut legacy = b"PRAFT01\0".to_vec();
    legacy.extend_from_slice(&Sha256::digest(&body));
    legacy.extend_from_slice(&body);
    install(data.path(), &legacy);
    let clock = Arc::new(ManualClock::new());
    let mut node = open(data.path(), &clock);
    assert_eq!(node.committed().unwrap(), history);
    assert_eq!(
        stored(data.path()),
        legacy,
        "opening is not an implicit upgrade"
    );
    std::fs::write(data.path().join("raft.next"), b"blocked checkpoint").unwrap();
    clock.advance(1400);
    assert!(node.tick().is_err());
    assert!(node.is_closed());
    assert_eq!(stored(data.path()), legacy);
    assert!(matches!(node.outbox(), Err(RaftStorageError::Closed)));
    drop(node);
    let mut node = open(data.path(), &clock);
    assert!(!data.path().join("raft.next").exists());
    assert_eq!(stored(data.path()), legacy);
    clock.advance(1400);
    node.tick().unwrap();
    assert_eq!(node.role(), Role::Leader);
    assert_eq!(node.propose(b"new".to_vec()).unwrap(), 2);
    assert!(stored(data.path()).starts_with(b"PRAFT02\0"));
    drop(node);
    let mut recovered = open(data.path(), &clock);
    let replay = recovered.committed().unwrap();
    assert_eq!(replay.len(), 2);
    assert_eq!(replay[0], history[0]);
    assert_eq!(replay[1].cmd, b"new");
    assert_eq!(recovered.current_term(), 2);
}

#[test]
fn every_incomplete_append_recovers_the_acknowledged_prefix_before_reuse() {
    let source = tempfile::tempdir().unwrap();
    let (_, mut node) = leader(source.path());
    node.propose(b"acknowledged".to_vec()).unwrap();
    let prefix = stored(source.path());
    node.propose(b"later".to_vec()).unwrap();
    let complete = stored(source.path());
    assert!(complete.starts_with(&prefix));
    drop(node);
    let record = &complete[prefix.len()..];
    assert!(record.len() > 72);
    for cut in 0..record.len() {
        let data = tempfile::tempdir().unwrap();
        let mut partial = prefix.clone();
        partial.extend_from_slice(&record[..cut]);
        install(data.path(), &partial);
        std::fs::write(data.path().join("raft.next"), b"abandoned").unwrap();
        let clock = Arc::new(ManualClock::new());
        let mut recovered = open(data.path(), &clock);
        assert_eq!(recovered.log().len(), 1, "cut {cut}");
        assert_eq!(recovered.commit_index(), 1, "cut {cut}");
        assert_eq!(stored(data.path()), prefix, "cut {cut}");
        assert!(!data.path().join("raft.next").exists());
        assert_eq!(recovered.committed().unwrap()[0].cmd, b"acknowledged");
        clock.advance(1400);
        recovered.tick().unwrap();
        assert_eq!(recovered.propose(b"replacement".to_vec()).unwrap(), 2);
        drop(recovered);
        let mut recovered = open(data.path(), &clock);
        let replay = recovered.committed().unwrap();
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].cmd, b"acknowledged");
        assert_eq!(replay[1].cmd, b"replacement");
    }
    let data = tempfile::tempdir().unwrap();
    install(data.path(), &complete);
    let mut recovered = open(data.path(), &Arc::new(ManualClock::new()));
    assert_eq!(recovered.committed().unwrap().len(), 2);
    assert_eq!(stored(data.path()), complete);
}

#[test]
fn complete_record_corruption_and_reordering_never_discard_evidence() {
    let data = tempfile::tempdir().unwrap();
    let (_, mut node) = leader(data.path());
    node.propose(b"first".to_vec()).unwrap();
    let prefix = stored(data.path());
    node.propose(b"second".to_vec()).unwrap();
    let complete = stored(data.path());
    drop(node);
    let start = prefix.len();
    let mut corruptions = Vec::new();
    for offset in [0, 8, 40, 72, complete.len() - start - 1] {
        let mut bytes = complete.clone();
        bytes[start + offset] ^= 1;
        corruptions.push(bytes);
    }
    let mut duplicate = complete.clone();
    duplicate.extend_from_slice(&complete[start..]);
    corruptions.push(duplicate);
    let mut length = complete.clone();
    length[start..start + 8].copy_from_slice(&1_000_000u64.to_le_bytes());
    corruptions.push(length);
    for bytes in corruptions {
        install(data.path(), &bytes);
        std::fs::write(data.path().join("raft.next"), b"preserve").unwrap();
        assert!(DurableRaft::open(
            data.path(),
            "journal",
            "n",
            vec![],
            Arc::new(ManualClock::new())
        )
        .is_err());
        assert_eq!(stored(data.path()), bytes);
        assert_eq!(
            std::fs::read(data.path().join("raft.next")).unwrap(),
            b"preserve"
        );
    }
}

#[test]
fn valid_checksums_cannot_publish_invalid_state_transitions() {
    let source = tempfile::tempdir().unwrap();
    let (_, mut node) = leader(source.path());
    node.propose(b"committed".to_vec()).unwrap();
    let prefix = stored(source.path());
    drop(node);
    let bad_entry = |index, term| {
        vec![LogEntry {
            index,
            term,
            cmd: vec![1],
        }]
    };
    for delta in [
        (0, Some("n".to_owned()), 1, vec![], 1),
        (1, None, 1, vec![], 1),
        (1, Some("outsider".to_owned()), 1, vec![], 1),
        (1, Some("n".to_owned()), 1, vec![], 0),
        (1, Some("n".to_owned()), 1, vec![], 2),
        (2, Some("n".to_owned()), 0, bad_entry(1, 2), 1),
        (1, Some("n".to_owned()), 1, bad_entry(0, 1), 1),
        (1, Some("n".to_owned()), 1, bad_entry(2, 0), 1),
        (1, Some("n".to_owned()), 1, bad_entry(2, 2), 1),
    ] {
        // Explicit integer widths are the stored schema, independent of Rust's
        // inference for unsuffixed literals in the malformed examples above.
        let delta = (
            delta.0 as u64,
            delta.1,
            delta.2 as u64,
            delta.3,
            delta.4 as u64,
        );
        let data = tempfile::tempdir().unwrap();
        let mut bytes = prefix.clone();
        append_frame(&mut bytes, &delta);
        bytes.push(0); // A recoverable tail must not hide the invalid full frame.
        install(data.path(), &bytes);
        std::fs::write(data.path().join("raft.next"), b"preserve").unwrap();
        assert!(DurableRaft::open(
            data.path(),
            "journal",
            "n",
            vec![],
            Arc::new(ManualClock::new())
        )
        .is_err());
        assert_eq!(stored(data.path()), bytes);
        assert_eq!(
            std::fs::read(data.path().join("raft.next")).unwrap(),
            b"preserve"
        );
    }
}

#[test]
fn application_preflight_precedes_journal_tail_and_temporary_cleanup() {
    let data = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut directory =
        DurableDirectory::open(data.path(), "application", "n", vec![], clock.clone()).unwrap();
    clock.advance(1400);
    directory.tick().unwrap();
    directory
        .propose(&DirCmd::Upsert {
            node: "one".into(),
            endpoints: vec!["tcp://127.0.0.1:7000".into()],
        })
        .unwrap();
    drop(directory);
    let mut bytes = stored(data.path());
    append_frame(
        &mut bytes,
        &(
            1u64,
            Some("n"),
            1u64,
            vec![LogEntry {
                index: 2,
                term: 1,
                cmd: vec![0xff],
            }],
            1u64,
        ),
    );
    bytes.extend_from_slice(b"torn");
    install(data.path(), &bytes);
    std::fs::write(data.path().join("raft.next"), b"preserve").unwrap();
    assert!(DurableDirectory::open(data.path(), "application", "n", vec![], clock).is_err());
    assert_eq!(stored(data.path()), bytes);
    assert_eq!(
        std::fs::read(data.path().join("raft.next")).unwrap(),
        b"preserve"
    );
}

#[test]
fn checkpoint_failure_fences_owner_and_replays_only_published_history() {
    let data = tempfile::tempdir().unwrap();
    let (clock, mut node) = leader(data.path());
    std::fs::write(data.path().join("raft.next"), b"block replacement").unwrap();
    let mut acknowledged = 0;
    let mut last = stored(data.path());
    let mut failed = false;
    for value in 0..2048u64 {
        match node.propose(value.to_le_bytes().to_vec()) {
            Ok(index) => {
                acknowledged = index;
                node.committed().unwrap();
                last = stored(data.path());
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed, "bounded journal must eventually checkpoint");
    assert!(
        acknowledged > 1,
        "ordinary appends must not rewrite a checkpoint"
    );
    assert!(node.is_closed());
    assert_eq!(node.commit_index(), acknowledged);
    assert_eq!(node.log().len() as u64, acknowledged);
    assert!(node.outbox().is_err());
    assert!(node.committed().is_err());
    assert_eq!(stored(data.path()), last);
    drop(node);
    let mut recovered = open(data.path(), &clock);
    let replay = recovered.committed().unwrap();
    assert_eq!(replay.len() as u64, acknowledged);
    for (value, entry) in replay.iter().enumerate() {
        assert_eq!(entry.cmd, (value as u64).to_le_bytes());
    }
    assert!(!data.path().join("raft.next").exists());
    clock.advance(1400);
    recovered.tick().unwrap();
    assert_eq!(
        recovered.propose(b"after recovery".to_vec()).unwrap(),
        acknowledged + 1
    );
}

#[test]
fn repeated_checkpoints_preserve_all_committed_entries() {
    let data = tempfile::tempdir().unwrap();
    let (clock, mut node) = leader(data.path());
    let mut previous = stored(data.path());
    let mut checkpoints = 0;
    for value in 0..1800u64 {
        node.propose(value.to_le_bytes().to_vec()).unwrap();
        node.committed().unwrap();
        let current = stored(data.path());
        if !current.starts_with(&previous) {
            checkpoints += 1;
        }
        previous = current;
    }
    assert!(checkpoints >= 2);
    drop(node);
    let mut recovered = open(data.path(), &clock);
    let replay = recovered.committed().unwrap();
    assert_eq!(replay.len(), 1800);
    for (value, entry) in replay.iter().enumerate() {
        assert_eq!(entry.index, value as u64 + 1);
        assert_eq!(entry.cmd, (value as u64).to_le_bytes());
    }
}

#[test]
fn oversized_journal_is_rejected_before_any_recovery_cleanup() {
    let data = tempfile::tempdir().unwrap();
    let (_, node) = leader(data.path());
    drop(node);
    let prefix = stored(data.path());
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(data.path().join("raft.state"))
        .unwrap();
    file.set_len(40 * 1024 * 1024).unwrap();
    drop(file);
    std::fs::write(data.path().join("raft.next"), b"preserve").unwrap();
    assert!(DurableRaft::open(
        data.path(),
        "journal",
        "n",
        vec![],
        Arc::new(ManualClock::new())
    )
    .is_err());
    let bytes = stored(data.path());
    assert_eq!(bytes.len(), 40 * 1024 * 1024);
    assert!(bytes.starts_with(&prefix));
    assert_eq!(
        std::fs::read(data.path().join("raft.next")).unwrap(),
        b"preserve"
    );
}

#[cfg(unix)]
#[test]
fn append_rejects_a_replaced_symlink_without_writing_its_target() {
    let data = tempfile::tempdir().unwrap();
    let (_, mut node) = leader(data.path());
    node.propose(b"published".to_vec()).unwrap();
    let original = stored(data.path());
    let target = data.path().join("protected");
    std::fs::write(&target, b"protected").unwrap();
    std::fs::rename(data.path().join("raft.state"), data.path().join("backup")).unwrap();
    std::os::unix::fs::symlink(&target, data.path().join("raft.state")).unwrap();
    assert!(node.propose(b"must not escape".to_vec()).is_err());
    assert!(node.is_closed());
    assert_eq!(node.commit_index(), 1);
    assert_eq!(node.log().len(), 1);
    assert_eq!(std::fs::read(target).unwrap(), b"protected");
    assert_eq!(std::fs::read(data.path().join("backup")).unwrap(), original);
}

#[cfg(all(
    target_os = "linux",
    target_env = "gnu",
    not(target_feature = "crt-static")
))]
#[test]
fn append_sync_failure_is_fenced_and_full_uncertain_record_can_recover() {
    const CHILD: &str = "PARROT_RAFT_SYNC_CHILD";
    if let Some(path) = std::env::var_os(CHILD) {
        let data = std::path::PathBuf::from(path);
        let (clock, mut node) = leader(&data);
        assert!(
            matches!(
                node.propose(b"uncertain".to_vec()),
                Err(RaftStorageError::Storage(_))
            ),
            "the actual append fsync error must prevent success"
        );
        assert!(node.is_closed());
        assert_eq!(node.current_term(), 1);
        assert_eq!(node.commit_index(), 0);
        assert!(node.log().is_empty());
        assert!(node.committed().is_err());
        assert!(node.outbox().is_err());
        drop(node);
        let mut recovered = open(&data, &clock);
        assert_eq!(recovered.commit_index(), 1);
        let replay = recovered.committed().unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].cmd, b"uncertain");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let library = data.path().join("fail-sync.so");
    let compiler = std::process::Command::new("cc")
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
            "/tests/fixtures/raft-journal-fault.c"
        ))
        .args(["-ldl", "-o"])
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        compiler.status.success(),
        "{}",
        String::from_utf8_lossy(&compiler.stderr)
    );
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "append_sync_failure_is_fenced_and_full_uncertain_record_can_recover",
            "--nocapture",
        ])
        .env(CHILD, data.path())
        .env("LD_PRELOAD", library)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "child {:?}\n{}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}

#[test]
fn declared_entry_limits_reject_snapshots_and_deltas_before_cleanup() {
    let source = tempfile::tempdir().unwrap();
    let (_, node) = leader(source.path());
    drop(node);
    let prefix = stored(source.path());
    for legacy in [true, false] {
        let entries: Vec<_> = (1..=65_537)
            .map(|index| LogEntry {
                index,
                term: 1,
                cmd: vec![],
            })
            .collect();
        let mut bytes = if legacy {
            let body = bincode::serde::encode_to_vec(
                (
                    1u8,
                    "journal",
                    "n",
                    Vec::<String>::new(),
                    1u64,
                    Some("n"),
                    entries,
                    0u64,
                ),
                bincode::config::standard(),
            )
            .unwrap();
            let mut bytes = b"PRAFT01\0".to_vec();
            bytes.extend_from_slice(&Sha256::digest(&body));
            bytes.extend_from_slice(&body);
            bytes
        } else {
            let mut bytes = prefix.clone();
            append_frame(&mut bytes, &(1u64, Some("n"), 0u64, entries, 0u64));
            bytes
        };
        if !legacy {
            bytes.push(0);
        }
        let data = tempfile::tempdir().unwrap();
        install(data.path(), &bytes);
        std::fs::write(data.path().join("raft.next"), b"preserve").unwrap();
        let error = DurableRaft::open(
            data.path(),
            "journal",
            "n",
            vec![],
            Arc::new(ManualClock::new()),
        )
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains("persistent log entry capacity exceeded"),
            "{error}"
        );
        assert_eq!(stored(data.path()), bytes);
        assert_eq!(
            std::fs::read(data.path().join("raft.next")).unwrap(),
            b"preserve"
        );
    }
}
