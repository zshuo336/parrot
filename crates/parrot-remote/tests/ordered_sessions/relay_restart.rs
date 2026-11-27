// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use parrot_remote::{handshake::TopologyRole, tls::TlsConfig, RemoteActorRef};
use std::os::unix::process::ExitStatusExt;

const HUB: &str = "restart-hub";
const WAIT: Duration = Duration::from_secs(5);

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "hub subprocess driven by the relay restart contracts"]
async fn hub_fixture() {
    let dir = PathBuf::from(std::env::var_os("PARROT_RELAY_FIXTURE_DIR").unwrap());
    let tls = TlsConfig {
        ca_pem: std::fs::read(dir.join("ca.pem")).unwrap(),
        cert_pem: std::fs::read(dir.join("hub.pem")).unwrap(),
        key_pem: std::fs::read(dir.join("hub.key")).unwrap(),
    };
    let cfg = RemoteConfig::tcp(HUB, Some("127.0.0.1:0".parse().unwrap()))
        .with_role(TopologyRole::Hub)
        .with_mtls(tls, []);
    let hub = start(cfg, state(None).0).await;
    std::fs::write(dir.join("address"), hub.local_addr().unwrap().to_string()).unwrap();
    wait_file(&dir.join("observe")).await;
    until(|| hub.ingress.relay_table.len() == 1 && parrot_remote::RelayMetrics::relayed_ask() > 0)
        .await;
    let forwarded = parrot_remote::RelayMetrics::relayed_ask();
    std::fs::write(dir.join("pending"), b"1").unwrap();
    if dir.join("replacement").exists() {
        wait_file(&dir.join("retry")).await;
        until(|| {
            parrot_remote::RelayMetrics::relayed_ask() > forwarded
                && !hub.ingress.relay_table.is_empty()
        })
        .await;
        std::fs::write(dir.join("retried"), b"forwarded").unwrap();
    }
    wait_file(&dir.join("drain")).await;
    until(|| hub.ingress.relay_table.is_empty()).await;
    std::fs::write(dir.join("drained"), b"0").unwrap();
    wait_file(&dir.join("stop")).await;
    hub.shutdown().await.unwrap();
    std::fs::write(dir.join("finished"), b"closed").unwrap();
}

async fn launch(dir: &Path, tls: &TlsConfig) -> (Process, NodeAddr) {
    for (name, pem) in [
        ("ca.pem", &tls.ca_pem),
        ("hub.pem", &tls.cert_pem),
        ("hub.key", &tls.key_pem),
    ] {
        std::fs::write(dir.join(name), pem).unwrap();
    }
    let child = Process(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "relay_restart::hub_fixture"])
            .env("PARROT_RELAY_FIXTURE_DIR", dir)
            .spawn()
            .unwrap(),
    );
    let addr = String::from_utf8(wait_file(&dir.join("address")).await).unwrap();
    (child, NodeAddr::tcp(HUB, addr.parse().unwrap()))
}

async fn signal_and_wait(dir: &Path, signal: &str, reply: &str, expected: &[u8]) {
    std::fs::write(dir.join(signal), b"go").unwrap();
    assert_eq!(wait_file(&dir.join(reply)).await, expected);
}

fn close(reference: &RemoteActorRef) -> tokio::task::JoinHandle<ActorResult<()>> {
    let reference = reference.clone();
    tokio::spawn(async move {
        reference
            .close_ordered_session(Duration::from_secs(20))
            .await
    })
}

async fn restart(cancel_before_exit: bool) {
    let pki = pki::Pki::new();
    let credentials = pki.node(HUB);
    let original = tempfile::tempdir().unwrap();
    let replacement = tempfile::tempdir().unwrap();
    let (mut old_hub, old_address) = launch(original.path(), &credentials).await;
    // Neither endpoint listens. The hub is the only route for requests and receipts.
    let cfg = |name: &str| RemoteConfig::tcp(name, None).with_mtls(pki.node(name), []);
    let (activity, mut received) = state(None);
    let release = Release(activity.clone());
    let target = start(
        cfg("recovery-target").with_ordered_relays([HUB.into()]),
        activity.clone(),
    )
    .await;
    let source = start(cfg("recovery-source"), state(None).0).await;
    target.connect(&old_address).await.unwrap();
    source.connect(&old_address).await.unwrap();
    let reference = source
        .remote_ref("parrot://recovery-target/target")
        .unwrap();
    send(&reference, 700).await;
    tokio::time::timeout(WAIT, activity.entered.notified())
        .await
        .unwrap();
    let attempt = close(&reference);
    tokio::time::timeout(WAIT, activity.dropping.notified())
        .await
        .unwrap();
    signal_and_wait(original.path(), "observe", "pending", b"1").await;
    assert_eq!(source.callbacks.len(), 1);
    if cancel_before_exit {
        attempt.abort();
        assert!(attempt.await.unwrap_err().is_cancelled());
        assert!(source.callbacks.is_empty());
        let links = source.links_snapshot().await;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, HUB);
        links[0].1.close();
        signal_and_wait(original.path(), "drain", "drained", b"0").await;
        assert_eq!(old_hub.kill_and_reap().signal(), Some(libc::SIGKILL));
    } else {
        assert_eq!(old_hub.kill_and_reap().signal(), Some(libc::SIGKILL));
        let error = tokio::time::timeout(WAIT, attempt)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains("remote connection lost"),
            "{error}"
        );
    }
    assert!(source.callbacks.is_empty());
    assert!(!activity.dropped.load(Ordering::SeqCst));

    std::fs::write(replacement.path().join("replacement"), b"1").unwrap();
    let (mut new_hub, address) = launch(replacement.path(), &credentials).await;
    target.connect(&address).await.unwrap();
    source.connect(&address).await.unwrap();
    let pending = close(&reference);
    signal_and_wait(replacement.path(), "observe", "pending", b"1").await;
    assert!(!activity.dropped.load(Ordering::SeqCst));
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(source.callbacks.is_empty());
    let retry = close(&reference);
    signal_and_wait(replacement.path(), "retry", "retried", b"forwarded").await;
    assert_eq!(source.callbacks.len(), 1);
    assert!(!activity.dropped.load(Ordering::SeqCst));
    release.release();
    tokio::time::timeout(WAIT, retry)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(activity.dropped.load(Ordering::SeqCst));
    send(&reference, 41).await;
    seen(&mut received, 41).await;
    reference.close_ordered_session(WAIT).await.unwrap();
    assert!(received.try_recv().is_err());
    assert!(source.callbacks.is_empty());
    signal_and_wait(replacement.path(), "drain", "drained", b"0").await;
    signal_and_wait(replacement.path(), "stop", "finished", b"closed").await;
    until(|| new_hub.0.try_wait().unwrap().is_some()).await;
    assert!(new_hub.0.wait().unwrap().success());
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_mid_close_hub_recovers_over_new_process_and_ignores_cancelled_retry() {
    restart(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_cancel_and_disconnect_drain_old_hub_before_process_replacement() {
    restart(true).await;
}
