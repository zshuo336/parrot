#![cfg(feature = "loader")]
mod common;
use parrot_abi::{DylibLoader, LoadError, NativeArtifactLimits};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-native-input-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn invalid_budgets_fail_before_opening_the_path() {
    for max_file_bytes in [0, isize::MAX as usize, usize::MAX] {
        let limits = NativeArtifactLimits {
            max_file_bytes,
            ..Default::default()
        };
        let path = std::path::Path::new("/no/such/native-input");
        assert!(matches!(
            DylibLoader::scan_violations_with_limits(path, limits),
            Err(LoadError::InvalidConfig(_))
        ));
        assert!(matches!(
            unsafe { DylibLoader::load_with_limits(path, "", limits) },
            Err(LoadError::InvalidConfig(_))
        ));
    }
}

#[test]
fn default_limit_rejects_sparse_oversize_input_before_parsing_or_loading() {
    let scratch = Scratch::new();
    let path = scratch.0.join("oversized");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(NativeArtifactLimits::default().max_file_bytes as u64 + 1)
        .unwrap();
    assert!(matches!(
        DylibLoader::scan_violations(&path),
        Err(LoadError::ResourceLimit(_))
    ));
    assert!(matches!(
        unsafe { DylibLoader::load(&path, "") },
        Err(LoadError::ResourceLimit(_))
    ));
}

#[test]
fn real_library_is_accepted_at_exact_limit_and_rejected_one_byte_below() {
    let _guard = common::load_mutex();
    let path = common::testcomp();
    let size = std::fs::metadata(&path).unwrap().len() as usize;
    let limits = NativeArtifactLimits {
        max_file_bytes: size,
        ..Default::default()
    };
    assert!(DylibLoader::scan_violations_with_limits(&path, limits)
        .unwrap()
        .is_empty());
    let h = unsafe { DylibLoader::load_with_limits(&path, &common::testcomp_sha256(), limits) }
        .unwrap();
    DylibLoader.construct(&h, "{}").unwrap();
    drop(h);
    let limits = NativeArtifactLimits {
        max_file_bytes: size - 1,
        ..Default::default()
    };
    assert!(matches!(
        DylibLoader::scan_violations_with_limits(&path, limits),
        Err(LoadError::ResourceLimit(_))
    ));
    assert!(matches!(
        unsafe { DylibLoader::load_with_limits(&path, "", limits) },
        Err(LoadError::ResourceLimit(_))
    ));
}

#[test]
fn directory_is_not_an_artifact() {
    let scratch = Scratch::new();
    assert!(matches!(
        DylibLoader::scan_violations(&scratch.0),
        Err(LoadError::Scan(_))
    ));
    assert!(matches!(
        unsafe { DylibLoader::load(&scratch.0, "") },
        Err(LoadError::Open(_))
    ));
}

#[cfg(unix)]
#[test]
fn fifo_and_fifo_symlink_are_rejected_without_needing_a_writer() {
    use std::io::Write;
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    use std::time::{Duration, Instant};
    let scratch = Scratch::new();
    let fifo = scratch.0.join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let symlink = scratch.0.join("symlink");
    std::os::unix::fs::symlink(&fifo, &symlink).unwrap();
    for path in [&fifo, &symlink] {
        for load in [false, true] {
            let (done, wait) = std::sync::mpsc::channel();
            let writer_path = fifo.clone();
            let rescuer = std::thread::spawn(move || {
                if wait.recv_timeout(Duration::from_millis(250)).is_ok() {
                    return false;
                }
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if wait.try_recv().is_ok() {
                        return false;
                    }
                    if let Ok(mut writer) = std::fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&writer_path)
                    {
                        // Only an old blocking reader needs this writer/EOF.
                        let _ = writer.write_all(b"not a native image");
                        return true;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "failed to rescue blocked FIFO reader"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            let result = if load {
                unsafe { DylibLoader::load(path, "") }.map(|_| ())
            } else {
                DylibLoader::scan_violations(path).map(|_| ())
            };
            let _ = done.send(());
            let needed_writer = rescuer.join().unwrap();
            assert!(
                !needed_writer,
                "native artifact API waited for a FIFO writer"
            );
            let message = result.unwrap_err().to_string();
            assert!(message.contains("regular"), "wrong rejection: {message}");
        }
    }
}
