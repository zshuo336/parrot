use super::*;

#[tokio::test]
async fn handoff_preserves_outstanding_sender_records_until_they_are_drained() {
    let from = tempfile::tempdir().unwrap();
    let to = tempfile::tempdir().unwrap();
    let cfg = config(from.path());
    let target = handoff::prepare(&config(to.path()), "node").unwrap();
    let ticket = sender::Ticket {
        source: "node".into(),
        incarnation: [1; 16],
        stream: [2; 16],
        receiver: [3; 16],
        epoch: None,
        ticket: [4; 16],
        actor: true,
        destination: "peer".into(),
        path: "parrot://peer/actor".into(),
    };
    let owner = ReceiverOwner::open(&cfg, "node").unwrap();
    let journal = sender::Journal::open(&owner, true, 1).unwrap().unwrap();
    journal.record(ticket.clone(), false).await.unwrap();
    drop((journal, owner));
    assert!(handoff::seal(&cfg, "node", &target)
        .unwrap_err()
        .contains("outstanding sender tickets"));
    let owner = ReceiverOwner::open(&cfg, "node").unwrap();
    let journal = sender::Journal::open(&owner, true, 1).unwrap().unwrap();
    assert_eq!(journal.pending(), [ticket.clone()]);
    journal.record(ticket, true).await.unwrap();
    drop((journal, owner));
    handoff::seal(&cfg, "node", &target).unwrap();
}

#[test]
fn receiver_handoff_is_exclusive_retryable_and_preserves_all_retired_epochs() {
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let [a, b, c] = dirs.each_ref().map(|d| config(d.path()));
    let node = "receiver";
    let first = ReceiverOwner::open(&a, node).unwrap();
    let first_id = first.receiver();
    let destination = handoff::prepare(&b, node).unwrap();
    let other = handoff::prepare(&c, node).unwrap();
    assert_eq!(handoff::prepare(&b, node).unwrap(), destination);
    assert!(ReceiverOwner::open(&b, node).is_err());
    assert!(handoff::seal(&a, node, &destination).is_err());
    drop(first);
    let seal = handoff::seal(&a, node, &destination).unwrap();
    assert!(ReceiverOwner::open(&a, node).is_err());
    assert_eq!(seal, handoff::seal(&a, node, &destination).unwrap());
    assert!(handoff::seal(&a, node, &other).is_err());
    let digest = Sha256::digest(&seal).into();
    assert!(handoff::activate(&c, node, &seal, digest).is_err());
    assert!(handoff::activate(&b, node, &seal, [0; 32]).is_err());
    handoff::activate(&b, node, &seal, digest).unwrap();
    handoff::activate(&b, node, &seal, digest).unwrap();
    let second = ReceiverOwner::open(&b, node).unwrap();
    let second_id = second.receiver();
    assert_eq!(second.epoch(), 2);
    assert!(second.proves_retired(first_id, 1));
    assert!(!second.proves_retired(second_id, 2));
    drop(second);
    handoff::activate(&b, node, &seal, digest).unwrap();
    let next = handoff::seal(&b, node, &other).unwrap();
    assert!(handoff::activate(&b, node, &seal, digest).is_err());
    handoff::activate(&c, node, &next, Sha256::digest(&next).into()).unwrap();
    let third = ReceiverOwner::open(&c, node).unwrap();
    assert_eq!(third.epoch(), 3);
    assert!(third.proves_retired(first_id, 1));
    assert!(third.proves_retired(second_id, 2));
}

fn config(directory: &Path) -> OrderedReceiverState {
    OrderedReceiverState {
        directory: directory.into(),
        realm: "receiver-contract".into(),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn live_registration_pins_the_source_directory_and_a_single_destination() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("source");
    let moved = root.path().join("moved");
    std::fs::create_dir(&dir).unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let invite = handoff::prepare(&config(first.path()), "node").unwrap();
    let other = handoff::prepare(&config(second.path()), "node").unwrap();
    let owner = ReceiverOwner::open(&config(&dir), "node").unwrap();
    std::fs::rename(&dir, &moved).unwrap();
    std::fs::create_dir(&dir).unwrap();
    let foreign = ReceiverOwner::open(&config(&dir), "node").unwrap();
    assert!(owner
        .reserve_standby(&invite)
        .unwrap_err()
        .contains("replaced"));
    drop(foreign);
    std::fs::rename(&dir, root.path().join("foreign")).unwrap();
    std::fs::rename(moved, &dir).unwrap();
    let proof = owner.reserve_standby(&invite).unwrap();
    assert_eq!(owner.reserve_standby(&invite).unwrap(), proof);
    assert!(owner.reserve_standby(&other).is_err());
    drop(owner);
    assert!(ReceiverOwner::open(&config(&dir), "node")
        .err()
        .unwrap()
        .contains("sealed"));
    assert!(handoff::seal(&config(&dir), "node", &other).is_err());
    assert_eq!(
        handoff::seal(&config(&dir), "node", &invite).unwrap(),
        proof
    );
}

#[test]
fn epochs_serialize_owners_and_only_attest_previous_owners_of_the_same_domain() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = config(directory.path());
    let first = ReceiverOwner::open(&cfg, "node").unwrap();
    let old = first.receiver();
    assert!(ReceiverOwner::open(&cfg, "node").is_err());
    assert!(!first.proves_retired(old, 1));
    drop(first);
    let second = ReceiverOwner::open(&cfg, "node").unwrap();
    assert!(second.proves_retired(old, 1));
    assert!(!second.proves_retired(second.receiver(), 2));
    assert!(!second.proves_retired(old, u64::MAX));
    let unrelated = tempfile::tempdir().unwrap();
    let other = ReceiverOwner::open(&config(unrelated.path()), "node").unwrap();
    assert!(!other.proves_retired(old, 1));
}

#[test]
fn copied_state_or_replaced_lock_inode_cannot_certify_an_active_original_owner() {
    let directory = tempfile::tempdir().unwrap();
    let owner = ReceiverOwner::open(&config(directory.path()), "node").unwrap();
    let copy = tempfile::tempdir().unwrap();
    std::fs::copy(
        directory.path().join("ordered.state"),
        copy.path().join("ordered.state"),
    )
    .unwrap();
    let bytes = std::fs::read(copy.path().join("ordered.state")).unwrap();
    assert!(ReceiverOwner::open(&config(copy.path()), "node").is_err());
    assert_eq!(
        std::fs::read(copy.path().join("ordered.state")).unwrap(),
        bytes
    );
    std::fs::rename(
        directory.path().join(".parrot-ordered.lock"),
        directory.path().join("saved.lock"),
    )
    .unwrap();
    assert!(ReceiverOwner::open(&config(directory.path()), "node").is_err());
    assert!(owner.valid_process());
}

#[test]
fn corrupt_or_foreign_bindings_fail_before_mutating_state_or_abandoned_temporary() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = config(directory.path());
    drop(ReceiverOwner::open(&cfg, "node").unwrap());
    let path = directory.path().join("ordered.state");
    let bytes = std::fs::read(&path).unwrap();
    let temporary = directory.path().join("ordered.next");
    std::fs::write(&temporary, b"unfinished").unwrap();
    let mut foreign = cfg.clone();
    foreign.realm = "other-realm".into();
    assert!(ReceiverOwner::open(&foreign, "node").is_err());
    assert!(ReceiverOwner::open(&cfg, "other-node").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read(&temporary).unwrap(), b"unfinished");
    for invalid in [
        vec![],
        bytes[..39].to_vec(),
        {
            let mut corrupt = bytes.clone();
            corrupt[40] ^= 1;
            corrupt
        },
        vec![0; LIMIT + 41],
    ] {
        std::fs::write(&path, &invalid).unwrap();
        assert!(ReceiverOwner::open(&cfg, "node").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), invalid);
        assert_eq!(std::fs::read(&temporary).unwrap(), b"unfinished");
    }
    std::fs::write(&path, &bytes).unwrap();
    let second = ReceiverOwner::open(&cfg, "node").unwrap();
    assert_eq!(second.epoch(), 2);
    assert!(!temporary.exists());
}

#[test]
fn exhausted_epoch_does_not_wrap_or_discard_a_temporary() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = config(directory.path());
    drop(ReceiverOwner::open(&cfg, "node").unwrap());
    let path = directory.path().join("ordered.state");
    let original = std::fs::read(&path).unwrap();
    let mut saved: Saved = serde_json::from_slice(&original[40..]).unwrap();
    saved.epoch = u64::MAX;
    let body = serde_json::to_vec(&saved).unwrap();
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&Sha256::digest(&body));
    bytes.extend_from_slice(&body);
    std::fs::write(&path, &bytes).unwrap();
    std::fs::write(directory.path().join("ordered.next"), b"evidence").unwrap();
    assert!(ReceiverOwner::open(&cfg, "node")
        .err()
        .unwrap()
        .contains("epoch exhausted"));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_eq!(
        std::fs::read(directory.path().join("ordered.next")).unwrap(),
        b"evidence"
    );
}

#[test]
fn symlinks_hardlinks_and_nonregular_entries_are_not_opened_as_state() {
    use std::os::unix::fs::symlink;
    for entry in [".parrot-ordered.lock", "ordered.state", "ordered.next"] {
        let directory = tempfile::tempdir().unwrap();
        let cfg = config(directory.path());
        drop(ReceiverOwner::open(&cfg, "node").unwrap());
        let foreign = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(foreign.path(), b"private").unwrap();
        let target = directory.path().join(entry);
        if target.exists() {
            std::fs::remove_file(&target).unwrap();
        }
        symlink(foreign.path(), &target).unwrap();
        assert!(ReceiverOwner::open(&cfg, "node").is_err());
        assert_eq!(std::fs::read(foreign.path()).unwrap(), b"private");
        std::fs::remove_file(&target).unwrap();
        std::fs::hard_link(foreign.path(), &target).unwrap();
        assert!(ReceiverOwner::open(&cfg, "node").is_err());
        assert_eq!(std::fs::read(foreign.path()).unwrap(), b"private");
    }
    let directory = tempfile::tempdir().unwrap();
    let fifo = std::ffi::CString::new(
        directory
            .path()
            .join(".parrot-ordered.lock")
            .as_os_str()
            .as_encoded_bytes(),
    )
    .unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(ReceiverOwner::open(&config(directory.path()), "node").is_err());
}

#[test]
fn inherited_owner_cannot_attest_or_unlock_the_original_process() {
    const CASE: &str = "PARROT_ORDERED_RECEIVER_FORK_CASE";
    if std::env::var_os(CASE).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "ordered::recovery::tests::inherited_owner_cannot_attest_or_unlock_the_original_process", "--test-threads=1"])
            .env(CASE, "1").status().unwrap();
        assert!(
            status.success(),
            "isolated fork owner contract failed: {status}"
        );
        return;
    }
    unsafe {
        libc::alarm(5);
    }
    let directory = tempfile::tempdir().unwrap();
    let cfg = config(directory.path());
    let old = ReceiverOwner::open(&cfg, "node").unwrap();
    let receiver = old.receiver();
    drop(old);
    let owner = ReceiverOwner::open(&cfg, "node").unwrap();
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        let invalid = !owner.valid_process() && !owner.proves_retired(receiver, 1);
        drop(owner);
        unsafe {
            libc::_exit(if invalid { 0 } else { 3 });
        }
    }
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert_eq!(status, 0);
    assert!(owner.proves_retired(receiver, 1));
    assert!(
        ReceiverOwner::open(&cfg, "node").is_err(),
        "fork child unlocked the parent owner"
    );
    drop(owner);
    assert!(ReceiverOwner::open(&cfg, "node")
        .unwrap()
        .proves_retired(receiver, 1));
    unsafe {
        libc::alarm(0);
    }
}
