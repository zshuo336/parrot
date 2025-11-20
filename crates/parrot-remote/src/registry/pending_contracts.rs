use crate::{
    handshake::{caps, HandshakeBody},
    roles::directory::{client::DirectoryClient, DirectoryRequestStatus},
    transport::{memory::MemoryTransport, ConnectionHandle, Transport},
    Frame, FrameSender, LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig, ReplyPayload,
};
use bytes::Bytes;
use parrot_api::address::ActorRef;
use std::{future::Future, sync::Arc, task::Context, time::Duration};
use tokio::sync::{mpsc, oneshot, watch};

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}

pub(crate) struct Peer {
    pub(crate) remote: Arc<RemoteActorSystem>,
    pub(crate) address: NodeAddr,
    transport: MemoryTransport,
    connection: ConnectionHandle,
    inbound: mpsc::Receiver<(Frame, FrameSender, String)>,
}
impl Peer {
    pub(crate) async fn new() -> Self {
        let id = uuid::Uuid::new_v4();
        let remote = RemoteActorSystem::new(
            RemoteConfig::mem(format!("pending-source-{id}")),
            Arc::new(Empty),
        )
        .unwrap();
        remote.start().await.unwrap();
        let node = format!("pending-peer-{id}");
        let address = NodeAddr::mem(&node);
        let (tx, inbound) = mpsc::channel(8);
        let (shutdown, _) = watch::channel(false);
        let transport = MemoryTransport::new(
            HandshakeBody {
                node_id: node,
                capabilities: caps::BIN | caps::TELL_COMPLETION,
                ..Default::default()
            },
            tx,
            Arc::new(|_| {}),
            shutdown,
            None,
        )
        .unwrap();
        let (_, connection) =
            tokio::try_join!(remote.connect(&address), transport.accept()).unwrap();
        Self {
            remote,
            address,
            transport,
            connection,
            inbound,
        }
    }

    pub(crate) async fn request(&mut self, path: &str, key: &str) -> u64 {
        self.request_frame(path, key).await.header.correlation_id
    }

    pub(crate) async fn request_frame(&mut self, path: &str, key: &str) -> Frame {
        let (frame, _, from) = self.inbound.recv().await.unwrap();
        assert_eq!(frame.path, path);
        assert_eq!(frame.type_key, key);
        assert_eq!(from, self.remote.config.node_id);
        assert_ne!(frame.header.correlation_id, 0);
        assert_eq!(self.remote.callbacks.len(), 1);
        frame
    }

    pub(crate) async fn respond(&self, cid: u64, bytes: Bytes, key: &str) {
        self.connection
            .sender
            .send(Frame::reply(cid, "", key, bytes))
            .await
            .unwrap();
        // The caller future is deliberately unpolled until ingress consumes its slot.
        while !self.remote.callbacks.is_empty() {
            tokio::task::yield_now().await;
        }
    }

    pub(crate) fn replacement(&self, cid: u64) -> oneshot::Receiver<ReplyPayload> {
        let (tx, rx) = oneshot::channel();
        let sender = self.remote.sender_of(&self.address.node_id).unwrap();
        self.remote
            .callbacks
            .insert_on(cid, "replacement", tx, &sender)
            .unwrap();
        rx
    }

    pub(crate) fn check_replacement(&self, cid: u64, mut rx: oneshot::Receiver<ReplyPayload>) {
        assert_eq!(
            self.remote.callbacks.len(),
            1,
            "old request removed replacement"
        );
        let sender = self.remote.sender_of(&self.address.node_id).unwrap();
        let payload =
            || ReplyPayload::Ok(Bytes::from_static(b"replacement"), "replacement-key".into());
        let (tx, _rx) = mpsc::channel(1);
        let wrong = FrameSender::new(tx, &self.address.node_id);
        assert!(!self.remote.callbacks.complete_from(cid, payload(), &wrong));
        assert!(self.remote.callbacks.complete_from(cid, payload(), &sender));
        assert!(
            matches!(rx.try_recv().unwrap(), ReplyPayload::Ok(bytes, key)
            if bytes.as_ref() == b"replacement" && key == "replacement-key")
        );
        assert!(self.remote.callbacks.is_empty());
    }

    pub(crate) async fn close(self) {
        self.remote.shutdown().await.unwrap();
        self.transport.shutdown().await;
        self.connection.closed.await.unwrap();
    }
}

pub(crate) fn pending<F: Future>(future: std::pin::Pin<&mut F>) {
    assert!(future
        .poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
        .is_pending());
}

async fn actor_request(completion: bool, cancel: bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut peer = Peer::new().await;
        let reference = peer
            .remote
            .remote_ref(&format!("parrot://{}/target", peer.address.node_id))
            .unwrap();
        let mut request = Box::pin(async {
            if completion {
                reference
                    .deliver_with_ack(Box::new(crate::ingress::TestAsk(7)), Duration::from_secs(3))
                    .await
            } else {
                let reply = reference.send(Box::new(crate::ingress::TestAsk(7))).await?;
                assert_eq!(
                    reply.downcast_ref::<crate::ingress::TestReply>().unwrap().0,
                    17
                );
                Ok(())
            }
        });
        pending(request.as_mut());
        let cid = peer
            .request(
                &format!("parrot://{}/target", peer.address.node_id),
                "bin:parrot_remote::TestAsk#v1",
            )
            .await;
        if cancel {
            assert!(peer.remote.callbacks.remove(cid));
        } else if completion {
            peer.respond(cid, Bytes::new(), crate::frame::TELL_COMPLETION_KEY)
                .await;
        } else {
            peer.respond(
                cid,
                parrot_api::message::serde_remote_serialize(&crate::ingress::TestReply(17))
                    .unwrap()
                    .into(),
                "bin:parrot_remote::TestReply#v1",
            )
            .await;
        }
        let replacement = peer.replacement(cid);
        if cancel {
            drop(request);
        } else {
            request.await.unwrap();
        }
        peer.check_replacement(cid, replacement);
        peer.close().await;
    })
    .await
    .expect("actor request ownership made no progress");
}

#[tokio::test]
async fn ask_completion_preserves_reused_id() {
    actor_request(false, false).await;
}
#[tokio::test]
async fn ask_cancellation_preserves_reused_id() {
    actor_request(false, true).await;
}
#[tokio::test]
async fn tell_completion_preserves_reused_id() {
    actor_request(true, false).await;
}
#[tokio::test]
async fn tell_cancellation_preserves_reused_id() {
    actor_request(true, true).await;
}

async fn directory_request(cancel: bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut peer = Peer::new().await;
        let client = DirectoryClient::new(peer.remote.clone(), "ownership").unwrap();
        let address = peer.address.clone();
        let mut request =
            Box::pin(client.status(&address, "unknown", [0; 32], Duration::from_secs(3)));
        pending(request.as_mut());
        let cid = peer
            .request(
                &format!(
                    "parrot://{}{}",
                    peer.address.node_id,
                    crate::roles::directory::client::CLIENT_PATH
                ),
                crate::roles::directory::client::CLIENT_KEY,
            )
            .await;
        if cancel {
            assert!(peer.remote.callbacks.remove(cid));
        } else {
            let reply = Result::<_, crate::roles::directory::network::NetworkDirectoryError>::Ok(
                DirectoryRequestStatus::Unknown,
            );
            peer.respond(
                cid,
                parrot_api::message::serde_remote_serialize(&reply)
                    .unwrap()
                    .into(),
                crate::roles::directory::client::CLIENT_REPLY_KEY,
            )
            .await;
        }
        let replacement = peer.replacement(cid);
        if cancel {
            drop(request);
        } else {
            assert!(matches!(
                request.await.unwrap(),
                DirectoryRequestStatus::Unknown
            ));
        }
        peer.check_replacement(cid, replacement);
        peer.close().await;
    })
    .await
    .expect("Directory request ownership made no progress");
}
#[tokio::test]
async fn directory_completion_preserves_reused_id() {
    directory_request(false).await;
}
#[tokio::test]
async fn directory_cancellation_preserves_reused_id() {
    directory_request(true).await;
}
