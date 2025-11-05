//! The OS thread owns the guardian until confirmed retirement, even when the
//! calling future or runtime is dropped. A private socket also covers abrupt
//! launcher death. Group ownership does not contain deliberately escaping code.
use parrot_remote::process_group::PinnedProcess;
use std::{
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, net::UnixStream, process::CommandExt},
    },
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use tokio::sync::watch;

type Receipt = watch::Receiver<Option<Result<(), String>>>;
pub struct Gateway {
    stop: Option<mpsc::Sender<()>>,
    ready: watch::Receiver<Option<Result<u16, String>>>,
    finished: Receipt,
}
pub struct GatewaySpec {
    pub command: Vec<std::ffi::OsString>,
    pub node: &'static str,
    pub marker: &'static str,
    pub port: u16,
    pub inline_ray: bool,
    pub startup_output_limit: usize,
}
pub fn source_root() -> Result<PathBuf, String> {
    std::env::var_os("PARROT_SOURCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .canonicalize()
        .map_err(|e| format!("source root: {e}; set PARROT_SOURCE_ROOT"))
}
impl Gateway {
    pub fn spawn(spec: GatewaySpec, startup: Duration) -> Result<Self, String> {
        if startup.is_zero() {
            return Err("startup timeout must be positive".into());
        }
        if spec.command.is_empty() || spec.marker.is_empty() || spec.startup_output_limit == 0 {
            return Err("gateway command, marker and output limit must be nonempty".into());
        }
        let root = source_root()?;
        if !root.join("tools/parrot-obs/gateway_worker.py").is_file() {
            return Err("missing source file: tools/parrot-obs/gateway_worker.py".into());
        }
        let ray = spec.inline_ray;
        let directory = tempfile::Builder::new()
            .prefix("parrot-probe-")
            .tempdir()
            .map_err(|e| e.to_string())?;
        let token = directory.path().join("ray-token");
        if ray {
            // The directory is private (0700); never inherit an ambient cluster credential.
            std::fs::write(
                &token,
                format!(
                    "{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                ),
            )
            .map_err(|e| e.to_string())?;
        }
        // Preserve Unix argument bytes, including explicit JVM classpaths.
        // The Python guardian decodes with os.fsdecode rather than lossy UTF-8.
        let command: Vec<&[u8]> = spec
            .command
            .iter()
            .map(|arg| arg.as_os_str().as_bytes())
            .collect();
        let config = serde_json::to_vec(&serde_json::json!({
            "command_os_bytes": command, "cwd": root, "python_entrypoint": ray,
            "env": {"PYTHONPATH": root.join("interop/python"), "PARROT_NODE_ID": spec.node,
                "RAY_ADDRESS": "local", "RAY_USAGE_STATS_ENABLED": "0", "RAY_AUTH_MODE": "token", "RAY_AUTH_TOKEN_PATH": token},
            "log": directory.path().join("gateway.log"),
            "marker": spec.marker, "port": spec.port,
            "startup_output_limit": spec.startup_output_limit
        })).map_err(|e| e.to_string())?;
        if config.len() >= 262144 {
            return Err("gateway configuration exceeds limit".into());
        }
        let (stop, stopped) = mpsc::channel();
        let (ready, readiness) = watch::channel(None);
        let (finished, completion) = watch::channel(None);
        std::thread::Builder::new()
            .name("parrot-probe-owner".into())
            .spawn(move || {
                let result = own(root, directory, config, stopped, &ready, startup);
                if readiness_unset(&ready) {
                    ready.send_replace(Some(Err(result
                        .as_ref()
                        .err()
                        .cloned()
                        .unwrap_or_else(|| "gateway stopped before readiness".into()))));
                }
                finished.send_replace(Some(result));
            })
            .map_err(|e| format!("gateway owner thread: {e}"))?;
        Ok(Self {
            stop: Some(stop),
            ready: readiness,
            finished: completion,
        })
    }
    pub async fn ready(&mut self) -> Result<u16, String> {
        loop {
            if let Some(result) = self.ready.borrow().clone() {
                return result;
            }
            self.ready
                .changed()
                .await
                .map_err(|_| "gateway readiness owner ended".to_string())?;
        }
    }
    pub async fn ended(&mut self) -> Result<(), String> {
        wait(&mut self.finished).await
    }
    pub async fn retire(&mut self) -> Result<(), String> {
        self.stop.take();
        wait(&mut self.finished).await
    }
}
async fn wait(receipt: &mut Receipt) -> Result<(), String> {
    loop {
        if let Some(result) = receipt.borrow().clone() {
            return result;
        }
        receipt
            .changed()
            .await
            .map_err(|_| "gateway cleanup owner ended without receipt".to_string())?;
    }
}
fn readiness_unset(ready: &watch::Sender<Option<Result<u16, String>>>) -> bool {
    ready.borrow().is_none()
}
fn own(
    root: PathBuf,
    directory: tempfile::TempDir,
    config: Vec<u8>,
    stop: mpsc::Receiver<()>,
    ready: &watch::Sender<Option<Result<u16, String>>>,
    startup: Duration,
) -> Result<(), String> {
    let (mut channel, inherited) = UnixStream::pair().map_err(|e| e.to_string())?;
    let fd = inherited.as_raw_fd();
    let mut command = Command::new("python3");
    command
        .arg(root.join("tools/parrot-obs/gateway_worker.py"))
        .arg(fd.to_string())
        .env_remove("RAY_AUTH_TOKEN")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = PinnedProcess::spawn(command).map_err(|e| format!("spawn guardian: {e}"))?;
    drop(inherited);
    // Keep bounded gateway logs for diagnosis, but remove the private credential
    // only after group retirement. Ray's own runtime logs have separate retention.
    let logs = directory.keep();
    eprintln!("gateway logs: {}", logs.display());
    let monitor = monitor(&mut channel, &config, &stop, ready, startup);
    if let Err(error) = &monitor {
        if readiness_unset(ready) {
            ready.send_replace(Some(Err(error.clone())));
        }
    }
    let _ = channel.write_all(b"stop\n");
    std::thread::sleep(Duration::from_millis(2200));
    let cleanup = child.retire(Duration::from_secs(10));
    drop(channel);
    if cleanup.is_ok() {
        match std::fs::remove_file(logs.join("ray-token")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{monitor:?}; remove private credential: {e}")),
        }
    }
    match (monitor, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (result, Err(error)) => Err(format!(
            "gateway result: {result:?}; group retirement: {error}"
        )),
    }
}
fn monitor(
    channel: &mut UnixStream,
    config: &[u8],
    stop: &mpsc::Receiver<()>,
    ready: &watch::Sender<Option<Result<u16, String>>>,
    startup: Duration,
) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(startup)
        .ok_or("startup deadline overflow")?;
    channel
        .set_write_timeout(Some(startup.min(Duration::from_secs(5))))
        .map_err(|e| e.to_string())?;
    channel
        .write_all(config)
        .and_then(|_| channel.write_all(b"\n"))
        .map_err(|e| e.to_string())?;
    channel
        .set_read_timeout(Some(Duration::from_millis(20)))
        .map_err(|e| e.to_string())?;
    let mut pending = Vec::new();
    loop {
        if !matches!(stop.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            return Ok(());
        }
        if readiness_unset(ready) && Instant::now() >= deadline {
            return Err("gateway startup deadline exceeded".into());
        }
        let mut buffer = [0; 4096];
        match channel.read(&mut buffer) {
            Ok(0) => return Err("gateway guardian control closed".into()),
            Ok(n) => pending.extend_from_slice(&buffer[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => return Err(format!("gateway control: {e}")),
        }
        if pending.len() > 8192 {
            return Err("gateway event exceeds limit".into());
        }
        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = pending.drain(..=end).collect();
            let event: serde_json::Value =
                serde_json::from_slice(&line).map_err(|e| e.to_string())?;
            match event["event"].as_str() {
                Some("ready") if readiness_unset(ready) => {
                    let port = event["port"]
                        .as_u64()
                        .and_then(|p| u16::try_from(p).ok())
                        .filter(|p| *p != 0)
                        .ok_or("invalid guardian readiness port")?;
                    ready.send_replace(Some(Ok(port)));
                }
                Some("failed" | "exited") => return Err(format!("gateway terminated: {event}")),
                _ => return Err(format!("unexpected guardian event: {event}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_config(directory: &tempfile::TempDir, script: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "command":["python3","-u","-c",script],"cwd":directory.path(),
            "log":directory.path().join("gateway.log"), "marker":"READY=", "port":0,
            "startup_output_limit":1024
        }))
        .unwrap()
    }
    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
    #[test]
    fn cancelled_readiness_retires_and_preserves_repeated_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let config = fixture_config(&directory, "import time;time.sleep(120)");
        let (stop, stopped) = mpsc::channel();
        let (ready, _) = watch::channel(None);
        let (finished, mut receipt) = watch::channel(None);
        let worker = std::thread::spawn(move || {
            finished.send_replace(Some(own(
                root(),
                directory,
                config,
                stopped,
                &ready,
                Duration::from_secs(5),
            )));
        });
        drop(stop);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(8), wait(&mut receipt))
                    .await
                    .unwrap(),
                Ok(())
            );
            assert_eq!(wait(&mut receipt).await, Ok(()));
        });
        worker.join().unwrap();
    }
    #[test]
    fn startup_deadline_is_a_failure_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let config = fixture_config(&directory, "import time;time.sleep(120)");
        let (_stop, stopped) = mpsc::channel();
        let (ready, receipt) = watch::channel(None);
        let result = own(
            root(),
            directory,
            config,
            stopped,
            &ready,
            Duration::from_millis(150),
        );
        assert!(result.unwrap_err().contains("startup deadline"));
        assert!(receipt
            .borrow()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .contains("startup deadline"));
    }
    #[test]
    fn excess_startup_output_is_rejected_before_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let config = fixture_config(
            &directory,
            "import time;print('x'*3000,flush=True);time.sleep(120)",
        );
        let (_stop, stopped) = mpsc::channel();
        let (ready, receipt) = watch::channel(None);
        let result = own(
            root(),
            directory,
            config,
            stopped,
            &ready,
            Duration::from_secs(5),
        );
        assert!(result.unwrap_err().contains("startup output exceeds limit"));
        assert!(receipt.borrow().as_ref().unwrap().is_err());
    }
    #[test]
    fn readiness_marker_precedes_post_ready_output_budget() {
        let directory = tempfile::tempdir().unwrap();
        let config = fixture_config(
            &directory,
            "import os,time;os.write(1,b'READY=12345\\n'+b'x'*2000);time.sleep(120)",
        );
        let (stop, stopped) = mpsc::channel();
        let (ready, mut receipt) = watch::channel(None);
        let worker = std::thread::spawn(move || {
            own(
                root(),
                directory,
                config,
                stopped,
                &ready,
                Duration::from_secs(5),
            )
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let observed = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    if let Some(result) = receipt.borrow().clone() {
                        break result;
                    }
                    receipt.changed().await.unwrap();
                }
            })
            .await
            .unwrap()
        });
        drop(stop);
        let retired = worker.join().unwrap();
        assert_eq!(
            observed,
            Ok(12345),
            "post-readiness bytes consumed the startup budget"
        );
        assert_eq!(retired, Ok(()));
    }
}
