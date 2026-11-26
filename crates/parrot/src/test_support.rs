//! Isolated library contracts.
use std::time::Duration;

#[path = "../tests/support/isolated.rs"]
mod child;

pub(crate) fn isolated(full_name: &str, body: impl FnOnce()) {
    isolated_with_timeout(full_name, Duration::from_secs(30), body);
}

pub(crate) fn isolated_with_timeout(full_name: &str, timeout: Duration, body: impl FnOnce()) {
    child::run(full_name, "PARROT_ISOLATED_UNIT_CASE", timeout, body);
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
