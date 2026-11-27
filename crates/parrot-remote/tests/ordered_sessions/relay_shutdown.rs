// SPDX-License-Identifier: AGPL-3.0-only
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_hub_requests_retires_forwarded_close_without_certifying_delivery_cleanup() {
    let pki = Pki::new();
    let config = |name: &str| {
        RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap())).with_mtls(pki.node(name), [])
    };
    let hub = start(
        config("drain-hub").with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    let (activity, mut rx) = state();
    let release = Release(activity.clone());
    let target = start(
        config("drain-target").with_ordered_relays(["drain-hub".into()]),
        Arc::new(Target(activity.clone())),
    )
    .await;
    let source = start(config("drain-source"), Arc::new(Empty)).await;
    let address = NodeAddr::tcp("drain-hub", hub.local_addr().unwrap());
    target.connect(&address).await.unwrap();
    source.connect(&address).await.unwrap();
    let reference = source.remote_ref("parrot://drain-target/target").unwrap();
    strict(&reference, 700).await;
    tokio::time::timeout(Duration::from_secs(3), activity.entered.notified())
        .await
        .unwrap();

    let closing = reference.clone();
    let attempt =
        tokio::spawn(async move { closing.close_ordered_session(Duration::from_secs(15)).await });
    tokio::time::timeout(Duration::from_secs(3), activity.dropping.notified())
        .await
        .unwrap();
    assert_eq!(hub.ingress.relay_table.len(), 1);
    hub.ingress.close_asks();
    assert!(
        hub.ingress.relay_table.is_empty(),
        "closed hub retained the forwarded request"
    );
    let error = tokio::time::timeout(Duration::from_secs(2), attempt)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error.to_string().contains("remote connection lost"),
        "{error}"
    );
    assert!(!activity.dropped.load(Ordering::SeqCst));

    source
        .connect(&NodeAddr::tcp("drain-target", target.local_addr().unwrap()))
        .await
        .unwrap();
    assert!(reference
        .close_ordered_session(Duration::from_millis(100))
        .await
        .is_err());
    assert!(!activity.dropped.load(Ordering::SeqCst));
    release.release();
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    assert!(activity.dropped.load(Ordering::SeqCst));
    strict(&reference, 31).await;
    seen(&mut rx, 31).await;
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    for node in [&source, &target, &hub] {
        node.shutdown().await.unwrap();
    }
}
