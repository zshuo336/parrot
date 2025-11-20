use parrot_config::{ConfigError, ParrotConfig};
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

// Independent contract value: both source and expanded UTF-8 are limited to 1 MiB.
const LIMIT: usize = 1024 * 1024;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-config-input-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rejected(result: Result<ParrotConfig, ConfigError>, expected: ErrorKind) {
    match result {
        Err(ConfigError::Io { source, .. }) => assert_eq!(source.kind(), expected),
        other => panic!("expected {expected:?} input rejection, received {other:?}"),
    }
}

#[test]
fn raw_source_budget_includes_exact_limit_and_rejects_one_more_byte() {
    let directory = Directory::new();
    let mut text = vec![b' '; LIMIT];
    text[0] = b'#';
    let path = directory.file("configuration.toml", &text);
    for optional in [false, true] {
        let builder = ParrotConfig::builder();
        let loaded = if optional {
            builder.load_optional_file(&path)
        } else {
            builder.load_file(&path)
        };
        loaded.unwrap().build().unwrap();
    }
    text.push(b' ');
    std::fs::write(&path, text).unwrap();
    rejected(
        ParrotConfig::builder().load_file(&path),
        ErrorKind::InvalidData,
    );
    rejected(
        ParrotConfig::builder().load_optional_file(&path),
        ErrorKind::InvalidData,
    );
}

#[test]
fn invalid_utf8_is_an_input_error_even_when_optional() {
    let directory = Directory::new();
    let path = directory.file("invalid.toml", &[0xff]);
    rejected(
        ParrotConfig::builder().load_file(&path),
        ErrorKind::InvalidData,
    );
    rejected(
        ParrotConfig::builder().load_optional_file(&path),
        ErrorKind::InvalidData,
    );
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child(test: &str, path: &Path, setup: impl FnOnce(&mut Command)) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture"])
        .env("PARROT_INPUT_RESOURCE_CHILD", path)
        .stdout(Stdio::null());
    setup(&mut command);
    let mut process = OwnedChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success(), "input child failed: {test}: {status}");
            break;
        }
        assert!(Instant::now() < deadline, "input child blocked: {test}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn repeated_environment_values_cannot_amplify_a_small_source_past_the_budget() {
    if let Some(path) = std::env::var_os("PARROT_INPUT_RESOURCE_CHILD") {
        // The first load expands to exactly LIMIT bytes of valid comment text.
        ParrotConfig::builder()
            .load_file(&path)
            .unwrap()
            .build()
            .unwrap();
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(b"${PARROT_INPUT_TEST_VALUE}");
        std::fs::write(&path, raw).unwrap();
        rejected(
            ParrotConfig::builder().load_file(&path),
            ErrorKind::InvalidData,
        );
        rejected(
            ParrotConfig::builder().load_optional_file(&path),
            ErrorKind::InvalidData,
        );
        return;
    }
    let directory = Directory::new();
    let token = "${PARROT_INPUT_TEST_VALUE}";
    let value_bytes = (LIMIT - 2) / 64;
    let raw = format!(
        "#{}{}",
        token.repeat(64),
        " ".repeat(LIMIT - 1 - value_bytes * 64)
    );
    let path = directory.file("expanded.toml", raw.as_bytes());
    child(
        "repeated_environment_values_cannot_amplify_a_small_source_past_the_budget",
        &path,
        |command| {
            command.env("PARROT_INPUT_TEST_VALUE", "x".repeat(value_bytes));
        },
    );
}

#[cfg(unix)]
#[test]
fn fifo_without_a_writer_is_rejected_without_waiting_for_data() {
    if let Some(path) = std::env::var_os("PARROT_INPUT_RESOURCE_CHILD") {
        rejected(
            ParrotConfig::builder().load_file(&path),
            ErrorKind::InvalidInput,
        );
        rejected(
            ParrotConfig::builder().load_optional_file(&path),
            ErrorKind::InvalidInput,
        );
        return;
    }
    use std::os::unix::ffi::OsStrExt;
    let directory = Directory::new();
    let path = directory.0.join("source.fifo");
    let encoded = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
    child(
        "fifo_without_a_writer_is_rejected_without_waiting_for_data",
        &path,
        |_| {},
    );
}

#[cfg(unix)]
#[test]
fn symlinks_to_regular_configuration_files_remain_supported() {
    let directory = Directory::new();
    let target = directory.file("target.toml", b"[thread]\nshared_pool_size=3");
    let path = directory.0.join("mounted.toml");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let resolved = ParrotConfig::builder()
        .load_file(&path)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(resolved.thread.shared_pool_size, 3);
}

#[cfg(unix)]
#[test]
fn device_and_directory_inputs_are_rejected_before_reading() {
    rejected(
        ParrotConfig::builder().load_file("/dev/null"),
        ErrorKind::InvalidInput,
    );
    rejected(
        ParrotConfig::builder().load_optional_file("/dev/null"),
        ErrorKind::InvalidInput,
    );
    let directory = Directory::new();
    assert!(ParrotConfig::builder().load_file(&directory.0).is_err());
}
