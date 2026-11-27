// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use parrot_remote::ordered::handoff;
use sha2::{Digest, Sha256};
use std::os::unix::process::ExitStatusExt;

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
    let mut old = Process(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "standby::receiver_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("PARROT_STANDBY_TEST", root)
            .env(
                "PARROT_STANDBY_DIGEST",
                hex::encode(Sha256::digest(&invite)),
            )
            .spawn()
            .unwrap(),
    );
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
