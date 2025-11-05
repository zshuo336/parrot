//! Owned, unreaped session leaders pin the identity used for group retirement.
//! Callers never receive the Child, so they cannot accidentally reap it before
//! the last signal. Signals target the owned group; retirement also observes
//! other groups in the same session. Deliberately escaping the session is not contained.
mod liveness;
pub use liveness::has_live_group;
use std::os::unix::process::CommandExt;
use std::{
    io,
    process::{Child, Command},
    time::{Duration, Instant},
};

pub struct PinnedProcess {
    child: Option<Child>,
    result: Option<Result<(), String>>,
}
impl PinnedProcess {
    pub fn spawn(mut command: Command) -> io::Result<Self> {
        // setsid is async-signal-safe; the child becomes its own session and
        // group leader before executing the trusted guardian.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().map(|child| Self {
            child: Some(child),
            result: None,
        })
    }
    /// A result is retained for repeated waiters. Zero budgets do not transfer
    /// ownership or prevent a subsequent real retirement attempt.
    pub fn retire(&mut self, budget: Duration) -> Result<(), String> {
        if let Some(result) = &self.result {
            return result.clone();
        }
        if budget.is_zero() {
            return Err("group retirement budget must be positive".into());
        }
        let child = self.child.as_mut().expect("unretired group has an owner");
        let deadline = Instant::now()
            .checked_add(budget)
            .ok_or("group retirement deadline overflow")?;
        let mut errors = Vec::new();
        let mut empty = false;
        loop {
            // No competing child reaper or SIGCHLD=SIG_IGN is supported.
            // Detect ownership already lost before attempting any group signal.
            // ECHILD is a loss of ownership, never permission to use a stale ID.
            if let Err(error) = still_owned(child.id()) {
                let result = Err(format!("group identity no longer owned: {error}"));
                self.child.take();
                self.result = Some(result.clone());
                return result;
            }
            if unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    errors.push(format!("kill gateway group: {error}"));
                    break;
                }
            }
            match liveness::has_live_session(child.id(), deadline) {
                Ok(false) if empty => break,
                Ok(live) => empty = !live,
                Err(error) => {
                    errors.push(error);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Err(error) = child.wait() {
            errors.push(format!("wait gateway guardian: {error}"));
        }
        self.child.take();
        let result = if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        };
        self.result = Some(result.clone());
        result
    }
}
fn still_owned(pid: u32) -> io::Result<()> {
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        if unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        } == 0
        {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
impl Drop for PinnedProcess {
    fn drop(&mut self) {
        if self.child.is_some() {
            if let Err(error) = self.retire(Duration::from_secs(10)) {
                tracing::error!(%error, "owned process group cleanup failed");
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Stdio;
    fn command(script: &str) -> Command {
        let mut cmd = Command::new("python3");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }
    #[test]
    fn invalid_budget_retains_owner_and_repeated_retirement_is_stable() {
        let mut process = PinnedProcess::spawn(command("import time;time.sleep(120)")).unwrap();
        let group = process.child.as_ref().unwrap().id();
        assert!(process.retire(Duration::ZERO).is_err());
        assert!(has_live_group(group, Instant::now() + Duration::from_secs(3)).unwrap());
        assert_eq!(process.retire(Duration::from_secs(5)), Ok(()));
        assert_eq!(process.retire(Duration::ZERO), Ok(()));
        assert!(!has_live_group(group, Instant::now() + Duration::from_secs(3)).unwrap());
    }
    #[test]
    fn dead_unreaped_leader_keeps_descendants_owned() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("ready");
        let mut cmd = command("import subprocess,sys;from pathlib import Path;p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(120)']);Path(sys.argv[1]).write_text(str(p.pid))");
        cmd.arg(&record);
        let mut process = PinnedProcess::spawn(cmd).unwrap();
        let group = process.child.as_ref().unwrap().id();
        let until = Instant::now() + Duration::from_secs(5);
        while !record.exists() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        loop {
            let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            assert_eq!(
                unsafe {
                    libc::waitid(
                        libc::P_PID,
                        group,
                        info.as_mut_ptr(),
                        libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                    )
                },
                0
            );
            if unsafe { info.assume_init().si_pid() } != 0 {
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(has_live_group(group, until).unwrap());
        assert_eq!(process.retire(Duration::from_secs(5)), Ok(()));
        assert!(!has_live_group(group, Instant::now() + Duration::from_secs(3)).unwrap());
    }
    #[test]
    fn lost_child_ownership_returns_stable_error_and_preserves_other_group() {
        let mut sentinel = PinnedProcess::spawn(command("import time;time.sleep(120)")).unwrap();
        let other = sentinel.child.as_ref().unwrap().id();
        let mut process = PinnedProcess::spawn(command("pass")).unwrap();
        // Model an unsupported external reaper. The public API never exposes Child.
        process.child.as_mut().unwrap().wait().unwrap();
        let result = process.retire(Duration::from_secs(5));
        assert!(result
            .as_ref()
            .unwrap_err()
            .contains("identity no longer owned"));
        assert_eq!(process.retire(Duration::from_secs(5)), result);
        assert!(has_live_group(other, Instant::now() + Duration::from_secs(3)).unwrap());
        sentinel.retire(Duration::from_secs(5)).unwrap();
    }
    #[test]
    fn retirement_waits_for_worker_in_a_separate_group_within_session() {
        let directory = tempfile::tempdir().unwrap();
        let record = directory.path().join("worker.json");
        let script = r#"import os,sys,subprocess,time
worker = r'''import os,sys,socket,time,json
from pathlib import Path
os.setpgrp()
s=socket.socket();s.bind(('127.0.0.1',0));s.listen()
parent=os.getppid()
record=Path(sys.argv[1]);temporary=record.with_suffix('.tmp');temporary.write_text(json.dumps({'pid':os.getpid(),'port':s.getsockname()[1]}));temporary.replace(record)
while os.getppid()==parent:time.sleep(.01)
time.sleep(.3)
'''
subprocess.Popen([sys.executable,'-c',worker,sys.argv[1]])
while True:time.sleep(.1)
"#;
        let mut cmd = command(script);
        cmd.arg(&record);
        let mut process = PinnedProcess::spawn(cmd).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !record.exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
        let port = value["port"].as_u64().unwrap() as u16;
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err());
        assert_eq!(process.retire(Duration::from_secs(5)), Ok(()));
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "retirement returned while worker still listened"
        );
    }
    #[test]
    fn drop_retires_owned_group() {
        let process = PinnedProcess::spawn(command("import time;time.sleep(120)")).unwrap();
        let group = process.child.as_ref().unwrap().id();
        drop(process);
        assert!(!has_live_group(group, Instant::now() + Duration::from_secs(3)).unwrap());
    }
}
