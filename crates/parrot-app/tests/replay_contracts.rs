use parrot_app::debug::replay::{
    record_sink, FileSink, JsonlRecord, MemSink, RecordSink, ReplayLog,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "parrot-replay-contract-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> PathBuf {
        self.0.join("record.jsonl")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn rec(seq: u64) -> JsonlRecord {
    record_sink(
        seq,
        50 - seq.min(50),
        "rx",
        0x10,
        "/counter",
        "bin:test.Counter",
        &[0, 255],
    )
}
// A regressed blocking open/lock must fail this test without stranding its
// parent. Re-exec the already running harness, and kill/reap only this child.
fn run_probe(test: &str, variable: &str, value: &std::ffi::OsStr) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(variable, value)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "probe {test} failed: {status}");
            return;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("probe {test} blocked for 120 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn append_ownership_is_exclusive_across_processes() {
    const CHILD: &str = "PARROT_REPLAY_LOCK_PROBE";
    if let Some(path) = std::env::var_os(CHILD) {
        let path = PathBuf::from(path);
        assert!(FileSink::open(&path).is_err());
        assert!(ReplayLog::load(&path).is_err());
        return;
    }
    let scratch = Scratch::new();
    let path = scratch.path();
    let mut sink = FileSink::open(&path).unwrap();
    sink.write(&rec(0)).unwrap();
    run_probe(
        "append_ownership_is_exclusive_across_processes",
        CHILD,
        path.as_os_str(),
    );
    sink.finish().unwrap();
    let mut reopened = FileSink::open(&path).unwrap();
    reopened.write(&rec(1)).unwrap();
    reopened.finish().unwrap();
    assert_eq!(ReplayLog::load(&path).unwrap().all().len(), 2);
}
#[cfg(unix)]
#[test]
fn new_recordings_are_private_even_with_a_permissive_umask() {
    const CHILD: &str = "PARROT_REPLAY_MODE_PROBE";
    if std::env::var_os(CHILD).is_none() {
        run_probe(
            "new_recordings_are_private_even_with_a_permissive_umask",
            CHILD,
            std::ffi::OsStr::new("1"),
        );
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    // Process-global state is changed only in this isolated child.
    unsafe {
        libc::umask(0);
    }
    let scratch = Scratch::new();
    let path = scratch.path();
    FileSink::open(&path).unwrap().finish().unwrap();
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
#[test]
fn malformed_payload_is_rejected_by_parse_instead_of_silently_changing_bytes() {
    for hex in ["0", "gg", "00gg01", "é0", "€0"] {
        let mut r = rec(0);
        r.payload_hex = hex.into();
        let text = serde_json::to_string(&r).unwrap();
        assert!(
            ReplayLog::parse(&text).is_err(),
            "accepted invalid hex {hex:?}"
        );
    }
}
#[test]
fn unicode_payload_never_panics() {
    let mut r = rec(0);
    r.payload_hex = "€0".into();
    assert!(
        std::panic::catch_unwind(|| r.payload_bytes()).is_ok(),
        "UTF-8 byte slicing panic"
    );
}
#[test]
fn replay_preflights_all_records_before_any_delivery() {
    let mut bad = rec(1);
    bad.payload_hex = "not hex".into();
    for records in [vec![rec(0), bad], vec![rec(2), rec(1)]] {
        let log = ReplayLog { records };
        let mut effects = 0;
        let result = log.replay(|_| {
            effects += 1;
            Ok(())
        });
        assert!(
            result.is_err(),
            "invalid directly constructed log was replayed"
        );
        assert_eq!(
            effects, 0,
            "corrupt input caused partial business side effects"
        );
    }
}
#[test]
fn file_sink_preserves_monotonic_sequence_across_reopen() {
    let scratch = Scratch::new();
    let p = scratch.path();
    {
        let mut s = FileSink::open(&p).unwrap();
        s.write(&rec(0)).unwrap();
        s.write(&rec(1)).unwrap();
    }
    let before = std::fs::read(&p).unwrap();
    let mut sink = FileSink::open(&p).unwrap();
    assert!(sink.write(&rec(1)).is_err(), "reopen reset the sequence");
    assert_eq!(std::fs::read(&p).unwrap(), before);
    sink.write(&rec(2)).unwrap();
    drop(sink);
    assert_eq!(ReplayLog::load(&p).unwrap().all().len(), 3);
}
#[test]
fn append_cannot_concatenate_onto_unterminated_or_corrupt_tail() {
    let scratch = Scratch::new();
    let p = scratch.path();
    for tail in [serde_json::to_string(&rec(0)).unwrap(), "{\"seq\":".into()] {
        std::fs::write(&p, &tail).unwrap();
        assert!(
            FileSink::open(&p).is_err(),
            "accepted append boundary without complete JSONL line"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), tail);
    }
}
#[test]
fn only_one_file_sink_can_own_the_append_sequence() {
    let scratch = Scratch::new();
    let p = scratch.path();
    let mut first = FileSink::open(&p).unwrap();
    first.write(&rec(0)).unwrap();
    assert!(
        FileSink::open(&p).is_err(),
        "two independent writers own the same sequence"
    );
    drop(first);
    FileSink::open(&p).unwrap();
}
#[test]
fn sinks_reject_corruption_without_appending_it() {
    let scratch = Scratch::new();
    let p = scratch.path();
    let mut file = FileSink::open(&p).unwrap();
    let mut mem = MemSink::default();
    for sink in [&mut file as &mut dyn RecordSink, &mut mem] {
        sink.write(&rec(0)).unwrap();
        let mut bad = rec(1);
        bad.payload_hex = "0g".into();
        assert!(sink.write(&bad).is_err());
        assert!(sink.write(&rec(0)).is_err());
        sink.write(&rec(1)).unwrap();
    }
    drop(file);
    assert_eq!(ReplayLog::load(&p).unwrap().all().len(), 2);
}

#[test]
fn all_byte_values_roundtrip_and_uppercase_hex_is_accepted() {
    let bytes: Vec<u8> = (0..=255).collect();
    let mut r = record_sink(0, 0, "tx", 0x10, "/x", "bin:x", &bytes);
    r.payload_hex.make_ascii_uppercase();
    assert_eq!(r.payload_bytes().unwrap(), bytes);
    for bad in ["0", "0g", "€0", " 00", "00\n"] {
        r.payload_hex = bad.into();
        assert!(r.payload_bytes().is_err());
    }
}
#[test]
fn seq_zero_and_u64_max_are_valid_and_cid_does_not_reorder() {
    let scratch = Scratch::new();
    let p = scratch.path();
    let mut s = FileSink::open(&p).unwrap();
    s.write(&rec(0)).unwrap();
    s.write(&rec(u64::MAX)).unwrap();
    assert!(s.write(&rec(0)).is_err());
    s.finish().unwrap();
    let log = ReplayLog::load(&p).unwrap();
    let mut seen = Vec::new();
    log.replay(|r| {
        seen.push((r.seq, r.cid));
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, vec![(0, 50), (u64::MAX, 0)]);
}
#[test]
fn payload_line_file_and_record_budgets_have_exact_boundaries() {
    use parrot_app::debug::replay::{ReplayError, ReplayLimits};
    let scratch = Scratch::new();
    let p = scratch.path();
    let r = rec(0);
    let text = serde_json::to_string(&r).unwrap() + "\n";
    let exact = ReplayLimits {
        max_file_bytes: text.len(),
        max_line_bytes: text.len(),
        max_records: 1,
        max_payload_bytes: 2,
    };
    let mut file = FileSink::open_with_limits(&p, exact).unwrap();
    file.write(&r).unwrap();
    assert!(file.write(&rec(1)).is_err());
    file.finish().unwrap();
    ReplayLog::load_with_limits(&p, exact).unwrap();
    ReplayLog::parse_with_limits(&text, exact).unwrap();
    for small in [
        ReplayLimits {
            max_file_bytes: text.len() - 1,
            ..exact
        },
        ReplayLimits {
            max_line_bytes: text.len() - 1,
            ..exact
        },
        ReplayLimits {
            max_payload_bytes: 1,
            ..exact
        },
    ] {
        assert!(matches!(
            ReplayLog::parse_with_limits(&text, small),
            Err(ReplayError::ResourceLimit(_))
        ));
        assert!(matches!(
            ReplayLog::load_with_limits(&p, small),
            Err(ReplayError::ResourceLimit(_))
        ));
        let mut sink = MemSink::with_limits(small).unwrap();
        assert!(sink.write(&r).is_err());
        assert!(sink.records().is_empty());
    }
    let more = text + &serde_json::to_string(&rec(1)).unwrap() + "\n";
    assert!(ReplayLog::parse_with_limits(
        &more,
        ReplayLimits {
            max_file_bytes: 4096,
            ..exact
        }
    )
    .is_err());
}
#[test]
fn unknown_fields_direction_and_bad_utf8_are_rejected_at_the_input_line() {
    let good = serde_json::to_string(&rec(0)).unwrap();
    for tail in [
        good.replace("\"rx\"", "\"forward\""),
        good.replace("\"seq\":0", "\"seq\":1,\"mystery\":true"),
        good.replace("00ff", "0g"),
    ] {
        let error = ReplayLog::parse(&(good.clone() + "\n\n" + &tail)).unwrap_err();
        assert!(
            matches!(
                error,
                parrot_app::debug::replay::ReplayError::BadLine { line: 3, .. }
            ),
            "{error:?}"
        );
    }
    let scratch = Scratch::new();
    let p = scratch.path();
    std::fs::write(&p, b"\xff\n").unwrap();
    assert!(ReplayLog::load(&p).is_err());
}
#[test]
fn invalid_limits_fail_before_creating_a_file() {
    use parrot_app::debug::replay::{ReplayError, ReplayLimits};
    let scratch = Scratch::new();
    let p = scratch.path();
    for limits in [
        ReplayLimits {
            max_records: 0,
            ..Default::default()
        },
        ReplayLimits {
            max_file_bytes: usize::MAX,
            ..Default::default()
        },
        ReplayLimits {
            max_payload_bytes: usize::MAX,
            ..Default::default()
        },
    ] {
        assert!(matches!(
            FileSink::open_with_limits(&p, limits),
            Err(ReplayError::InvalidLimits)
        ));
        assert!(!p.exists());
        assert!(ReplayLog::parse_with_limits("", limits).is_err());
    }
}
#[test]
fn reader_cannot_observe_a_live_writers_partial_line() {
    let scratch = Scratch::new();
    let p = scratch.path();
    let mut writer = FileSink::open(&p).unwrap();
    writer.write(&rec(0)).unwrap();
    assert!(ReplayLog::load(&p).is_err());
    writer.finish().unwrap();
    assert_eq!(ReplayLog::load(&p).unwrap().records.len(), 1);
}
#[cfg(unix)]
#[test]
fn partial_os_write_poison_is_observable_and_cannot_be_retried_as_a_new_record() {
    use parrot_app::debug::replay::ReplayError;
    const CHILD: &str = "PARROT_REPLAY_SHORT_WRITE_PROBE";
    if std::env::var_os(CHILD).is_none() {
        run_probe(
            "partial_os_write_poison_is_observable_and_cannot_be_retried_as_a_new_record",
            CHILD,
            std::ffi::OsStr::new("1"),
        );
        return;
    }
    let scratch = Scratch::new();
    let p = scratch.path();
    let mut sink = FileSink::open(&p).unwrap();
    // Keep the process-global fault in this isolated child and restore it
    // even if an assertion unwinds. Exit hooks (including coverage writers)
    // must not inherit the artificial 64-byte limit.
    struct FileSizeFault {
        limit: libc::rlimit,
        signal: libc::sigaction,
        active: bool,
    }
    impl FileSizeFault {
        fn restore(&mut self) {
            let limit = unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &self.limit) };
            let signal =
                unsafe { libc::sigaction(libc::SIGXFSZ, &self.signal, std::ptr::null_mut()) };
            if limit == 0 && signal == 0 {
                self.active = false;
            }
            assert_eq!(limit, 0, "restore file size limit");
            assert_eq!(signal, 0, "restore file size signal action");
        }
    }
    impl Drop for FileSizeFault {
        fn drop(&mut self) {
            if self.active {
                // Best effort during unwinding; normal restoration checks both
                // syscalls before any of the post-fault assertions execute.
                unsafe {
                    libc::setrlimit(libc::RLIMIT_FSIZE, &self.limit);
                    libc::sigaction(libc::SIGXFSZ, &self.signal, std::ptr::null_mut());
                }
            }
        }
    }
    let mut fault = unsafe {
        let mut limit = std::mem::zeroed::<libc::rlimit>();
        let mut signal = std::mem::zeroed::<libc::sigaction>();
        assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit), 0);
        assert_eq!(
            libc::sigaction(libc::SIGXFSZ, std::ptr::null(), &mut signal),
            0
        );
        FileSizeFault {
            limit,
            signal,
            active: true,
        }
    };
    unsafe {
        let mut ignored = std::mem::zeroed::<libc::sigaction>();
        ignored.sa_sigaction = libc::SIG_IGN;
        assert_eq!(libc::sigemptyset(&mut ignored.sa_mask), 0);
        assert_eq!(
            libc::sigaction(libc::SIGXFSZ, &ignored, std::ptr::null_mut()),
            0
        );
        let mut limit = fault.limit;
        limit.rlim_cur = 64;
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
    }
    let r = record_sink(0, 0, "rx", 0x10, "/x", "bin:x", &[0; 1024]);
    assert!(matches!(sink.write(&r), Err(ReplayError::Io(_))));
    fault.restore();
    // Recovery of the OS resource must not make a partially written sink
    // usable again. A separate file proves the underlying fault is gone.
    let recovered = scratch.0.join("resource-recovered");
    std::fs::write(&recovered, [0x5a; 128]).unwrap();
    assert_eq!(std::fs::read(&recovered).unwrap(), [0x5a; 128]);
    assert!(matches!(sink.write(&rec(1)), Err(ReplayError::Poisoned)));
    assert!(matches!(sink.sync(), Err(ReplayError::Poisoned)));
    drop(sink);
    let bytes = std::fs::read(&p).unwrap();
    assert!(!bytes.is_empty());
    assert!(bytes.len() <= 64);
    assert!(FileSink::open(&p).is_err());
    assert_eq!(std::fs::read(&p).unwrap(), bytes);
}

#[test]
fn oversize_sparse_files_and_directories_are_rejected() {
    use parrot_app::debug::replay::{ReplayError, ReplayLimits};
    let scratch = Scratch::new();
    let p = scratch.path();
    std::fs::File::create(&p)
        .unwrap()
        .set_len(ReplayLimits::default().max_file_bytes as u64 + 1)
        .unwrap();
    assert!(matches!(
        ReplayLog::load(&p),
        Err(ReplayError::ResourceLimit(_))
    ));
    assert!(matches!(
        FileSink::open(&p),
        Err(ReplayError::ResourceLimit(_))
    ));
    assert!(ReplayLog::load(&scratch.0).is_err());
    assert!(FileSink::open(&scratch.0).is_err());
}
#[cfg(unix)]
#[test]
fn fifo_paths_are_rejected_and_regular_symlinks_preserve_file_locking() {
    use std::os::unix::ffi::OsStrExt;
    const CHILD: &str = "PARROT_REPLAY_FIFO_PROBE";
    if let Some(dir) = std::env::var_os(CHILD) {
        for name in ["fifo", "fifo-link"] {
            let path = PathBuf::from(&dir).join(name);
            assert!(ReplayLog::load(&path)
                .unwrap_err()
                .to_string()
                .contains("regular file"));
            let err = FileSink::open(&path).err().expect("FIFO writer accepted");
            assert!(err.to_string().contains("regular file"));
        }
        return;
    }
    let scratch = Scratch::new();
    let regular = scratch.path();
    let alias = scratch.0.join("alias");
    let sink = FileSink::open(&regular).unwrap();
    std::os::unix::fs::symlink(&regular, &alias).unwrap();
    assert!(FileSink::open(&alias).is_err());
    drop(sink);
    let mut sink = FileSink::open(&alias).unwrap();
    sink.write(&rec(0)).unwrap();
    sink.finish().unwrap();
    assert_eq!(ReplayLog::load(&alias).unwrap().all().len(), 1);
    let fifo = scratch.0.join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let fifo_link = scratch.0.join("fifo-link");
    std::os::unix::fs::symlink(&fifo, &fifo_link).unwrap();
    run_probe(
        "fifo_paths_are_rejected_and_regular_symlinks_preserve_file_locking",
        CHILD,
        scratch.0.as_os_str(),
    );
}

fn capture_text(path: &std::path::Path) -> String {
    use parrot_app::debug::Recording;
    use parrot_remote::capture::CaptureDirection;
    let recording = Recording::start(path, Default::default(), Default::default()).unwrap();
    let capture = recording.capture();
    capture.connection("node", "mem").unwrap().observe(
        CaptureDirection::Rx,
        &parrot_remote::frame::Frame::tell("/actor", "test", vec![1, 2].into()),
    );
    let report = recording.finish().unwrap();
    assert_eq!(report.written, 1);
    assert_eq!(report.stats.lost, 0);
    std::fs::read_to_string(path).unwrap()
}
#[test]
fn capture_interval_requires_honest_complete_footer_and_never_allows_append() {
    let scratch = Scratch::new();
    let text = capture_text(&scratch.path());
    assert_eq!(ReplayLog::parse(&text).unwrap().records.len(), 1);
    assert!(FileSink::open(&scratch.path()).is_err());
    let lines: Vec<_> = text.lines().collect();
    let truncated = format!("{}\n{}\n", lines[0], lines[1]);
    assert!(ReplayLog::parse(&truncated).is_err());
    assert!(ReplayLog::parse(text.trim_end()).is_err());
    for receipt in [
        serde_json::json!({"written":1,"stats":{"observed":2,"queued":1,"lost":1},"complete":true}),
        serde_json::json!({"written":2,"stats":{"observed":1,"queued":1,"lost":0},"complete":true}),
        serde_json::json!({"written":1,"stats":{"observed":2,"queued":1,"lost":0},"complete":true}),
        serde_json::json!({"written":1,"stats":{"observed":1,"queued":1,"lost":0},"complete":false}),
    ] {
        let invalid = format!("{truncated}{}\n", serde_json::json!({"end":receipt}));
        assert!(ReplayLog::parse(&invalid).is_err());
    }
    for invalid in [
        format!("{}\n{}\n", lines[1], lines[2]),
        format!("{}\n{text}", lines[0]),
        format!("{text}{}\n", lines[1]),
        format!("{text}{}\n", lines[2]),
        format!(
            "{truncated}{}\n{}\n",
            serde_json::to_string(&rec(1)).unwrap(),
            lines[2]
        ),
    ] {
        assert!(ReplayLog::parse(&invalid).is_err());
    }
    let mut frame: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    frame["frame"]["seq"] = 1.into();
    assert!(ReplayLog::parse(&format!("{}\n{frame}\n{}\n", lines[0], lines[2])).is_err());
    frame["frame"]["seq"] = 0.into();
    frame["frame"].as_object_mut().unwrap().remove("wire");
    assert!(ReplayLog::parse(&format!("{}\n{frame}\n{}\n", lines[0], lines[2])).is_err());
}
#[test]
fn capture_drop_and_explicit_loss_never_become_replayable() {
    use parrot_app::debug::Recording;
    use parrot_remote::capture::{CaptureDirection, CaptureLimits};
    let scratch = Scratch::new();
    let recording =
        Recording::start(&scratch.path(), Default::default(), Default::default()).unwrap();
    let capture = recording.capture();
    drop(recording);
    assert!(ReplayLog::load(&scratch.path()).is_err());
    assert!(std::fs::read_to_string(scratch.path())
        .unwrap()
        .contains("\"complete\":false"));
    let second = scratch.0.join("loss.jsonl");
    let recording = Recording::start(
        &second,
        CaptureLimits {
            max_event_bytes: 512,
            max_pending_bytes: 512,
            ..Default::default()
        },
        Default::default(),
    )
    .unwrap();
    recording
        .capture()
        .connection("node", "mem")
        .unwrap()
        .observe(
            CaptureDirection::Rx,
            &parrot_remote::frame::Frame::tell("/actor", "test", vec![0; 1024].into()),
        );
    assert!(recording.finish().is_err());
    assert!(ReplayLog::load(&second).is_err());
    assert_eq!(capture.stats().observed, 0);
}
#[test]
fn capture_file_is_exclusive_private_and_budget_failure_retains_incomplete_evidence() {
    use parrot_app::debug::{Recording, ReplayLimits};
    use parrot_remote::capture::CaptureDirection;
    let scratch = Scratch::new();
    let recording = Recording::start(
        &scratch.path(),
        Default::default(),
        ReplayLimits {
            max_file_bytes: 64,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(Recording::start(&scratch.path(), Default::default(), Default::default()).is_err());
    assert!(ReplayLog::load(&scratch.path()).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(scratch.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }
    let capture = recording.capture();
    let conn = capture.connection("node", "mem").unwrap();
    conn.observe(
        CaptureDirection::Rx,
        &parrot_remote::frame::Frame::tell("/actor", "test", vec![1].into()),
    );
    assert!(recording.finish().is_err());
    let text = std::fs::read_to_string(scratch.path()).unwrap();
    assert!(text.contains("\"start\""));
    assert!(!text.contains("\"end\""));
    assert!(ReplayLog::load(&scratch.path()).is_err());
    let prior = capture.stats();
    conn.observe(
        CaptureDirection::Rx,
        &parrot_remote::frame::Frame::heartbeat(),
    );
    assert_eq!(capture.stats(), prior);
}

#[test]
fn required_capture_format_rejects_empty_and_legacy_files() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path(), "").unwrap();
    assert!(ReplayLog::load_capture(&scratch.path()).is_err());
    std::fs::write(scratch.path(), serde_json::to_string(&rec(0)).unwrap()).unwrap();
    assert!(ReplayLog::load(&scratch.path()).is_ok());
    assert!(ReplayLog::load_capture(&scratch.path()).is_err());
    let other = scratch.0.join("empty-capture.jsonl");
    parrot_app::debug::Recording::start(&other, Default::default(), Default::default())
        .unwrap()
        .finish()
        .unwrap();
    assert!(ReplayLog::load_capture(&other).unwrap().records.is_empty());
}

#[cfg(unix)]
fn forked_writer_lock(explicit_finish: bool) {
    let scratch = Scratch::new();
    let mut writer = FileSink::open(&scratch.path()).unwrap();
    writer.write(&rec(0)).unwrap();
    let mut gate = [0; 2];
    assert_eq!(unsafe { libc::pipe(gate.as_mut_ptr()) }, 0);
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        // The harness is multithreaded. Only async-signal-safe syscalls are
        // allowed after fork; never allocate or run Rust destructors here.
        unsafe {
            libc::close(gate[1]);
            let mut byte = 0u8;
            libc::read(gate[0], (&mut byte as *mut u8).cast(), 1);
            libc::_exit(0);
        }
    }
    unsafe {
        libc::close(gate[0]);
    }
    let finished = if explicit_finish {
        writer.finish().map(|_| ())
    } else {
        drop(writer);
        Ok(())
    };
    let readable = ReplayLog::load(&scratch.path());
    // Reap the owned child before assertions, including on the old failing code.
    let mut status = 0;
    unsafe {
        let byte = 1u8;
        libc::write(gate[1], (&byte as *const u8).cast(), 1);
        libc::close(gate[1]);
        libc::waitpid(child, &mut status, 0);
    }
    assert_eq!(status, 0);
    finished.unwrap();
    assert!(
        readable.is_ok(),
        "finished writer left its lock inherited in a child: {readable:?}"
    );
}

#[cfg(unix)]
#[test]
fn finished_writer_unlocks_even_while_a_forked_child_holds_its_descriptor() {
    forked_writer_lock(true);
}
#[cfg(unix)]
#[test]
fn dropped_writer_unlocks_even_while_a_forked_child_holds_its_descriptor() {
    forked_writer_lock(false);
}
