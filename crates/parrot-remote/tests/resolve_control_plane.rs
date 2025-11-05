use async_trait::async_trait;
use bytes::Bytes;
use parrot_remote::{
    cache::CacheState,
    handshake::HandshakeBody,
    ingress::Ingress,
    roles::directory::DirEntry,
    security::PeerAuthentication,
    transport::{memory::MemoryTransport, Transport},
    CallbackRegistry, LocalLookup,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

struct Lookup {
    entry: DirEntry,
}

#[async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }

    async fn resolve_directory(
        &self,
        key: &str,
        min_version: u64,
    ) -> Result<Option<DirEntry>, String> {
        if key == "service-a" && self.entry.version >= min_version {
            Ok(Some(self.entry.clone()))
        } else {
            Ok(None)
        }
    }
}

#[tokio::test]
async fn resolve_q_r_updates_cache_and_invalidate_forces_miss() {
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let (in_a, mut rx_a) = mpsc::channel(32);
    let (in_b, mut rx_b) = mpsc::channel(32);
    let node_a = format!("resolve-a-{}", std::process::id());
    let node_b = format!("resolve-b-{}", std::process::id());
    let make = |node: String, inbound| {
        MemoryTransport::new(
            HandshakeBody {
                node_id: node,
                ..Default::default()
            },
            inbound,
            Arc::new(|_| {}),
            shutdown.clone(),
            None,
        )
        .unwrap()
    };
    let transport_a = make(node_a, in_a);
    let transport_b = make(node_b.clone(), in_b);
    let address = parrot_remote::node::NodeAddr::mem(node_b);
    let (a, b) = tokio::try_join!(transport_a.connect(&address), transport_b.accept()).unwrap();
    assert_eq!(a.sender.authentication(), PeerAuthentication::InProcess);
    assert_eq!(b.sender.authentication(), PeerAuthentication::InProcess);
    let source = Arc::new(Ingress::new(
        Arc::new(Lookup {
            entry: DirEntry {
                endpoints: vec!["tcp://127.0.0.1:7001".into()],
                version: 7,
                ttl_s: 60,
            },
        }),
        Arc::new(CallbackRegistry::default()),
    ));
    let target = Arc::new(Ingress::new(
        Arc::new(Lookup {
            entry: DirEntry {
                endpoints: vec!["tcp://127.0.0.1:7001".into()],
                version: 7,
                ttl_s: 60,
            },
        }),
        Arc::new(CallbackRegistry::default()),
    ));
    let target_task = {
        let target = target.clone();
        tokio::spawn(async move {
            while let Some((frame, back, from)) = rx_b.recv().await {
                target.dispatch(frame, &back, &from).await;
            }
        })
    };
    let source_task = {
        let source = source.clone();
        tokio::spawn(async move {
            while let Some((frame, back, from)) = rx_a.recv().await {
                source.dispatch(frame, &back, &from).await;
            }
        })
    };

    let entry = source
        .resolve_remote(&a.sender, "service-a", 1, Duration::from_secs(2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.version, 7);
    assert_eq!(entry.endpoints, vec!["tcp://127.0.0.1:7001"]);
    assert!(matches!(
        source.cached_resolve(&a.sender, "service-a", std::time::Instant::now()),
        Some((_, CacheState::Fresh))
    ));

    let invalidation =
        parrot_remote::resolve::encode_invalidate(&parrot_remote::resolve::Invalidate {
            key: "service-a".into(),
            version: 8,
        })
        .unwrap();
    b.sender
        .send(parrot_remote::Frame {
            header: parrot_remote::FrameHeader {
                frame_len: 0,
                version: 1,
                frame_type: parrot_remote::frame::frame_type::INVALIDATE,
                flags: 0,
                correlation_id: 0,
                hop_count: 0,
                hop_limit: 8,
                seq: parrot_remote::frame::SEQ_NONE,
            },
            path: "service-a".into(),
            type_key: parrot_remote::resolve::RESPONSE_KEY.to_owned(),
            payload: Bytes::from(invalidation),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while source
            .cached_resolve(&a.sender, "service-a", std::time::Instant::now())
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    a.sender.close();
    b.sender.close();
    shutdown.send_replace(true);
    transport_a.close();
    transport_b.close();
    transport_a.shutdown().await;
    transport_b.shutdown().await;
    target_task.abort();
    source_task.abort();
    assert!(target_task.await.unwrap_err().is_cancelled());
    assert!(source_task.await.unwrap_err().is_cancelled());
}
