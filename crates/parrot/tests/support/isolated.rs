// SPDX-License-Identifier: AGPL-3.0-only
//! Process isolation with a completion marker, so an empty exact filter fails.
use std::{
    process::Command,
    time::{Duration, Instant},
};

pub(crate) fn run(full_name: &str, flag: &str, timeout: Duration, body: impl FnOnce()) {
    let marker = format!("PARROT_ISOLATED_CASE_OK {full_name}");
    if std::env::var(flag).as_deref() == Ok(full_name) {
        body();
        println!("{marker}");
        return;
    }
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", full_name, "--nocapture"])
        .env(flag, full_name)
        .stdout(stdout.as_file().try_clone().unwrap())
        .stderr(stderr.as_file().try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // libtest writes its summary in fragments. Worker diagnostics must not split it.
    let output = std::fs::read_to_string(stdout.path()).unwrap();
    print!("{output}");
    eprint!(
        "{}",
        String::from_utf8_lossy(&std::fs::read(stderr.path()).unwrap())
    );
    assert!(
        status.is_some_and(|status| status.success()),
        "{full_name}: {status:?}"
    );
    assert_eq!(
        output
            .lines()
            .filter(|line| line.trim_end().ends_with(&marker))
            .count(),
        1,
        "{full_name}: child did not execute its body exactly once"
    );
    assert!(
        output.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "{full_name}: exact selection must execute one successful child test"
    );
}
