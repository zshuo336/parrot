use super::*;
use crate::{
    registry::pending_contracts::Peer,
    system::lifecycle_diagnostic_contracts::{isolated, Probe},
};
use std::sync::atomic::Ordering;

async fn exercise(probe: Arc<Probe>) {
    let mut peer = Peer::new().await;
    let stop = CancellationToken::new();
    let (tx, rx) = mpsc::channel(2);
    let sender = tokio::spawn(pump(
        Arc::downgrade(&peer.remote),
        stop.clone(),
        vec![peer.address.clone()],
        rx,
        Duration::from_secs(3),
    ));
    tx.send(Bytes::from_static(b"first-raft")).await.unwrap();
    let path = format!("parrot://{}{RAFT_PATH}", peer.address.node_id);
    let first = peer.request_frame(&path, RAFT_KEY).await;
    assert_eq!(first.borrowed_ask_payload().unwrap(), b"first-raft");
    let cid = first.header.correlation_id;
    probe.armed.store(true, Ordering::SeqCst);
    peer.respond(cid, Bytes::new(), "rejected-acceptance").await;
    probe.wait().await;
    assert!(peer.remote.callbacks.is_empty());
    tx.send(Bytes::from_static(b"second-raft"))
        .await
        .expect("diagnostic killed the Raft sender queue");
    let next = tokio::time::timeout(Duration::from_secs(3), peer.request_frame(&path, RAFT_KEY))
        .await
        .expect("diagnostic prevented the next Raft acceptance attempt");
    assert_eq!(next.borrowed_ask_payload().unwrap(), b"second-raft");
    assert_ne!(next.header.correlation_id, cid);
    peer.respond(next.header.correlation_id, Bytes::new(), RAFT_ACK_KEY)
        .await;
    drop(tx);
    sender.await.expect("diagnostic panicked the Raft sender");
    assert!(peer.remote.callbacks.is_empty());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    probe.armed.store(false, Ordering::SeqCst);
    stop.cancel();
    peer.close().await;
}

macro_rules! contract {
    ($name:ident, $payload:literal) => {
        #[tokio::test]
        async fn $name() {
            let probe = Probe::new("Directory Raft delivery failed; protocol will retry");
            isolated(
                concat!(
                    "roles::directory::network::diagnostic_contracts::",
                    stringify!($name)
                ),
                &["parrot_remote::roles::directory::network"],
                $payload,
                probe.clone(),
                exercise(probe),
            )
            .await;
        }
    };
}
contract!(raft_sender_diagnostic_preserves_next_message, false);
contract!(raft_sender_diagnostic_payload_preserves_next_message, true);
