//! Process-group ownership independent of the caller's future and Tokio runtime.
//!
//! The guardian pins the group identity until the owner's last signal. Closing
//! the private socket also retires the group if this host is killed abruptly.
use super::gateway_artifacts::PrivateArtifacts;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::gateway_artifacts::RetirementArtifacts;
use super::DeployError;
use std::sync::{Arc, Mutex};

pub(super) type Completion = tokio::sync::watch::Receiver<Option<Result<(), String>>>;

pub(super) async fn wait_for_retirement(completion: &mut Completion) -> Result<(), DeployError> {
    loop {
        if let Some(result) = completion.borrow().clone() {
            return result.map_err(DeployError::Engine);
        }
        completion.changed().await.map_err(|_| {
            DeployError::Engine("gateway cleanup owner ended without a result".into())
        })?;
    }
}

pub(super) struct OwnedGateway {
    stop: Option<std::sync::mpsc::Sender<()>>,
    finished: Completion,
    failure: Arc<Mutex<Option<String>>>,
    exit_code: Arc<Mutex<Option<i64>>>,
}

impl OwnedGateway {
    pub async fn wait_command(&mut self) -> Result<(), DeployError> {
        wait_for_retirement(&mut self.finished).await?;
        if *self.exit_code.lock().unwrap() == Some(0) {
            return Ok(());
        }
        Err(DeployError::Engine(
            self.failure
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "gateway command ended without success".into()),
        ))
    }
    pub fn completion(&self) -> Completion {
        self.finished.clone()
    }
    pub fn check_running(&self) -> Result<(), DeployError> {
        if let Some(error) = self.failure.lock().unwrap().as_ref() {
            return Err(DeployError::Engine(error.clone()));
        }
        if self.finished.borrow().is_some() {
            return Err(DeployError::Engine("gateway owner has retired".into()));
        }
        Ok(())
    }

    pub async fn retire(&mut self) -> Result<(), DeployError> {
        self.stop.take();
        wait_for_retirement(&mut self.finished).await
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn spawn_admitted(
        _command: &tokio::process::Command,
        _root: &std::path::Path,
        _inline: bool,
        _artifacts: Option<Arc<PrivateArtifacts>>,
        _reservation: Option<Arc<super::gateway_admission::Reservation>>,
    ) -> Result<Self, DeployError> {
        Err(DeployError::Engine(
            "local gateway group ownership verification requires Linux or macOS".into(),
        ))
    }

    #[cfg(all(test, unix))]
    pub fn spawn(
        command: &tokio::process::Command,
        root: &std::path::Path,
        inline: bool,
    ) -> Result<Self, DeployError> {
        Self::spawn_with_artifacts(command, root, inline, None)
    }

    #[cfg(all(test, unix))]
    pub fn spawn_with_artifacts(
        command: &tokio::process::Command,
        root: &std::path::Path,
        inline: bool,
        artifacts: Option<Arc<PrivateArtifacts>>,
    ) -> Result<Self, DeployError> {
        Self::spawn_admitted(command, root, inline, artifacts, None)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn spawn_admitted(
        command: &tokio::process::Command,
        root: &std::path::Path,
        inline: bool,
        artifacts: Option<Arc<PrivateArtifacts>>,
        reservation: Option<Arc<super::gateway_admission::Reservation>>,
    ) -> Result<Self, DeployError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::CommandExt;
        let io_error = |e| DeployError::Engine(format!("gateway owner: {e}"));
        let utf8 = |s: &std::ffi::OsStr| {
            s.to_str()
                .map(str::to_owned)
                .ok_or_else(|| DeployError::Config("gateway command must be valid UTF-8".into()))
        };
        let command = command.as_std();
        let args = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(utf8)
            .collect::<Result<Vec<_>, _>>()?;
        let mut environment = serde_json::Map::new();
        for (key, value) in command.get_envs() {
            let value = value.ok_or_else(|| {
                DeployError::Config("gateway owner does not accept env_remove".into())
            })?;
            environment.insert(utf8(key)?, serde_json::Value::String(utf8(value)?));
        }
        let cwd = match command.get_current_dir() {
            Some(path) => path.to_path_buf(),
            None => std::env::current_dir().map_err(io_error)?,
        };
        let directory = tempfile::Builder::new()
            .prefix("parrot-host-")
            .tempdir()
            .map_err(io_error)?;
        let config = serde_json::to_vec(&serde_json::json!({
            "command": args, "cwd": utf8(cwd.as_os_str())?, "env": environment,
            "python_entrypoint": inline,
            "log": utf8(directory.path().join("gateway.log").as_os_str())?,
        }))
        .map_err(|e| DeployError::Config(e.to_string()))?;
        if config.len() >= 262144 {
            return Err(DeployError::Config(
                "gateway configuration exceeds limit".into(),
            ));
        }
        let (channel, inherited) = UnixStream::pair().map_err(io_error)?;
        let fd = inherited.as_raw_fd();
        let mut guardian = std::process::Command::new("python3");
        guardian
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .arg(root.join("tools/parrot-obs/gateway_worker.py"))
            .arg(fd.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit());
        // Only async-signal-safe syscalls between fork and exec. The inherited
        // endpoint is private; the host's endpoint remains close-on-exec.
        unsafe {
            guardian.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child =
            parrot_remote::process_group::PinnedProcess::spawn(guardian).map_err(io_error)?;
        if let Some(reservation) = &reservation {
            reservation.started();
        }
        let artifacts = artifacts.map(|directory| Arc::new(RetirementArtifacts::new(directory)));
        drop(inherited);
        let logs = directory.keep();
        tracing::info!(path = %logs.display(), "gateway logs");
        let (stop, receiver) = std::sync::mpsc::channel();
        let (finished, completion) = tokio::sync::watch::channel(None);
        let failure = Arc::new(Mutex::new(None));
        let exit_code = Arc::new(Mutex::new(None));
        // Retain recoverable ownership if the OS refuses to create the thread.
        let resources = Arc::new(Mutex::new(Some(child)));
        let worker_resources = resources.clone();
        let worker_failure = failure.clone();
        let worker_exit = exit_code.clone();
        // Keep a second reference through thread-spawn failure and final reap.
        let _spawn_artifacts = artifacts.clone();
        let spawn_reservation = reservation.clone();
        let spawned = std::thread::Builder::new()
            .name("parrot-gateway-owner".into())
            .spawn(move || {
                let mut child = worker_resources.lock().unwrap().take().unwrap();
                let result = supervise(
                    channel,
                    receiver,
                    &worker_failure,
                    &worker_exit,
                    &mut child,
                    &config,
                );
                if let Err(error) = &result {
                    tracing::error!(%error, "gateway retirement failed");
                }
                if result.is_ok() {
                    if let Some(artifacts) = &artifacts {
                        artifacts.retired();
                    }
                    if let Some(reservation) = &reservation {
                        reservation.retired();
                    }
                }
                drop(artifacts);
                drop(reservation);
                finished.send_replace(Some(result));
            });
        if let Err(error) = spawned {
            let mut child = resources.lock().unwrap().take().unwrap();
            let cleanup = final_reap(&mut child);
            if cleanup.is_ok() {
                if let Some(artifacts) = &_spawn_artifacts {
                    artifacts.retired();
                }
                if let Some(reservation) = &spawn_reservation {
                    reservation.retired();
                }
            }
            return Err(DeployError::Engine(format!(
                "gateway owner thread: {error}; cleanup: {cleanup:?}"
            )));
        }
        let owned = Self {
            stop: Some(stop),
            finished: completion,
            failure,
            exit_code,
        };
        Ok(owned)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn final_reap(child: &mut parrot_remote::process_group::PinnedProcess) -> Result<(), String> {
    child.retire(std::time::Duration::from_secs(10))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn supervise(
    mut channel: std::os::unix::net::UnixStream,
    stop: std::sync::mpsc::Receiver<()>,
    failure: &Mutex<Option<String>>,
    exit_code: &Mutex<Option<i64>>,
    child: &mut parrot_remote::process_group::PinnedProcess,
    config: &[u8],
) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::time::Duration;
    let monitor = || -> Result<(), String> {
        channel
            .set_write_timeout(Some(Duration::from_secs(5)))
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
            if !matches!(stop.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                return Ok(());
            }
            let mut buffer = [0; 4096];
            match channel.read(&mut buffer) {
                Ok(0) => return Err("gateway guardian control closed".into()),
                Ok(n) => pending.extend_from_slice(&buffer[..n]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue
                }
                Err(error) => return Err(format!("gateway control: {error}")),
            }
            if pending.len() > 8192 {
                return Err("gateway event exceeds limit".into());
            }
            while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = pending.drain(..=end).collect();
                let event: serde_json::Value =
                    serde_json::from_slice(&line).map_err(|e| e.to_string())?;
                if event["event"] == "exited" {
                    *exit_code.lock().unwrap() = event["code"].as_i64();
                }
                if event["event"] == "exited" || event["event"] == "failed" {
                    return Err(format!("gateway terminated: {event}"));
                }
            }
        }
    }();
    if let Err(error) = monitor {
        *failure.lock().unwrap() = Some(error);
    }
    let _ = channel.write_all(b"stop\n");
    std::thread::sleep(Duration::from_millis(2200));
    let result = final_reap(child);
    drop(channel);
    result
}
