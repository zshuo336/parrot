// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use parrot_remote::tls::TlsConfig;

fn root() -> PathBuf {
    std::env::var_os("PARROT_HANDOFF_RUN")
        .expect("handoff run directory")
        .into()
}

fn credentials(root: &Path, name: &str) -> TlsConfig {
    TlsConfig {
        ca_pem: std::fs::read(root.join("ca.pem")).unwrap(),
        cert_pem: std::fs::read(root.join(format!("{name}.pem"))).unwrap(),
        key_pem: std::fs::read(root.join(format!("{name}.key"))).unwrap(),
    }
}

async fn signal(root: &Path, name: &str) {
    tokio::time::timeout(Duration::from_secs(90), async {
        while !root.join(name).is_file() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect(name);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "operator-driven cross-host receiver handoff, original host"]
async fn original_host() {
    let root = root();
    let (delivery, _rx) = state(None);
    let release = Release(delivery.clone());
    let mut cfg = config(&root.join("state")).with_mtls(credentials(&root, "receiver"), []);
    cfg.bind = Some(
        std::env::var("PARROT_HANDOFF_BIND")
            .unwrap()
            .parse()
            .unwrap(),
    );
    let original = start(cfg.clone(), delivery.clone()).await;
    #[cfg(target_os = "linux")]
    let digest_hex = if std::env::var_os("PARROT_HANDOFF_DISCOVER").is_some() {
        let id = hex::encode(original.ordered_receiver_id().unwrap());
        std::fs::write(root.join("receiver-id"), id).unwrap();
        signal(&root, "invite-digest").await;
        Some(std::fs::read_to_string(root.join("invite-digest")).unwrap())
    } else {
        std::env::var("PARROT_HANDOFF_STANDBY_DIGEST").ok()
    };
    #[cfg(target_os = "linux")]
    if let Some(hex) = digest_hex {
        let hex = hex.trim();
        let digest = hex::decode(hex).unwrap().try_into().unwrap();
        original.register_receiver_standby(digest).await.unwrap();
    }
    std::fs::write(
        root.join("address"),
        original.local_addr().unwrap().to_string(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(90), delivery.entered.notified())
        .await
        .unwrap();
    std::fs::write(root.join("entered"), b"ok").unwrap();
    signal(&root, "stop").await;
    let old = original.clone();
    let shutdown = tokio::spawn(async move { old.shutdown().await });
    tokio::time::timeout(Duration::from_secs(5), delivery.dropping.notified())
        .await
        .unwrap();
    std::fs::write(root.join("dropping"), b"ok").unwrap();
    signal(&root, "release").await;
    release.release();
    shutdown.await.unwrap().unwrap();
    assert!(delivery.dropped.load(Ordering::SeqCst));
    std::fs::write(root.join("retired"), b"ok").unwrap();
    signal(&root, "sealed").await;
    let failure = RemoteActorSystem::new(cfg, Arc::new(Target(state(None).0)))
        .err()
        .unwrap();
    assert!(failure.to_string().contains("sealed"), "{failure}");
    println!("old host rejected restart after actual delivery cleanup and seal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "operator-driven cross-host receiver handoff, sender and destination host"]
async fn destination_host() {
    let root = root();
    let cfg = config(&root.join("state")).with_mtls(credentials(&root, "receiver"), []);
    let (new_state, mut rx) = state(None);
    let failure = RemoteActorSystem::new(cfg.clone(), Arc::new(Target(new_state.clone())))
        .err()
        .unwrap();
    assert!(failure.to_string().contains("awaits handoff"), "{failure}");
    let source = start(
        RemoteConfig::tcp("recovery-source", None).with_mtls(credentials(&root, "sender"), []),
        state(None).0,
    )
    .await;
    source
        .connect(&NodeAddr::tcp(
            "recovery-target",
            std::env::var("PARROT_HANDOFF_OLD")
                .unwrap()
                .parse()
                .unwrap(),
        ))
        .await
        .unwrap();
    let reference = source
        .remote_ref("parrot://recovery-target/target")
        .unwrap();
    send(&reference, 700).await;
    let links = source.links_snapshot().await;
    assert_eq!(links.len(), 1);
    links[0].1.close();
    tokio::time::timeout(Duration::from_secs(5), async {
        while source
            .links_snapshot()
            .await
            .iter()
            .any(|(_, peer, _)| !peer.is_closed())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(reference
        .close_ordered_session(Duration::from_millis(250))
        .await
        .is_err());
    std::fs::write(root.join("disconnected"), b"ok").unwrap();
    signal(&root, "activated").await;
    let replacement = start(cfg, new_state).await;
    connect(&source, &replacement).await;
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    send(&reference, 37).await;
    seen(&mut rx, 37).await;
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    source.shutdown().await.unwrap();
    replacement.shutdown().await.unwrap();
    println!("original sender closed the old ticket through successor and observed message 37");
}
