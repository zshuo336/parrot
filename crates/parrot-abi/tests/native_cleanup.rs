//! Observe native hooks independently of the loader's report/registry.
#![cfg(feature = "loader")]
// A current-thread runtime and this test-only lock serialize shared fixture counters.
#![allow(clippy::await_holding_lock)]
mod common;

use parrot_abi::{DylibHandle, DylibLoader, UnloadError, ABI_ERR_STATE};
use std::future::Future;
use std::time::{Duration, Instant};

struct Observer {
    _lib: libloading::Library,
    observe: unsafe extern "C" fn(u32) -> u64,
    release: unsafe extern "C" fn(),
}
impl Observer {
    fn new() -> Self {
        unsafe {
            let lib = libloading::Library::new(common::testcomp()).unwrap();
            let observe = *lib.get(b"lifecycle_observe").unwrap();
            let release = *lib.get(b"lifecycle_release").unwrap();
            let reset = lib
                .get::<unsafe extern "C" fn()>(b"lifecycle_reset")
                .unwrap();
            reset();
            Self {
                _lib: lib,
                observe,
                release,
            }
        }
    }
    fn count(&self, index: u32) -> u64 {
        unsafe { (self.observe)(index) }
    }
    fn release(&self) {
        unsafe { (self.release)() }
    }
    fn wait(&self, index: u32, expected: u64) {
        let until = Instant::now() + Duration::from_secs(10);
        while self.count(index) != expected {
            assert!(
                Instant::now() < until,
                "native observation {index}: wanted {expected}, got {}",
                self.count(index)
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn finished(&self, instances: u64) {
        self.wait(1, instances);
        assert_eq!(self.count(0), instances);
        assert_eq!(self.count(2), 0, "destroy before drain or duplicate drain");
        assert_eq!(
            self.count(3),
            instances,
            "all drains precede the first destroy"
        );
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        self.release();
    }
}
fn handle(configs: &[&str]) -> DylibHandle {
    let h = unsafe { DylibLoader::load(&common::testcomp(), "") }.unwrap();
    for cfg in configs {
        DylibLoader.construct(&h, cfg).unwrap();
    }
    h
}

#[test]
fn direct_drop_drains_and_destroys_every_instance_once_without_runtime() {
    let _lock = common::load_mutex();
    let observer = Observer::new();
    drop(handle(&["{}", "{}", "{}"]));
    observer.finished(3);
}

#[test]
fn direct_drop_continues_destroy_after_drain_error() {
    let _lock = common::load_mutex();
    let observer = Observer::new();
    drop(handle(&["error", "{}"]));
    observer.finished(2);
}

#[tokio::test]
async fn successful_unload_reports_actual_hooks_and_destruction() {
    let _lock = common::load_mutex();
    let observer = Observer::new();
    let report = DylibLoader
        .unload(handle(&["{}", "{}"]), Duration::from_secs(10))
        .await
        .unwrap();
    observer.finished(2);
    assert_eq!(report.drained, 2);
    assert_eq!(report.aborted, 0);
    assert!(!report.force_closed);
}

#[tokio::test]
async fn drain_error_is_reported_after_all_instances_are_destroyed() {
    let _lock = common::load_mutex();
    let observer = Observer::new();
    let error = DylibLoader
        .unload(handle(&["error", "{}", "error"]), Duration::from_secs(10))
        .await
        .unwrap_err();
    match error {
        UnloadError::DrainFailed { codes, report } => {
            assert_eq!(codes, vec![ABI_ERR_STATE, ABI_ERR_STATE]);
            assert_eq!(report.drained, 1);
            assert_eq!(report.aborted, 0);
            assert!(!report.force_closed);
        }
        other => panic!("unexpected {other:?}"),
    }
    observer.finished(3);
}

#[test]
fn unpolled_unload_future_starts_and_owns_cleanup() {
    let _lock = common::load_mutex();
    let observer = Observer::new();
    let pending = DylibLoader.unload(handle(&["gate"]), Duration::from_secs(10));
    observer.wait(0, 1);
    assert_eq!(observer.count(1), 0);
    drop(pending);
    assert_eq!(observer.count(1), 0);
    observer.release();
    observer.finished(1);
}

#[tokio::test]
async fn timeout_leaves_active_native_hook_owned_until_release() {
    let _lock = common::load_mutex();
    let observer = Observer::new();
    let pending = DylibLoader.unload(handle(&["gate"]), Duration::from_millis(50));
    observer.wait(0, 1);
    assert_eq!(pending.await, Err(UnloadError::TimedOut));
    assert_eq!(observer.count(1), 0, "cannot destroy active native state");
    observer.release();
    observer.finished(1);
}

#[tokio::test]
async fn cancelling_polled_wait_keeps_cleanup_and_current_thread_runtime_responsive() {
    let _lock = common::load_mutex();
    let observer = Observer::new();
    let mut pending = Box::pin(DylibLoader.unload(handle(&["gate"]), Duration::from_secs(10)));
    // Explicitly poll the waiting future while the native hook stays gated.
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    observer.wait(0, 1);
    drop(pending);
    assert_eq!(observer.count(1), 0);
    observer.release();
    observer.finished(1);
}

#[tokio::test]
async fn overflowing_timeout_returns_error_but_does_not_abandon_cleanup() {
    let _lock = common::load_mutex();
    let observer = Observer::new();
    assert_eq!(
        DylibLoader.unload(handle(&["{}"]), Duration::MAX).await,
        Err(UnloadError::InvalidTimeout)
    );
    observer.finished(1);
}

#[test]
fn polling_without_runtime_reports_error_and_still_cleans_up() {
    let _lock = common::load_mutex();
    let observer = Observer::new();
    let mut pending = Box::pin(DylibLoader.unload(handle(&["{}"]), Duration::from_secs(10)));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert_eq!(
        pending.as_mut().poll(&mut cx),
        std::task::Poll::Ready(Err(UnloadError::NoRuntime))
    );
    observer.finished(1);
}
