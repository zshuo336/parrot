#![cfg(unix)]
use parrot_remote::raft::{Clock, DurableRaft, ManualClock, RaftStorageError};
use std::sync::{mpsc, Arc, Mutex};
struct HeldClock {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}
impl Clock for HeldClock {
    fn now_ms(&self) -> u64 {
        self.entered.send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        0
    }
    fn election_timeout_ms(&self) -> u64 {
        300
    }
    fn heartbeat_ms(&self) -> u64 {
        50
    }
}
#[test]
fn failed_open_unlocks_while_an_unrelated_fork_retains_the_descriptor() {
    let directory = std::env::temp_dir().join(format!(
        "parrot-raft-open-fork-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    drop(
        DurableRaft::open(
            &directory,
            "cluster",
            "node",
            vec![],
            Arc::new(ManualClock::new()),
        )
        .unwrap(),
    );
    let (entered, observed) = mpsc::channel();
    let (release, held) = mpsc::channel();
    let path = directory.clone();
    let opener = std::thread::spawn(move || {
        DurableRaft::open(
            path,
            "wrong-cluster",
            "node",
            vec![],
            Arc::new(HeldClock {
                entered,
                release: Mutex::new(held),
            }),
        )
    });
    observed
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        // Only async-signal-safe syscalls in the child of a multi-threaded process.
        unsafe {
            libc::close(pipe[1]);
            libc::alarm(10);
            let mut byte = 0_u8;
            libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
            libc::_exit(0);
        }
    }
    unsafe {
        libc::close(pipe[0]);
    }
    release.send(()).unwrap();
    let rejected = opener.join().unwrap();
    let reopened = DurableRaft::open(
        &directory,
        "cluster",
        "node",
        vec![],
        Arc::new(ManualClock::new()),
    );
    let mut status = 0;
    unsafe {
        let byte = 1_u8;
        libc::write(pipe[1], (&byte as *const u8).cast(), 1);
        libc::close(pipe[1]);
        assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
    }
    assert_eq!(status, 0);
    assert!(matches!(rejected, Err(RaftStorageError::Invalid(_))));
    let result = reopened.map(drop).map_err(|error| error.to_string());
    std::fs::remove_dir_all(directory).unwrap();
    assert!(
        result.is_ok(),
        "failed-open lock outlived owner because a fork inherited its descriptor: {result:?}"
    );
}
