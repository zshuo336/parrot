use parrot_app::assemble::DeployError;
use std::{
    future::Future,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    process::{Child, Command},
    time::{Duration, Instant},
};

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn rejects_without_writer<T, F: Future<Output = Result<T, DeployError>>>(
    name: &str,
    load: impl FnOnce(PathBuf) -> F,
) {
    const CHILD: &str = "PARROT_POLICY_FIFO_CHILD";
    if let Some(path) = std::env::var_os(CHILD) {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(load(path.into()));
        assert!(
            matches!(result, Err(DeployError::Config(ref detail)) if detail.contains("expected a regular file")),
            "FIFO must be rejected as a special file"
        );
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("no-writer.fifo");
    let encoded = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
    let mut log = tempfile::tempfile().unwrap();
    let mut child = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, &path)
            .stdout(log.try_clone().unwrap())
            .stderr(log.try_clone().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            use std::io::{Read, Seek};
            log.rewind().unwrap();
            let mut output = String::new();
            log.read_to_string(&mut output).unwrap();
            assert!(status.success(), "FIFO child failed: {status}\n{output}");
            assert!(output.contains("1 passed; 0 failed"), "{output}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "policy FIFO blocked without a writer"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
