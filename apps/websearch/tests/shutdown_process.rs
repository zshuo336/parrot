//! Real OS signals while startup waits for absent gateways; no mock success exit.
#![cfg(unix)]
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

async fn signal_during_registration(signal: &str) {
    let directory = std::env::temp_dir().join(format!(
        "parrot-websearch-stop-{}-{}-{}",
        std::process::id(),
        signal,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _directory = Directory(directory.clone());
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_websearch"))
        .args([
            "--insecure-development",
            "--bind",
            "127.0.0.1:0",
            "--wait",
            "180",
            "--port",
            "0",
            "--data",
        ])
        .arg(&directory)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut web_port = None;
    let registry_port = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if let Some(port) = line.strip_prefix("PARROT_WEBSEARCH_PORT=") {
                web_port = Some(port.parse::<u16>().unwrap());
            }
            if let Some(port) = line.strip_prefix("PARROT_WEBSEARCH_REGISTRY_PORT=") {
                return port.parse::<u16>().unwrap();
            }
        }
        panic!("process exited before registration")
    })
    .await
    .unwrap();
    assert!(std::process::Command::new("kill")
        .args(["-s", signal, &pid.to_string()])
        .status()
        .unwrap()
        .success());
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{signal}: {status:?}");
    let mut tail = String::new();
    while let Some(line) = lines.next_line().await.unwrap() {
        tail.push_str(&line);
    }
    assert!(tail.contains("shutdown complete"), "{tail}");
    assert!(
        !directory.join("dedupe.tsv").exists(),
        "stop must precede seed admission"
    );
    for port in [web_port.unwrap(), registry_port] {
        let _listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    }
}

#[tokio::test]
async fn sigterm_retires_startup_without_waiting_for_gateway_deadline() {
    signal_during_registration("TERM").await;
}

#[tokio::test]
async fn sigint_retires_startup_without_waiting_for_gateway_deadline() {
    signal_during_registration("INT").await;
}
