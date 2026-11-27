// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use std::os::unix::process::ExitStatusExt;
#[path = "sender_alias_restart.rs"]
mod alias_restart;

fn sender_config(directory: &Path) -> RemoteConfig {
    let mut cfg = RemoteConfig::tcp("recovery-source", None)
        .with_insecure_development()
        .with_ordered_receiver_state(OrderedReceiverState {
            directory: directory.into(),
            realm: "sender-contract".into(),
        });
    cfg.persist_sender_sessions = true;
    cfg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "source subprocess driven by the sender restart contracts"]
async fn sender_process_fixture() {
    let dir = PathBuf::from(std::env::var_os("PARROT_SENDER_DIR").unwrap());
    let target = std::env::var("PARROT_SENDER_TARGET")
        .unwrap()
        .parse()
        .unwrap();
    let source = start(sender_config(&dir), state(None).0).await;
    source
        .connect(&NodeAddr::tcp("recovery-target", target))
        .await
        .unwrap();
    let reference = source
        .remote_ref("parrot://recovery-target/target")
        .unwrap();
    send(&reference, 700).await;
    std::fs::write(dir.join("sent"), b"sent").unwrap();
    if std::env::var_os("PARROT_CLOSE_IN_FLIGHT").is_some() {
        reference
            .close_ordered_session(Duration::from_secs(30))
            .await
            .unwrap();
        panic!("old worker retired while its destructor was blocked");
    }
    std::future::pending::<()>().await;
}

async fn restart(close_in_flight: bool) {
    let directory = tempfile::tempdir().unwrap();
    let receiver_directory = tempfile::tempdir().unwrap();
    let (target_state, mut rx) = state(None);
    let release = Release(target_state.clone());
    let target = start(config(receiver_directory.path()), target_state.clone()).await;
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "sender_restart::sender_process_fixture",
        "--ignored",
    ])
    .env("PARROT_SENDER_DIR", directory.path())
    .env(
        "PARROT_SENDER_TARGET",
        target.local_addr().unwrap().to_string(),
    )
    .env_remove("PARROT_CLOSE_IN_FLIGHT");
    if close_in_flight {
        cmd.env("PARROT_CLOSE_IN_FLIGHT", "1");
    }
    let mut child = Process(cmd.spawn().unwrap());
    wait_file(&directory.path().join("sent")).await;
    tokio::time::timeout(Duration::from_secs(5), target_state.entered.notified())
        .await
        .unwrap();
    if close_in_flight {
        tokio::time::timeout(Duration::from_secs(5), target_state.dropping.notified())
            .await
            .unwrap();
    }
    assert_eq!(child.kill_and_reap().signal(), Some(libc::SIGKILL));
    let mut disabled = sender_config(directory.path());
    disabled.persist_sender_sessions = false;
    assert!(RemoteActorSystem::new(disabled, Arc::new(Target(state(None).0))).is_err());
    let replacement = start(sender_config(directory.path()), state(None).0).await;
    replacement.nodes.add_seed(NodeAddr::mem("recovery-target"));
    let reference = replacement
        .remote_ref("parrot://recovery-target/target")
        .unwrap();
    assert!(reference
        .close_ordered_session(Duration::from_millis(50))
        .await
        .is_err());
    connect(&replacement, &target).await;
    if !close_in_flight {
        tokio::time::timeout(Duration::from_secs(5), target_state.dropping.notified())
            .await
            .unwrap();
    }
    assert!(reference
        .close_ordered_session(Duration::from_millis(50))
        .await
        .is_err());
    assert!(reference
        .deliver_ordered(
            Box::new(Work(99)),
            parrot_api::message::MessageOptions {
                timeout: Some(Duration::from_millis(70)),
                ..Default::default()
            }
        )
        .await
        .is_err());
    assert!(rx.try_recv().is_err());
    release.release();
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    assert!(target_state.dropped.load(Ordering::SeqCst));
    send(&reference, 1).await;
    seen(&mut rx, 1).await;
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    replacement.shutdown().await.unwrap();
    let mut disabled = sender_config(directory.path());
    disabled.persist_sender_sessions = false;
    let next = start(disabled, state(None).0).await;
    next.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_sender_recovers_the_original_ticket_before_replacement_delivery() {
    restart(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_sender_resumes_pending_close_until_actual_receiver_cleanup() {
    restart(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_ticket_publication_prevents_delivery_and_can_be_retried() {
    let directory = tempfile::tempdir().unwrap();
    let receiver_directory = tempfile::tempdir().unwrap();
    let (target_state, mut rx) = state(None);
    let target = start(config(receiver_directory.path()), target_state).await;
    let source = start(sender_config(directory.path()), state(None).0).await;
    connect(&source, &target).await;
    let reference = source
        .remote_ref("parrot://recovery-target/target")
        .unwrap();
    let next = directory.path().join("sender.next");
    std::fs::create_dir(&next).unwrap();
    assert!(reference
        .deliver_ordered(Box::new(Work(9)), Default::default())
        .await
        .is_err());
    std::fs::remove_dir(next).unwrap();
    send(&reference, 1).await;
    seen(&mut rx, 1).await;
    assert!(rx.try_recv().is_err());
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}
