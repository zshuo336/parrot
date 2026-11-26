#![cfg(target_os = "linux")]
use parrot_remote::process_group::PinnedProcess;
use std::{
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn denied_session_observation_returns_an_error_within_the_retirement_budget() {
    const NAME: &str = "denied_session_observation_returns_an_error_within_the_retirement_budget";
    const CHILD: &str = "PARROT_DENIED_SESSION_OBSERVATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("deny-proc.so");
        let compiled = Command::new("cc")
            .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/process_group/deny_proc.c"),
            )
            .args(["-ldl", "-o"])
            .arg(&library)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD, directory.path())
            .env("LD_PRELOAD", library)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "retirement exceeded its budget or lost the error: {:?}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout)
            .contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
        return;
    }
    unsafe {
        libc::alarm(5);
    }
    let marker = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap()).join("ready");
    let mut command = Command::new("python3");
    // The independent parent-death signal also cleans the actual old-code
    // negative case when the watchdog terminates this isolated test process.
    command.args(["-c", "import ctypes,os,pathlib,signal,sys,time; ctypes.CDLL(None).prctl(1,signal.SIGKILL); pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(60)"])
        .arg(&marker);
    let mut process = PinnedProcess::spawn(command).unwrap();
    let ready = Instant::now() + Duration::from_secs(3);
    while !marker.exists() {
        assert!(Instant::now() < ready, "owned fixture did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    let error = process.retire(Duration::from_millis(500)).unwrap_err();
    assert!(error.contains("Permission denied"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "blocking reap exceeded the retirement budget"
    );
    assert!(process.retire(Duration::from_millis(500)).is_err());
    unsafe {
        libc::alarm(0);
    }
}
