use parrot_app::{limits::AdmissionError, manifest::AppLoadError, AppManifest, ApplicationLimits};
use std::io::ErrorKind;

#[cfg(unix)]
#[path = "support/native_path.rs"]
mod native_path;

const MANIFEST: &str = "name = 'input-contract'\nversion = '1.0.0'\ncomponents = []\n";

fn io_error(result: Result<AppManifest, AppLoadError>, expected: ErrorKind) {
    match result {
        Err(AppLoadError::Io(error)) => assert_eq!(error.kind(), expected),
        other => panic!("expected {expected:?} input error, received {other:?}"),
    }
}

#[test]
fn source_budget_is_inclusive_and_precedes_utf8_and_toml() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("budget.app.toml");
    // Independent default contract: the application source budget is 4 MiB.
    for limit in [4096, 4 * 1024 * 1024] {
        let mut bytes = MANIFEST.as_bytes().to_vec();
        bytes.push(b'#');
        bytes.resize(limit, b'x');
        std::fs::write(&path, &bytes).unwrap();
        let limits = ApplicationLimits {
            max_source_bytes: limit as u64,
            ..Default::default()
        };
        let loaded = AppManifest::from_file_with_limits(&path, &limits).unwrap();
        assert_eq!(loaded, AppManifest::from_toml(MANIFEST).unwrap());
        if limit == 4 * 1024 * 1024 {
            assert_eq!(AppManifest::from_file(&path).unwrap(), loaded);
        }
        // Over-budget input must report admission, even with invalid UTF-8.
        bytes.push(0xff);
        std::fs::write(&path, bytes).unwrap();
        match AppManifest::from_file_with_limits(&path, &limits).unwrap_err() {
            AppLoadError::Admission(AdmissionError::Exceeded {
                resource: "source bytes",
                requested,
                limit: actual_limit,
            }) => {
                assert_eq!(actual_limit, limit as u64);
                assert_eq!(requested, actual_limit + 1);
            }
            other => panic!("expected source admission error, received {other:?}"),
        }
        if limit == 4 * 1024 * 1024 {
            assert!(matches!(
                AppManifest::from_file(&path),
                Err(AppLoadError::Admission(_))
            ));
        }
    }
}

#[test]
fn ordinary_file_errors_keep_their_io_or_parse_domains() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.app.toml");
    io_error(AppManifest::from_file(&path), ErrorKind::NotFound);
    std::fs::write(&path, [0xff]).unwrap();
    io_error(AppManifest::from_file(&path), ErrorKind::InvalidData);
    std::fs::write(&path, "name = [").unwrap();
    assert!(matches!(
        AppManifest::from_file(&path),
        Err(AppLoadError::Parse(_))
    ));
    std::fs::write(&path, MANIFEST).unwrap();
    assert_eq!(
        AppManifest::from_file(&path).unwrap(),
        AppManifest::from_toml(MANIFEST).unwrap()
    );
}

#[test]
fn directory_is_an_io_error_in_both_file_loaders() {
    let directory = tempfile::tempdir().unwrap();
    assert!(matches!(
        AppManifest::from_file(directory.path()),
        Err(AppLoadError::Io(_))
    ));
    assert!(matches!(
        AppManifest::from_file_with_limits(directory.path(), &ApplicationLimits::default()),
        Err(AppLoadError::Io(_))
    ));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        os::unix::{ffi::OsStrExt, fs::symlink},
        path::Path,
        process::{Child, Command, ExitStatus, Stdio},
        time::{Duration, Instant},
    };

    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn wait(child: &mut OwnedChild) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "manifest input child blocked");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn fifo(path: &Path) {
        let encoded = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
    }

    fn fifo_contract(test: &str, custom: bool) {
        if let Some(path) = std::env::var_os("PARROT_MANIFEST_INPUT_CHILD") {
            let loaded = if custom {
                AppManifest::from_file_with_limits(Path::new(&path), &ApplicationLimits::default())
            } else {
                AppManifest::from_file(Path::new(&path))
            };
            io_error(loaded, ErrorKind::InvalidInput);
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("no-writer.fifo");
        fifo(&path);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env("PARROT_MANIFEST_INPUT_CHILD", path)
            .stdout(Stdio::null());
        let mut child = OwnedChild(command.spawn().unwrap());
        assert!(wait(&mut child).success(), "manifest input child failed");
    }

    #[test]
    fn default_loader_rejects_fifo_without_waiting_for_a_writer() {
        fifo_contract(
            "unix::default_loader_rejects_fifo_without_waiting_for_a_writer",
            false,
        );
    }

    #[test]
    fn custom_loader_rejects_fifo_without_waiting_for_a_writer() {
        fifo_contract(
            "unix::custom_loader_rejects_fifo_without_waiting_for_a_writer",
            true,
        );
    }

    #[test]
    fn device_is_rejected_as_input_instead_of_a_toml_error() {
        io_error(
            AppManifest::from_file(Path::new("/dev/null")),
            ErrorKind::InvalidInput,
        );
        io_error(
            AppManifest::from_file_with_limits(
                Path::new("/dev/null"),
                &ApplicationLimits::default(),
            ),
            ErrorKind::InvalidInput,
        );
    }

    #[test]
    fn symlink_to_regular_file_preserves_content_and_custom_budget() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("version-1.app.toml");
        let path = directory.path().join("current.app.toml");
        std::fs::write(&target, MANIFEST).unwrap();
        symlink(&target, &path).unwrap();
        let expected = AppManifest::from_toml(MANIFEST).unwrap();
        assert_eq!(AppManifest::from_file(&path).unwrap(), expected);
        let mut limits = ApplicationLimits {
            max_source_bytes: MANIFEST.len() as u64,
            ..Default::default()
        };
        assert_eq!(
            AppManifest::from_file_with_limits(&path, &limits).unwrap(),
            expected
        );
        limits.max_source_bytes -= 1;
        assert!(matches!(
            AppManifest::from_file_with_limits(&path, &limits),
            Err(AppLoadError::Admission(_))
        ));
    }

    #[test]
    fn ordinary_native_paths_preserve_file_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(native_path::file_name());
        std::fs::write(&path, MANIFEST).unwrap();
        assert_eq!(
            AppManifest::from_file(&path).unwrap(),
            AppManifest::from_toml(MANIFEST).unwrap()
        );
        assert_eq!(
            AppManifest::from_file_with_limits(&path, &ApplicationLimits::default()).unwrap(),
            AppManifest::from_toml(MANIFEST).unwrap()
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn application_cli_rejects_special_manifest_before_recording_or_readiness() {
        let directory = tempfile::tempdir().unwrap();
        let pipe = directory.path().join("manifest.fifo");
        fifo(&pipe);
        for (index, path) in [pipe.as_path(), Path::new("/dev/null"), directory.path()]
            .into_iter()
            .enumerate()
        {
            let stdout = directory.path().join(format!("{index}.stdout"));
            let stderr = directory.path().join(format!("{index}.stderr"));
            let recording = directory.path().join(format!("{index}.recording"));
            let mut command = Command::new(env!("CARGO_BIN_EXE_parrot-app"));
            command
                .arg("run")
                .arg("--manifest")
                .arg(path)
                .arg("--record")
                .arg(&recording)
                .arg("--bind")
                .arg("127.0.0.1:0")
                .current_dir(directory.path())
                .stdout(std::fs::File::create(&stdout).unwrap())
                .stderr(std::fs::File::create(&stderr).unwrap());
            let mut child = OwnedChild(command.spawn().unwrap());
            assert_eq!(wait(&mut child).code(), Some(1));
            assert!(std::fs::read(&stdout).unwrap().is_empty());
            let error = std::fs::read_to_string(&stderr).unwrap();
            assert!(
                error.contains("io: manifest source must be a regular file"),
                "{error}"
            );
            assert!(!recording.exists(), "invalid manifest created a recording");
        }
    }
}
