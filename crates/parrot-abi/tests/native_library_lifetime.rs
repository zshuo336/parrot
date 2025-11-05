//! A child process tests the last library owner, including its C finalizer.
//! A crash/early unmap must fail the parent test, never kill the test harness.
#![cfg(all(feature = "loader", any(target_os = "linux", target_os = "macos")))]
use parrot_abi::{DylibLoader, UnloadError};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_for(value: &AtomicU64, expected: u64) {
    let end = Instant::now() + Duration::from_secs(10);
    while value.load(Ordering::Acquire) != expected {
        assert!(
            Instant::now() < end,
            "native observation did not reach {expected}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn child(mode: &str) {
    // AtomicU64 has the layout of u64 with atomic alignment, compatible with
    // this C11 fixture on the two supported 64-bit platforms. Shared storage
    // stays alive through the image finalizer; no native pointer is read back.
    let counts: Arc<[AtomicU64; 4]> = Arc::new(std::array::from_fn(|_| AtomicU64::new(0)));
    let path = std::path::PathBuf::from(std::env::var_os("PARROT_NATIVE_LIFETIME_IMAGE").unwrap());
    let h = unsafe { DylibLoader::load(&path, "") }.unwrap();
    DylibLoader
        .construct(&h, &(counts.as_ptr() as usize).to_string())
        .unwrap();
    let rescue = counts.clone();
    let watchdog = std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(10);
        while rescue[1].load(Ordering::Acquire) == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        rescue[1].store(1, Ordering::Release);
        let end = Instant::now() + Duration::from_secs(10);
        while rescue[3].load(Ordering::Acquire) == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        if rescue[3].load(Ordering::Acquire) == 0 {
            // A broken implementation may leave a callback alive after a test
            // assertion panics. Pin its host memory instead of causing test UB.
            std::mem::forget(rescue);
        }
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut cleanup = Box::pin(DylibLoader.unload(h, Duration::from_millis(50)));
        if mode == "timeout" {
            assert_eq!(cleanup.await, Err(UnloadError::TimedOut));
        } else {
            std::future::poll_fn(|cx| {
                assert!(cleanup.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            drop(cleanup);
        }
        wait_for(&counts[0], 1);
        assert_eq!(
            counts[1].load(Ordering::Acquire),
            0,
            "watchdog had to release a blocked runtime"
        );
        assert_eq!(
            counts[2].load(Ordering::Acquire),
            0,
            "active instance destroyed"
        );
        assert_eq!(
            counts[3].load(Ordering::Acquire),
            0,
            "active library closed"
        );
        counts[1].store(1, Ordering::Release);
        wait_for(&counts[3], 1);
        assert_eq!(counts[0].load(Ordering::Acquire), 1);
        assert_eq!(counts[2].load(Ordering::Acquire), 1);
    });
    watchdog.join().unwrap();
}

#[test]
fn last_library_owner_survives_timeout_and_cancellation() {
    if let Ok(mode) = std::env::var("PARROT_NATIVE_LIFETIME_CHILD") {
        child(&mode);
        return;
    }
    let dir = std::env::temp_dir().join(format!("parrot-native-lifetime-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _owned_directory = Cleanup(dir.clone());
    let image = dir.join(if cfg!(target_os = "macos") {
        "lifetime.dylib"
    } else {
        "lifetime.so"
    });
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lifetime.c");
    let output = std::process::Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-fPIC"])
        .arg(if cfg!(target_os = "macos") {
            "-dynamiclib"
        } else {
            "-shared"
        })
        .arg(source)
        .arg("-o")
        .arg(&image)
        .output()
        .expect("C compiler required for the independent ABI fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for mode in ["timeout", "cancel"] {
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "last_library_owner_survives_timeout_and_cancellation",
                "--nocapture",
            ])
            .env("PARROT_NATIVE_LIFETIME_CHILD", mode)
            .env("PARROT_NATIVE_LIFETIME_IMAGE", &image)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Allows macOS's first executable verification; the native gate itself
        // has a separate 10-second watchdog and cannot pass via that fallback.
        let end = Instant::now() + Duration::from_secs(300);
        while process.try_wait().unwrap().is_none() {
            if Instant::now() >= end {
                let _ = process.kill();
                let _ = process.wait();
                panic!("native lifetime child {mode} timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = process.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "child {mode}: {:?}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
