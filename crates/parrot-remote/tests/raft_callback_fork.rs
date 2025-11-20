//! A single-threaded harness: returning from fork into Rust is only exercised
//! before any runtime or test-harness threads have been started.
const CASES: [&str; 8] = [
    "fork_during_fresh_open",
    "fork_during_recovery_open",
    "fork_during_idle_tick",
    "fork_during_election_tick",
    "fork_during_vote",
    "fork_during_append",
    "fork_during_read_probe",
    "fork_during_snapshot_installation",
];

#[cfg(unix)]
mod contract {
    use parrot_remote::raft::{
        Clock, DurableRaft, LogEntry, ManualClock, RaftRpc, RaftStorageError, Role,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    struct Scratch {
        path: PathBuf,
        pid: u32,
    }
    impl Scratch {
        fn new() -> Self {
            Self {
                path: tempfile::tempdir().unwrap().keep(),
                pid: std::process::id(),
            }
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            if self.pid == std::process::id() {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    struct ForkClock {
        parent: u32,
        armed: AtomicBool,
        time: AtomicU64,
    }
    impl ForkClock {
        fn new(armed: bool) -> Self {
            Self {
                parent: std::process::id(),
                armed: AtomicBool::new(armed),
                time: AtomicU64::new(0),
            }
        }
        fn child(&self) -> bool {
            self.parent != std::process::id()
        }
    }
    impl Clock for ForkClock {
        fn now_ms(&self) -> u64 {
            if self.armed.swap(false, Ordering::SeqCst) {
                #[cfg(target_os = "linux")]
                assert_eq!(std::fs::read_dir("/proc/self/task").unwrap().count(), 1);
                let pid = unsafe { libc::fork() };
                assert!(pid >= 0);
                if pid == 0 {
                    unsafe { libc::alarm(10) };
                } else {
                    // Keep the parent's operation inside this callback until
                    // the child has checked the untouched disk and owner lock.
                    let mut status = 0;
                    loop {
                        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
                        if waited == pid {
                            break;
                        }
                        assert_eq!(
                            std::io::Error::last_os_error().kind(),
                            std::io::ErrorKind::Interrupted
                        );
                    }
                    eprintln!(
                        "RAFT_FORK_CHILD exited={} code={}",
                        libc::WIFEXITED(status),
                        libc::WEXITSTATUS(status)
                    );
                    assert!(libc::WIFEXITED(status));
                    assert_eq!(libc::WEXITSTATUS(status), 0, "forked operation failed");
                }
            }
            self.time.load(Ordering::SeqCst)
        }
        fn election_timeout_ms(&self) -> u64 {
            300
        }
        fn heartbeat_ms(&self) -> u64 {
            50
        }
    }

    fn open(path: &Path, clock: Arc<dyn Clock>) -> Result<DurableRaft, RaftStorageError> {
        DurableRaft::open(
            path,
            "callback-fork",
            "n",
            vec!["p".into(), "q".into()],
            clock,
        )
    }
    fn locked(path: &Path) {
        assert!(
            matches!(
                open(path, Arc::new(ManualClock::new())),
                Err(RaftStorageError::Storage(_))
            ),
            "fork cleanup released the live parent owner"
        );
    }

    pub fn run(case: &str) {
        let data = Scratch::new();
        let state = data.path.join("raft.state");
        let next = data.path.join("raft.next");
        let opening = case.ends_with("open");
        let recovery = case == "fork_during_recovery_open";
        let clock = Arc::new(ForkClock::new(opening));
        let original = if recovery {
            drop(open(&data.path, Arc::new(ManualClock::new())).unwrap());
            let bytes = std::fs::read(&state).unwrap();
            let mut incomplete = bytes.clone();
            incomplete.push(0);
            std::fs::write(&state, &incomplete).unwrap();
            std::fs::write(&next, b"retain until parent recovery").unwrap();
            Some((bytes, incomplete))
        } else {
            None
        };
        let result = open(&data.path, clock.clone());
        if opening && clock.child() {
            assert!(
                matches!(result, Err(RaftStorageError::Closed)),
                "inherited callback operation must be rejected"
            );
            if let Some((_, incomplete)) = original {
                assert_eq!(std::fs::read(&state).unwrap(), incomplete);
                assert_eq!(
                    std::fs::read(&next).unwrap(),
                    b"retain until parent recovery"
                );
            } else {
                assert!(!state.exists());
                assert!(!next.exists());
            }
            locked(&data.path);
            return;
        }
        let mut node = result.unwrap();
        if opening {
            if let Some((bytes, _)) = original {
                assert_eq!(std::fs::read(&state).unwrap(), bytes);
                assert!(!next.exists());
            }
            drop(node);
            drop(open(&data.path, Arc::new(ManualClock::new())).unwrap());
            return;
        }
        let before = std::fs::read(&state).unwrap();
        clock.armed.store(true, Ordering::SeqCst);
        clock.time.store(
            if case == "fork_during_election_tick" {
                1400
            } else {
                0
            },
            Ordering::SeqCst,
        );
        let result = match case {
            "fork_during_idle_tick" | "fork_during_election_tick" => node.tick(),
            "fork_during_vote" => node.step(
                "p".into(),
                RaftRpc::RequestVote {
                    term: 3,
                    candidate: "p".into(),
                    last_log_index: 0,
                    last_log_term: 0,
                },
            ),
            "fork_during_append" => node.step(
                "p".into(),
                RaftRpc::AppendEntries {
                    term: 3,
                    leader: "p".into(),
                    prev_log_index: 0,
                    prev_log_term: 0,
                    entries: vec![LogEntry {
                        index: 1,
                        term: 3,
                        cmd: b"parent-only".to_vec(),
                    }],
                    leader_commit: 1,
                },
            ),
            "fork_during_read_probe" => node.step(
                "p".into(),
                RaftRpc::ReadIndex {
                    term: 3,
                    leader: "p".into(),
                    context: 5,
                },
            ),
            "fork_during_snapshot_installation" => {
                use sha2::{Digest, Sha256};
                let mut hash = Sha256::new();
                hash.update(b"parrot:raft:snapshot#v1");
                hash.update(7u64.to_le_bytes());
                hash.update(2u64.to_le_bytes());
                hash.update(b"parent-only");
                node.step(
                    "p".into(),
                    RaftRpc::InstallSnapshot {
                        term: 3,
                        leader: "p".into(),
                        index: 7,
                        snapshot_term: 2,
                        digest: hash.finalize().into(),
                        offset: 0,
                        total: 11,
                        data: b"parent-only".to_vec(),
                    },
                )
            }
            _ => unreachable!(),
        };
        if clock.child() {
            assert!(
                matches!(result, Err(RaftStorageError::Closed)),
                "inherited callback operation must be rejected"
            );
            assert!(node.is_closed());
            assert_eq!(node.role(), Role::Follower);
            assert_eq!(node.leader_hint(), None);
            assert_eq!(node.current_term(), 0);
            assert_eq!(node.voted_for(), None);
            assert_eq!(node.commit_index(), 0);
            assert!(node.log().is_empty());
            assert!(node.outbox().is_err());
            assert!(node.committed().is_err());
            assert!(node.pending_snapshot().is_err());
            drop(node);
            assert_eq!(std::fs::read(&state).unwrap(), before);
            assert!(!next.exists());
            locked(&data.path);
            return;
        }
        result.unwrap();
        let term = match case {
            "fork_during_idle_tick" => 0,
            "fork_during_election_tick" => 1,
            _ => 3,
        };
        assert_eq!(node.current_term(), term);
        match case {
            "fork_during_snapshot_installation" => {
                assert_eq!(node.commit_index(), 7);
                assert_eq!(
                    node.pending_snapshot().unwrap().unwrap().data,
                    b"parent-only"
                );
                assert!(node.committed().unwrap().is_empty());
                assert!(matches!(
                    node.outbox().unwrap().as_slice(),
                    [(
                        _,
                        RaftRpc::InstallSnapshotResp {
                            installed: true,
                            index: 7,
                            ..
                        }
                    )]
                ));
            }
            "fork_during_read_probe" => {
                assert_eq!(
                    node.outbox().unwrap(),
                    vec![(
                        "p".into(),
                        RaftRpc::ReadIndexResp {
                            term: 3,
                            context: 5
                        }
                    )]
                );
            }
            "fork_during_vote" => {
                assert_eq!(node.voted_for(), Some("p"));
                assert_eq!(
                    node.outbox().unwrap(),
                    vec![(
                        "p".into(),
                        RaftRpc::RequestVoteResp {
                            term: 3,
                            vote_granted: true
                        }
                    )]
                );
            }
            "fork_during_append" => {
                assert_eq!(node.commit_index(), 1);
                assert_eq!(node.committed().unwrap()[0].cmd, b"parent-only");
                assert_eq!(
                    node.outbox().unwrap(),
                    vec![(
                        "p".into(),
                        RaftRpc::AppendEntriesResp {
                            term: 3,
                            success: true,
                            match_index: 1
                        }
                    )]
                );
            }
            "fork_during_election_tick" => assert_eq!(node.role(), Role::Candidate),
            _ => assert_eq!(std::fs::read(&state).unwrap(), before),
        }
        drop(node);
        let mut recovered = open(&data.path, Arc::new(ManualClock::new())).unwrap();
        assert_eq!(recovered.current_term(), term);
        assert_eq!(
            recovered.voted_for(),
            if case == "fork_during_vote" {
                Some("p")
            } else if term == 1 {
                Some("n")
            } else {
                None
            }
        );
        let committed = recovered.committed().unwrap();
        if case == "fork_during_snapshot_installation" {
            assert!(committed.is_empty());
            assert_eq!(
                recovered.pending_snapshot().unwrap().unwrap().data,
                b"parent-only"
            );
            recovered.snapshot_applied(7, 2).unwrap();
            assert!(recovered.committed().unwrap().is_empty());
        }
        if case == "fork_during_append" {
            assert_eq!(committed.len(), 1);
            assert_eq!(committed[0].cmd, b"parent-only");
        } else {
            assert!(committed.is_empty());
        }
    }
}

#[cfg(unix)]
fn main() {
    use std::io::Write;
    let mut filters = Vec::new();
    let mut skips = Vec::new();
    let (mut exact, mut list, mut ignored, mut quiet) = (false, false, false, false);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--exact" => exact = true,
            "--list" => list = true,
            "--ignored" => ignored = true,
            "--include-ignored" => ignored = false,
            "--quiet" | "-q" => quiet = true,
            "--nocapture" | "--show-output" => {}
            "--skip" => skips.push(args.next().expect("--skip needs a filter")),
            "--test-threads" => {
                assert!(
                    args.next()
                        .expect("option needs a value")
                        .parse::<usize>()
                        .unwrap()
                        > 0
                );
            }
            _ if arg.starts_with("--test-threads=") => {
                assert!(arg[15..].parse::<usize>().unwrap() > 0);
            }
            _ if arg.starts_with("--skip=") => skips.push(arg[7..].to_owned()),
            _ if arg.starts_with('-') => panic!("unsupported test option: {arg}"),
            _ => filters.push(arg),
        }
    }
    let selected: Vec<_> = CASES
        .into_iter()
        .filter(|case| {
            !ignored
                && (filters.is_empty()
                    || filters.iter().any(|filter| {
                        if exact {
                            *case == filter.as_str()
                        } else {
                            case.contains(filter.as_str())
                        }
                    }))
                && !skips.iter().any(|filter| case.contains(filter.as_str()))
        })
        .collect();
    if list {
        for case in selected {
            println!("{case}: test");
        }
        return;
    }
    let parent = std::process::id();
    let mut failed = 0;
    for case in &selected {
        unsafe { libc::alarm(30) };
        if !quiet {
            print!("test {case} ... ");
            std::io::stdout().flush().unwrap();
        }
        let result = std::panic::catch_unwind(|| contract::run(case));
        if std::process::id() != parent {
            unsafe { libc::_exit(if result.is_ok() { 0 } else { 101 }) };
        }
        if result.is_err() {
            failed += 1;
        }
        if !quiet {
            println!("{}", if result.is_ok() { "ok" } else { "FAILED" });
        }
    }
    unsafe { libc::alarm(0) };
    println!(
        "test result: {}. {} passed; {failed} failed; 0 ignored; {} filtered out",
        if failed == 0 { "ok" } else { "FAILED" },
        selected.len() - failed,
        CASES.len() - selected.len()
    );
    if failed != 0 {
        std::process::exit(101);
    }
}

#[cfg(not(unix))]
fn main() {
    println!(
        "test result: ok. 0 passed; 0 failed; {} ignored; 0 filtered out (requires Unix fork)",
        CASES.len()
    );
}
