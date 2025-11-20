//! Process isolation with a completion marker, so an empty exact filter fails.
use std::{
    process::Command,
    time::{Duration, Instant},
};

pub(crate) fn isolated(full_name: &str, body: impl FnOnce()) {
    isolated_with_timeout(full_name, Duration::from_secs(30), body);
}

pub(crate) fn isolated_with_timeout(full_name: &str, timeout: Duration, body: impl FnOnce()) {
    const FLAG: &str = "PARROT_ISOLATED_UNIT_CASE";
    let marker = format!("PARROT_ISOLATED_CASE_OK {full_name}");
    if std::env::var(FLAG).as_deref() == Ok(full_name) {
        body();
        println!("{marker}");
        return;
    }
    let log = tempfile::NamedTempFile::new().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", full_name, "--nocapture"])
        .env(FLAG, full_name)
        .stdout(log.as_file().try_clone().unwrap())
        .stderr(log.as_file().try_clone().unwrap())
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
    let output = std::fs::read_to_string(log.path()).unwrap();
    print!("{output}");
    assert!(
        status.is_some_and(|status| status.success()),
        "{full_name}: {status:?}"
    );
    assert_eq!(
        output.lines().filter(|line| *line == marker).count(),
        1,
        "{full_name}: child did not execute its body exactly once"
    );
    assert!(
        output.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "{full_name}: exact selection must execute one successful child test"
    );
}

#[test]
fn exact_filter_rejects_zero_executed_child_tests() {
    let failure = std::panic::catch_unwind(|| {
        isolated("test_support::__missing_child_case__", || {
            panic!("a missing test must never execute its body");
        });
    })
    .expect_err("an empty exact filter was accepted");
    let message = crate::owned_payload::panic_message(failure);
    assert!(
        message.contains("child did not execute its body exactly once"),
        "{message}"
    );
}
