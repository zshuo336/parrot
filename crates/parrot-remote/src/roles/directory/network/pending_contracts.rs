use super::*;
use crate::registry::pending_contracts::{pending, Peer};

async fn pump_ownership(cancel: bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut peer = Peer::new().await;
        let stop = CancellationToken::new();
        let (tx, rx) = mpsc::channel(1);
        tx.send(Bytes::from_static(b"raft-request")).await.unwrap();
        let mut sending = Box::pin(pump(
            Arc::downgrade(&peer.remote),
            stop.clone(),
            vec![peer.address.clone()],
            rx,
            Duration::from_secs(3),
        ));
        pending(sending.as_mut());
        let cid = peer
            .request(
                &format!("parrot://{}{RAFT_PATH}", peer.address.node_id),
                RAFT_KEY,
            )
            .await;
        if cancel {
            assert!(peer.remote.callbacks.remove(cid));
        } else {
            peer.respond(cid, Bytes::new(), RAFT_ACK_KEY).await;
        }
        let replacement = peer.replacement(cid);
        if cancel {
            stop.cancel();
            sending.await;
        } else {
            pending(sending.as_mut());
            stop.cancel();
            sending.await;
        }
        peer.check_replacement(cid, replacement);
        peer.close().await;
    })
    .await
    .expect("Raft sender ownership made no progress");
}

#[tokio::test]
async fn raft_acceptance_preserves_reused_id() {
    pump_ownership(false).await;
}
#[tokio::test]
async fn raft_cancellation_preserves_reused_id() {
    pump_ownership(true).await;
}
