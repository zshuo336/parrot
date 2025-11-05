use parrot_remote::durable::{Wal, WalRecord};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-wal-contract-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn rec(seq: u64) -> WalRecord {
    WalRecord {
        endpoint: "edge".into(),
        sender: "source".into(),
        seq,
        path: "/sink".into(),
        type_key: "bin:test".into(),
        payload: vec![7; 64].into(),
    }
}
fn child(test: &str, key: &str, value: &std::ffi::OsStr) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(key, value)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "child failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("WAL operation blocked");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn second_writer_cannot_open_or_recover_an_active_wal() {
    let scratch = Scratch::new();
    let wal = Wal::open(&scratch.0).unwrap();
    wal.append(rec(1)).unwrap();
    let before = std::fs::read(scratch.0.join("seg-00000001.log")).unwrap();
    assert!(Wal::open(&scratch.0).is_err());
    assert_eq!(
        std::fs::read(scratch.0.join("seg-00000001.log")).unwrap(),
        before
    );
    drop(wal);
    assert!(Wal::open(&scratch.0).is_ok());
}
#[test]
fn writer_lock_is_exclusive_across_processes() {
    const KEY: &str = "PARROT_WAL_LOCK_CHILD";
    if let Some(path) = std::env::var_os(KEY) {
        assert!(Wal::open(path).is_err());
        return;
    }
    let scratch = Scratch::new();
    let wal = Wal::open(&scratch.0).unwrap();
    wal.append(rec(1)).unwrap();
    child(
        "writer_lock_is_exclusive_across_processes",
        KEY,
        scratch.0.as_os_str(),
    );
    drop(wal);
    assert!(Wal::open(&scratch.0).is_ok());
}
#[test]
fn complete_tail_checksum_corruption_is_not_silently_deleted() {
    let scratch = Scratch::new();
    let wal = Wal::open(&scratch.0).unwrap();
    wal.append(rec(1)).unwrap();
    wal.sync().unwrap();
    drop(wal);
    let path = scratch.0.join("seg-00000001.log");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[8] ^= 1;
    std::fs::write(&path, &bytes).unwrap();
    assert!(Wal::open(&scratch.0).is_err());
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
#[test]
fn zero_segment_size_is_invalid_before_directory_creation() {
    let scratch = Scratch::new();
    let path = scratch.0.join("new");
    assert!(Wal::open_with(&path, 0).is_err());
    assert!(!path.exists());
}
#[test]
fn exhausted_segment_id_is_reported_without_panic_or_mutation() {
    let scratch = Scratch::new();
    let path = scratch.0.join(format!("seg-{}.log", u64::MAX));
    std::fs::write(&path, []).unwrap();
    let result = std::panic::catch_unwind(|| Wal::open(&scratch.0));
    assert!(result.is_ok());
    assert!(result.unwrap().is_err());
    assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
}
#[test]
fn ambiguous_segment_names_are_rejected() {
    let scratch = Scratch::new();
    std::fs::write(scratch.0.join("seg-00000001.log"), []).unwrap();
    std::fs::write(scratch.0.join("seg-1.log"), b"evidence").unwrap();
    assert!(Wal::open(&scratch.0).is_err());
    assert_eq!(
        std::fs::read(scratch.0.join("seg-1.log")).unwrap(),
        b"evidence"
    );
}
#[cfg(unix)]
#[test]
fn partial_os_write_poison_blocks_all_later_mutation() {
    const KEY: &str = "PARROT_WAL_PARTIAL_CHILD";
    if std::env::var_os(KEY).is_none() {
        child(
            "partial_os_write_poison_blocks_all_later_mutation",
            KEY,
            std::ffi::OsStr::new("1"),
        );
        return;
    }
    let scratch = Scratch::new();
    let wal = Wal::open(&scratch.0).unwrap();
    let mut original = unsafe { std::mem::zeroed::<libc::rlimit>() };
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut original), 0);
        let limited = libc::rlimit {
            rlim_cur: 16,
            ..original
        };
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limited), 0);
    }
    assert!(wal.append(rec(1)).is_err());
    unsafe {
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &original), 0);
    }
    let path = scratch.0.join("seg-00000001.log");
    let partial = std::fs::read(&path).unwrap();
    assert_eq!(partial.len(), 16);
    assert!(wal.sync().is_err());
    assert!(wal.append(rec(2)).is_err());
    assert!(wal.ack("edge", "source", 1).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), partial);
    drop(wal);
    let recovered = Wal::open(&scratch.0).unwrap();
    recovered.append(rec(1)).unwrap();
    recovered.sync().unwrap();
}
#[test]
fn failed_segment_roll_poison_cannot_ack_away_unconfirmed_data() {
    let scratch = Scratch::new();
    let wal = Wal::open_with(&scratch.0, 1).unwrap();
    std::fs::create_dir(scratch.0.join("seg-00000002.log")).unwrap();
    assert!(wal.append(rec(1)).is_err());
    std::fs::remove_dir(scratch.0.join("seg-00000002.log")).unwrap();
    assert!(wal.append(rec(2)).is_err());
    assert!(wal.ack("edge", "source", 1).is_err());
    assert!(wal.sync().is_err());
}

#[test]
fn incomplete_final_body_is_repaired_but_sealed_damage_is_preserved() {
    use std::io::Write;
    let scratch = Scratch::new();
    let wal = Wal::open(&scratch.0).unwrap();
    wal.append(rec(1)).unwrap();
    wal.sync().unwrap();
    drop(wal);
    let path = scratch.0.join("seg-00000001.log");
    let complete = std::fs::read(&path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[64, 0, 0, 0, 0, 0, 0, 0, 1, 2])
        .unwrap();
    let wal = Wal::open_with(&scratch.0, 1).unwrap();
    assert_eq!(wal.replay("edge").unwrap(), vec![rec(1)]);
    assert_eq!(std::fs::read(&path).unwrap(), complete);
    wal.append(rec(2)).unwrap();
    wal.sync().unwrap();
    drop(wal);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[1, 2])
        .unwrap();
    let corrupt = std::fs::read(&path).unwrap();
    assert!(Wal::open(&scratch.0).is_err());
    assert_eq!(std::fs::read(path).unwrap(), corrupt);
}
#[test]
fn resource_rejection_precedes_append_and_does_not_poison_good_state() {
    use parrot_remote::durable::WalLimits;
    let scratch = Scratch::new();
    let limits = WalLimits {
        max_pending_records: 1,
        ..Default::default()
    };
    let wal = Wal::open_with_limits(&scratch.0, 1024, limits).unwrap();
    wal.append(rec(1)).unwrap();
    let path = scratch.0.join("seg-00000001.log");
    let before = std::fs::read(&path).unwrap();
    assert!(wal.append(rec(2)).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(wal.replay("edge").unwrap(), vec![rec(1)]);
    wal.ack("edge", "source", 1).unwrap();
    wal.append(rec(2)).unwrap();
    wal.sync().unwrap();
    drop(wal);
    let wal = Wal::open_with_limits(&scratch.0, 1024, limits).unwrap();
    assert_eq!(wal.replay("edge").unwrap(), vec![rec(2)]);
}
#[test]
fn oversize_disk_and_record_headers_are_rejected_without_truncation() {
    use parrot_remote::durable::WalLimits;
    let scratch = Scratch::new();
    let path = scratch.0.join("seg-00000001.log");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(4097).unwrap();
    drop(file);
    assert!(Wal::open_with_limits(
        &scratch.0,
        1,
        WalLimits {
            max_disk_bytes: 4096,
            ..Default::default()
        }
    )
    .is_err());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 4097);
    let bytes = [255; 8];
    std::fs::write(&path, bytes).unwrap();
    assert!(Wal::open(&scratch.0).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
#[test]
fn pending_budget_failure_does_not_repair_an_unrelated_incomplete_tail() {
    use parrot_remote::durable::WalLimits;
    use std::io::Write;
    let scratch = Scratch::new();
    let wal = Wal::open(&scratch.0).unwrap();
    wal.append(rec(1)).unwrap();
    wal.append(rec(2)).unwrap();
    drop(wal);
    let path = scratch.0.join("seg-00000001.log");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[7, 8])
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(Wal::open_with_limits(
        &scratch.0,
        1024,
        WalLimits {
            max_pending_records: 1,
            ..Default::default()
        }
    )
    .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let recovered = Wal::open(&scratch.0).unwrap();
    assert_eq!(recovered.pending_len().unwrap(), 2);
}
#[test]
fn missing_middle_segment_and_invalid_names_cannot_hide_data_loss() {
    let scratch = Scratch::new();
    std::fs::write(scratch.0.join("seg-00000001.log"), []).unwrap();
    std::fs::write(scratch.0.join("seg-00000003.log"), []).unwrap();
    assert!(Wal::open(&scratch.0).is_err());
}
#[test]
fn segment_count_and_record_byte_limits_are_checked_before_storage_mutation() {
    use parrot_remote::durable::WalLimits;
    let scratch = Scratch::new();
    let wal = Wal::open_with_limits(
        &scratch.0,
        1,
        WalLimits {
            max_segments: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(wal.append(rec(1)).is_err());
    assert_eq!(
        std::fs::metadata(scratch.0.join("seg-00000001.log"))
            .unwrap()
            .len(),
        0
    );
    wal.sync().unwrap();
    drop(wal);
    let other = scratch.0.join("other");
    let wal = Wal::open_with_limits(
        &other,
        1024,
        WalLimits {
            max_record_bytes: 32,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(wal.append(rec(1)).is_err());
    assert_eq!(wal.pending_len().unwrap(), 0);
}
#[test]
fn acknowledgement_stream_and_byte_limits_fail_without_discarding_pending() {
    use parrot_remote::durable::WalLimits;
    let scratch = Scratch::new();
    let wal = Wal::open_with_limits(
        &scratch.0,
        1024,
        WalLimits {
            max_streams: 1,
            ..Default::default()
        },
    )
    .unwrap();
    wal.append(rec(1)).unwrap();
    wal.ack("elsewhere", "source", 1).unwrap();
    assert!(wal.ack("edge", "source", 1).is_err());
    assert_eq!(wal.replay("edge").unwrap(), vec![rec(1)]);
    wal.sync().unwrap();
    let other = scratch.0.join("other");
    let wal = Wal::open_with_limits(
        &other,
        1024,
        WalLimits {
            max_watermark_bytes: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(wal.ack("edge", "source", 1).is_err());
    wal.sync().unwrap();
}
#[cfg(unix)]
#[test]
fn special_files_and_symlink_segments_never_block_or_escape_the_directory() {
    const KEY: &str = "PARROT_WAL_SPECIAL_CHILD";
    if std::env::var_os(KEY).is_none() {
        child(
            "special_files_and_symlink_segments_never_block_or_escape_the_directory",
            KEY,
            std::ffi::OsStr::new("1"),
        );
        return;
    }
    use std::os::unix::fs::symlink;
    let scratch = Scratch::new();
    let segment = scratch.0.join("seg-00000001.log");
    let fifo = std::ffi::CString::new(segment.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(Wal::open(&scratch.0).is_err());
    std::fs::remove_file(&segment).unwrap();
    let outside = scratch.0.join("outside");
    std::fs::write(&outside, b"preserve").unwrap();
    symlink(&outside, &segment).unwrap();
    assert!(Wal::open(&scratch.0).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"preserve");
    std::fs::remove_file(segment).unwrap();
    std::fs::remove_file(scratch.0.join(".parrot-wal.lock")).unwrap();
    symlink(&outside, scratch.0.join(".parrot-wal.lock")).unwrap();
    assert!(Wal::open(&scratch.0).is_err());
}
#[cfg(unix)]
#[test]
fn process_crash_releases_writer_lock_and_preserves_synced_records() {
    const KEY: &str = "PARROT_WAL_CRASH_CHILD";
    if let Some(path) = std::env::var_os(KEY) {
        let wal = Wal::open(&path).unwrap();
        wal.append(rec(1)).unwrap();
        wal.sync().unwrap();
        std::fs::write(PathBuf::from(path).join("ready"), b"ready").unwrap();
        loop {
            std::thread::park();
        }
    }
    let scratch = Scratch::new();
    let mut process = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "process_crash_releases_writer_lock_and_preserves_synced_records",
        ])
        .env(KEY, &scratch.0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while !scratch.0.join("ready").exists() {
        if Instant::now() >= deadline || process.try_wait().unwrap().is_some() {
            let _ = process.kill();
            let _ = process.wait();
            panic!("writer child did not become ready");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let locked = Wal::open(&scratch.0).is_err();
    process.kill().unwrap();
    process.wait().unwrap();
    assert!(locked);
    let wal = Wal::open(&scratch.0).unwrap();
    assert_eq!(wal.replay("edge").unwrap(), vec![rec(1)]);
    wal.append(rec(2)).unwrap();
    wal.sync().unwrap();
}

#[test]
fn acknowledgement_checkpoint_reclaims_the_previous_active_file() {
    use parrot_remote::durable::WalLimits;
    let scratch = Scratch::new();
    let limits = WalLimits {
        max_disk_bytes: 2048,
        ..Default::default()
    };
    let wal = Wal::open_with_limits(&scratch.0, 1024, limits).unwrap();
    for seq in 1..=100 {
        wal.append(rec(seq)).unwrap();
        wal.ack("edge", "source", seq).unwrap();
    }
    assert_eq!(wal.pending_len().unwrap(), 0);
    drop(wal);
    let disk: u64 = std::fs::read_dir(&scratch.0)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name().to_string_lossy().starts_with("seg-"))
        .map(|e| e.metadata().unwrap().len())
        .sum();
    assert!(
        disk < 128,
        "only one compact watermark should survive, found {disk} bytes"
    );
    let wal = Wal::open_with_limits(&scratch.0, 1024, limits).unwrap();
    wal.append(rec(100)).unwrap();
    assert_eq!(wal.pending_len().unwrap(), 0);
    wal.append(rec(101)).unwrap();
    assert_eq!(wal.replay("edge").unwrap(), vec![rec(101)]);
}
#[test]
fn exact_disk_byte_budget_rejects_one_more_record_and_recovers_the_accepted_prefix() {
    use parrot_remote::durable::WalLimits;
    let scratch = Scratch::new();
    let record = rec(1);
    let wire = bincode::serde::encode_to_vec(&record, bincode::config::standard())
        .unwrap()
        .len()
        + 8;
    let wal = Wal::open_with_limits(
        &scratch.0,
        wire as u64,
        WalLimits {
            max_disk_bytes: wire as u64,
            ..Default::default()
        },
    )
    .unwrap();
    wal.append(record).unwrap();
    assert!(wal.append(rec(2)).is_err());
    wal.sync().unwrap();
    drop(wal);
    let wal = Wal::open_with_limits(
        &scratch.0,
        wire as u64,
        WalLimits {
            max_disk_bytes: wire as u64,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(wal.replay("edge").unwrap(), vec![rec(1)]);
}

#[cfg(unix)]
#[test]
fn partial_checkpoint_failure_preserves_old_segments_and_poisoned_handle_cannot_replay() {
    const KEY: &str = "PARROT_WAL_ACK_PARTIAL_CHILD";
    if std::env::var_os(KEY).is_none() {
        child(
            "partial_checkpoint_failure_preserves_old_segments_and_poisoned_handle_cannot_replay",
            KEY,
            std::ffi::OsStr::new("1"),
        );
        return;
    }
    let scratch = Scratch::new();
    let wal = Wal::open(&scratch.0).unwrap();
    wal.append(rec(1)).unwrap();
    wal.sync().unwrap();
    let old = scratch.0.join("seg-00000001.log");
    let original_bytes = std::fs::read(&old).unwrap();
    let mut original = unsafe { std::mem::zeroed::<libc::rlimit>() };
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut original), 0);
        let limited = libc::rlimit {
            rlim_cur: 8,
            ..original
        };
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limited), 0);
    }
    let failed = wal.ack("edge", "source", 1);
    unsafe {
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &original), 0);
    }
    assert!(failed.is_err());
    assert!(wal.replay("edge").is_err());
    assert!(wal.pending_len().is_err());
    assert!(wal.sync().is_err());
    assert_eq!(std::fs::read(old).unwrap(), original_bytes);
    drop(wal);
    let recovered = Wal::open(&scratch.0).unwrap();
    assert_eq!(recovered.replay("edge").unwrap(), vec![rec(1)]);
    recovered.ack("edge", "source", 1).unwrap();
    drop(recovered);
    assert!(Wal::open(&scratch.0)
        .unwrap()
        .replay("edge")
        .unwrap()
        .is_empty());
}
#[cfg(unix)]
#[test]
fn dropped_writer_releases_lock_despite_child_inheriting_descriptor() {
    let scratch = Scratch::new();
    let wal = Wal::open(&scratch.0).unwrap();
    wal.append(rec(1)).unwrap();
    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            libc::close(pipe[1]);
            let mut byte = 0u8;
            libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
            libc::_exit(0);
        }
    }
    unsafe {
        libc::close(pipe[0]);
    }
    drop(wal);
    let opened = Wal::open(&scratch.0);
    let mut status = 0;
    unsafe {
        let byte = 1u8;
        libc::write(pipe[1], (&byte as *const u8).cast(), 1);
        libc::close(pipe[1]);
        libc::waitpid(pid, &mut status, 0);
    }
    assert_eq!(status, 0);
    assert!(
        opened.is_ok(),
        "inherited descriptor kept writer lock after parent drop"
    );
}
#[cfg(unix)]
#[test]
fn new_wal_files_and_directory_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let dir = scratch.0.join("private");
    let wal = Wal::open(&dir).unwrap();
    wal.append(rec(1)).unwrap();
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o077,
        0
    );
    for entry in std::fs::read_dir(dir).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o077,
            0
        );
    }
}
#[test]
fn complete_crc_valid_but_hostile_codec_record_is_not_repaired() {
    let scratch = Scratch::new();
    let path = scratch.0.join("seg-00000001.log");
    // First field is a String. This encodes a u64::MAX declared length with no
    // backing bytes; valid CRC must not turn it into an allocation request.
    let body = bincode::serde::encode_to_vec(u64::MAX, bincode::config::standard()).unwrap();
    let mut crc = 0xffff_ffffu32;
    for byte in &body {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb88320 } else { 0 };
        }
    }
    let mut frame = (body.len() as u32).to_le_bytes().to_vec();
    frame.extend((!crc).to_le_bytes());
    frame.extend(body);
    std::fs::write(&path, &frame).unwrap();
    assert!(Wal::open(&scratch.0).is_err());
    assert_eq!(std::fs::read(path).unwrap(), frame);
}

#[test]
fn repeated_recovery_does_not_consume_empty_segments() {
    use parrot_remote::durable::WalLimits;
    let scratch = Scratch::new();
    let limits = WalLimits {
        max_segments: 2,
        ..Default::default()
    };
    for _ in 0..10 {
        let wal = Wal::open_with_limits(&scratch.0, 1024, limits).unwrap();
        wal.sync().unwrap();
    }
    let segments = std::fs::read_dir(&scratch.0)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("seg-")
        })
        .count();
    assert_eq!(segments, 1);
    let wal = Wal::open_with_limits(&scratch.0, 1024, limits).unwrap();
    wal.append(rec(1)).unwrap();
    drop(wal);
    let wal = Wal::open_with_limits(&scratch.0, 1024, limits).unwrap();
    wal.append(rec(2)).unwrap();
    assert_eq!(wal.replay("edge").unwrap(), vec![rec(1), rec(2)]);
}
