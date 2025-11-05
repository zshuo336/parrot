use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Own the manager immediately. EOF requests graceful retirement; killing the
/// manager still leaves its independent guardians responsible for their groups.
pub struct Gateway {
    child: Child,
    pub port: u16,
}
impl Gateway {
    pub async fn start(kind: &'static str, relay: Option<&'static str>) -> Self {
        tokio::task::spawn_blocking(move || {
            let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../remote-poc/owned_gateway.py");
            let mut command = Command::new("python3");
            command.arg(script).arg(kind);
            if let Some(relay) = relay {
                command.arg(relay);
            }
            let child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("gateway manager starts");
            let mut owner = Self { child, port: 0 };
            let stdout = owner.child.stdout.take().unwrap();
            let (tx, rx) = mpsc::sync_channel(1);
            let reader = thread::spawn(move || {
                let mut line = String::new();
                let result = BufReader::new(stdout)
                    .take(4096)
                    .read_line(&mut line)
                    .map(|_| line);
                let _ = tx.send(result);
            });
            let result = rx.recv_timeout(Duration::from_secs(100));
            if result.is_err() {
                owner.retire();
                reader.join().unwrap();
                panic!("gateway manager did not report a bound port: {result:?}");
            }
            reader.join().unwrap();
            let line = result.unwrap().expect("gateway address output");
            owner.port = line
                .strip_prefix("PORT ")
                .expect("actual bound-port receipt")
                .trim()
                .parse()
                .expect("nonzero gateway port");
            assert_ne!(owner.port, 0);
            assert!(
                owner.child.try_wait().unwrap().is_none(),
                "gateway manager exited"
            );
            owner
        })
        .await
        .unwrap()
    }
    fn retire(&mut self) -> Option<std::process::ExitStatus> {
        self.child.stdin.take(); // Lifetime EOF, not an unverified stored PID.
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        self.child.wait().ok()
    }
    pub async fn close(mut self) {
        tokio::task::spawn_blocking(move || {
            let status = self.retire().expect("manager reaped");
            assert!(status.success(), "gateway cleanup failed: {status}");
        })
        .await
        .unwrap();
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.retire();
    }
}
