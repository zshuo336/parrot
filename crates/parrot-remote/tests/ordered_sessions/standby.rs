// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use parrot_remote::ordered::handoff;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "receiver process owned by the Linux pidfd standby contract"]
async fn receiver_fixture() {
    let root = PathBuf::from(std::env::var_os("PARROT_STANDBY_TEST").unwrap());
    let (delivery, _rx) = state(Some(root.join("entered")));
    let node = start(config(&root.join("original")), delivery).await;
    let digest = hex::decode(std::env::var("PARROT_STANDBY_DIGEST").unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match node.register_receiver_standby(digest).await {
                Ok(()) => break,
                Err(e) if e.to_string().contains("Connection refused") => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(e) => panic!("registration failed: {e}"),
            }
        }
    })
    .await
    .unwrap();
    std::fs::write(root.join("ready"), node.local_addr().unwrap().to_string()).unwrap();
    std::future::pending::<()>().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn standby_waits_for_kernel_exit_and_retires_without_the_original_directory() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let from = root.join("original");
    let dest = root.join("standby");
    std::fs::create_dir(&from).unwrap();
    std::fs::create_dir(&dest).unwrap();
    let cfg = config(&dest);
    let invite = handoff::prepare(
        cfg.ordered_receiver_state.as_ref().unwrap(),
        "recovery-target",
    )
    .unwrap();
    let mut watcher = Process(
        std::process::Command::new(env!("CARGO_BIN_EXE_parrot-receiver-handoff"))
            .arg("watch")
            .arg(&dest)
            .args(["recovery-contract", "recovery-target"])
            .spawn()
            .unwrap(),
    );
    let mut old = receiver_process(root, &invite);
    let addr = String::from_utf8(wait_file(&root.join("ready")).await)
        .unwrap()
        .parse()
        .unwrap();
    let source = source().await;
    source
        .connect(&NodeAddr::tcp("recovery-target", addr))
        .await
        .unwrap();
    let reference = source
        .remote_ref("parrot://recovery-target/target")
        .unwrap();
    send(&reference, 700).await;
    assert_eq!(wait_file(&root.join("entered")).await, b"entered");
    let links = source.links_snapshot().await;
    assert_eq!(links.len(), 1);
    links[0].1.close();
    assert!(reference
        .close_ordered_session(Duration::from_millis(200))
        .await
        .is_err());
    assert_eq!(
        unsafe { libc::kill(old.0.id() as libc::pid_t, libc::SIGSTOP) },
        0
    );
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(old.0.id() as libc::pid_t, &mut status, libc::WUNTRACED) },
        old.0.id() as i32
    );
    assert!(libc::WIFSTOPPED(status));
    let offline = root.join("unmounted");
    std::fs::rename(&from, &offline).unwrap();
    let (delivery, mut rx) = state(None);
    let err = RemoteActorSystem::new(cfg.clone(), Arc::new(Target(delivery.clone())))
        .err()
        .unwrap();
    assert!(err.to_string().contains("awaits handoff"), "{err}");
    assert!(watcher.0.try_wait().unwrap().is_none());
    assert_eq!(old.kill_and_reap().signal(), Some(libc::SIGKILL));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(exit) = watcher.0.try_wait().unwrap() {
                assert!(exit.success());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pidfd watcher did not activate the standby");
    let replacement = start(cfg, delivery).await;
    connect(&source, &replacement).await;
    reference
        .close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    send(&reference, 71).await;
    seen(&mut rx, 71).await;
    std::fs::rename(offline, &from).unwrap();
    let err = RemoteActorSystem::new(config(&from), Arc::new(Target(state(None).0)))
        .err()
        .unwrap();
    assert!(err.to_string().contains("sealed"), "{err}");
    source.shutdown().await.unwrap();
    replacement.shutdown().await.unwrap();
}

fn receiver_process(root: &Path, invite: &[u8]) -> Process {
    Process(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "standby::receiver_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("PARROT_STANDBY_TEST", root)
            .env("PARROT_STANDBY_DIGEST", hex::encode(Sha256::digest(invite)))
            .spawn()
            .unwrap(),
    )
}

fn durable_process(dir: &Path, invite: &[u8]) -> Process {
    let mut child = Command::new(env!("CARGO_BIN_EXE_parrot-receiver-handoff"))
        .arg("witness")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(invite).unwrap();
    Process(child)
}

async fn output(mut child: Process) -> (bool, Vec<u8>, String) {
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("witness did not finish");
    let mut bytes = Vec::new();
    if let Some(mut stdout) = child.0.stdout.take() {
        stdout.read_to_end(&mut bytes).unwrap();
    }
    let mut detail = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut detail)
        .unwrap();
    (status.success(), bytes, detail)
}

async fn durable_recovery(restart_while_live: bool) {
    let root = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(journal.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("original")).unwrap();
    let cfg = config(dest.path());
    let state_cfg = cfg.ordered_receiver_state.as_ref().unwrap();
    let invite = handoff::prepare(state_cfg, "recovery-target").unwrap();
    let mut watcher = durable_process(journal.path(), &invite);
    let mut old = receiver_process(root.path(), &invite);
    let address = String::from_utf8(wait_file(&root.path().join("ready")).await)
        .unwrap()
        .parse()
        .unwrap();
    let source = source().await;
    source
        .connect(&NodeAddr::tcp("recovery-target", address))
        .await
        .unwrap();
    let reference = source
        .remote_ref("parrot://recovery-target/target")
        .unwrap();
    send(&reference, 700).await;
    assert_eq!(wait_file(&root.path().join("entered")).await, b"entered");
    let (success, bytes, detail) = output(durable_process(journal.path(), &invite)).await;
    assert!(
        !success && bytes.is_empty() && detail.contains("journal lock"),
        "{detail}"
    );
    assert_eq!(watcher.kill_and_reap().signal(), Some(libc::SIGKILL));

    if restart_while_live {
        watcher = durable_process(journal.path(), &invite);
        // Observe the resumed kernel wait before terminating the receiver.
        let fds = format!("/proc/{}/fdinfo", watcher.0.id());
        let pid = format!("Pid:\t{}", old.0.id());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::read_dir(&fds).unwrap().flatten().any(|entry| {
                    std::fs::read_to_string(entry.path()).is_ok_and(|s| s.lines().any(|l| l == pid))
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("replacement witness did not pin the live receiver");
        assert!(RemoteActorSystem::new(cfg.clone(), Arc::new(Target(state(None).0))).is_err());
        // Simulate the remote consumer disconnecting before output is written.
        drop(watcher.0.stdout.take());
    }
    assert_eq!(old.kill_and_reap().signal(), Some(libc::SIGKILL));
    if restart_while_live {
        let (success, _, detail) = output(watcher).await;
        assert!(
            !success && detail.to_lowercase().contains("broken pipe"),
            "{detail}"
        );
    }
    let (ok, proof, digest) = output(durable_process(journal.path(), &invite)).await;
    assert!(ok, "{digest}");
    assert_eq!(hex::encode(Sha256::digest(&proof)), digest.trim());
    handoff::activate(
        state_cfg,
        "recovery-target",
        &proof,
        Sha256::digest(&proof).into(),
    )
    .unwrap();
    let replay = output(durable_process(journal.path(), &invite)).await;
    assert_eq!(replay, (true, proof, digest));
    let other = tempfile::tempdir().unwrap();
    let foreign = handoff::prepare(
        config(other.path())
            .ordered_receiver_state
            .as_ref()
            .unwrap(),
        "recovery-target",
    )
    .unwrap();
    let (ok, bytes, detail) = output(durable_process(journal.path(), &foreign)).await;
    assert!(
        !ok && bytes.is_empty() && detail.contains("invalid standby enrollment"),
        "{detail}"
    );
    let file = journal.path().join("witness.state");
    let mut corrupt = std::fs::read(&file).unwrap();
    corrupt[8] ^= 1;
    std::fs::write(file, corrupt).unwrap();
    let (ok, bytes, detail) = output(durable_process(journal.path(), &invite)).await;
    assert!(
        !ok && bytes.is_empty() && detail.contains("corrupt witness journal"),
        "{detail}"
    );
    let (delivery, mut rx) = state(None);
    let target = start(cfg, delivery).await;
    connect(&source, &target).await;
    reference
        .close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    send(&reference, 81).await;
    seen(&mut rx, 81).await;
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn witness_restart_waits_for_live_receiver_and_replays_after_broken_output() {
    durable_recovery(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn witness_recovers_when_receiver_exited_during_observation_outage() {
    durable_recovery(false).await;
}
