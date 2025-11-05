#![cfg(feature = "runtime")]

use parrot_wasm::runtime::{CompilationLimits, WasmRuntime};
use parrot_wasm::{ComponentError, HostCtx, WasmConfig};
use std::path::{Path, PathBuf};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm")
}

#[test]
fn file_input_limit_has_an_exact_boundary_and_cache_can_be_disabled() {
    let len = std::fs::metadata(fixture()).unwrap().len() as usize;
    let config = WasmConfig {
        epoch_deadline: 1_000_000,
        ..Default::default()
    };
    let limited = WasmRuntime::new_with_compilation_limits(
        config.clone(),
        CompilationLimits {
            max_input_bytes: len - 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        limited.instantiate(&fixture(), HostCtx::default()),
        Err(ComponentError::ResourceLimit(_))
    ));
    let exact = WasmRuntime::new_with_compilation_limits(
        config,
        CompilationLimits {
            max_input_bytes: len,
            max_cache_entries: 0,
            max_cache_bytes: 0,
        },
    )
    .unwrap();
    for _ in 0..2 {
        let mut component = exact.instantiate(&fixture(), HostCtx::default()).unwrap();
        assert_eq!(
            component.handle("echo", b"exact limit").unwrap(),
            b"exact limit"
        );
    }
}

#[test]
fn invalid_compilation_limits_are_configuration_errors() {
    for max_input_bytes in [0, usize::MAX] {
        assert!(matches!(
            WasmRuntime::new_with_compilation_limits(
                WasmConfig::default(),
                CompilationLimits {
                    max_input_bytes,
                    ..Default::default()
                }
            ),
            Err(ComponentError::InvalidConfig(_))
        ));
    }
}

#[cfg(unix)]
#[test]
fn fifo_artifact_is_rejected_without_waiting_for_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    struct OwnedDir(PathBuf);
    impl Drop for OwnedDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let path = std::env::temp_dir().join(format!(
        "parrot-wasm-fifo-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&path).unwrap();
    let owned = OwnedDir(path);
    let fifo = owned.0.join("component.wasm");
    let cpath = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: cpath is NUL terminated and remains alive throughout this call.
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender
            .send(runtime.instantiate(&fifo, HostCtx::default()).map(|_| ()))
            .unwrap();
    });
    let result = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("FIFO open blocked");
    worker.join().unwrap();
    assert!(
        matches!(result, Err(ComponentError::AbiVersion(message)) if message.contains("regular file"))
    );
}
