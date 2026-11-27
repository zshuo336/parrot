// SPDX-License-Identifier: AGPL-3.0-only
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub async fn run(test: &str, key: &str, directory: &Path) {
    // RLIMIT_FSIZE in the child must apply to its storage files, not its diagnostics.
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(key, directory)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        tokio::try_join!(
            child.wait(),
            out.read_to_end(&mut stdout),
            err.read_to_end(&mut stderr)
        )
    })
    .await;
    if result.is_err() {
        let _ = child.kill().await;
    }
    let stdout = String::from_utf8_lossy(&stdout);
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&stderr));
    let (status, _, _) = result.expect("storage probe timed out").unwrap();
    assert!(status.success(), "{test}: {status}");
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
        "{test}: expected one successful child test"
    );
}

pub fn run_blocking(test: &str, key: &str, directory: &Path) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(test, key, directory));
}
