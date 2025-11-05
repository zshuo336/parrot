//! Deterministic adversarial ingress tests. In-process transport authentication
//! is tested separately in resolve_control_plane; these use explicit dev policy
//! to control response ordering and queue capacity without sockets or sleeps.
use parrot_remote::{
    frame::{frame_type, FrameHeader, DEFAULT_HOP_LIMIT, SEQ_NONE},
    ingress::{Ingress, LocalLookup},
    resolve::{self, Invalidate, Response},
    roles::directory::DirEntry,
    security::ControlPolicy,
    transport::FrameSender,
    CallbackRegistry, Frame,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
struct Lookup;
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
fn ingress() -> Arc<Ingress> {
    Arc::new(
        Ingress::new(Arc::new(Lookup), Arc::new(CallbackRegistry::default()))
            .with_control_policy(ControlPolicy::InsecureDevelopment),
    )
}
fn sender(node: &str, capacity: usize) -> (FrameSender, mpsc::Receiver<Frame>) {
    let (tx, rx) = mpsc::channel(capacity);
    (FrameSender::new(tx, node), rx)
}
fn reply(request: &Frame, version: u64, ttl: u32) -> Frame {
    let mut frame = request.clone();
    frame.header.frame_type = frame_type::RESOLVE_R;
    frame.payload = resolve::encode_response(&Response {
        key: request.path.clone(),
        entry: Some(DirEntry {
            version,
            ttl_s: ttl,
            endpoints: vec![format!("route-{version}")],
        }),
    })
    .unwrap()
    .into();
    frame
}
fn invalidation(key: &str, version: u64) -> Frame {
    Frame {
        header: FrameHeader {
            frame_len: 0,
            version: 1,
            frame_type: frame_type::INVALIDATE,
            flags: 0,
            correlation_id: 0,
            hop_count: 0,
            hop_limit: DEFAULT_HOP_LIMIT,
            seq: SEQ_NONE,
        },
        path: key.into(),
        type_key: resolve::RESPONSE_KEY.into(),
        payload: resolve::encode_invalidate(&Invalidate {
            key: key.into(),
            version,
        })
        .unwrap()
        .into(),
    }
}

#[tokio::test]
async fn cancelling_actual_resolve_during_send_or_reply_releases_pending_capacity() {
    let ingress = ingress();
    let (source, mut rx) = sender("a", 1);
    source.send(invalidation("fill-writer", 1)).await.unwrap();
    let mut blocked =
        Box::pin(ingress.resolve_remote(&source, "blocked", 0, Duration::from_secs(30)));
    assert!(futures::poll!(&mut blocked).is_pending());
    assert_eq!(ingress.pending_resolves(), 1);
    drop(blocked);
    assert_eq!(ingress.pending_resolves(), 0);
    rx.recv().await.unwrap();
    let mut pending =
        Box::pin(ingress.resolve_remote(&source, "waiting-reply", 0, Duration::from_secs(30)));
    assert!(futures::poll!(&mut pending).is_pending());
    let request = rx.recv().await.unwrap();
    drop(pending);
    assert_eq!(ingress.pending_resolves(), 0);
    ingress.dispatch(reply(&request, 7, 60), &source, "a").await;
    assert!(ingress
        .cached_resolve(&source, "waiting-reply", Instant::now())
        .is_none());

    let mut pending =
        Box::pin(ingress.resolve_remote(&source, "disconnect", 0, Duration::from_secs(30)));
    assert!(futures::poll!(&mut pending).is_pending());
    rx.recv().await.unwrap();
    source.close();
    assert!(tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .is_err());
    assert_eq!(ingress.pending_resolves(), 0);
}

#[tokio::test]
async fn pending_limit_close_send_error_and_invalid_timeouts_are_immediate() {
    let ingress = ingress();
    let (source, mut rx) = sender("a", 64);
    let mut pending = Vec::new();
    for _ in 0..64 {
        let mut request =
            Box::pin(ingress.resolve_remote(&source, "node", 0, Duration::from_secs(30)));
        assert!(futures::poll!(&mut request).is_pending());
        rx.recv().await.unwrap();
        pending.push(request);
    }
    assert_eq!(ingress.pending_resolves(), 64);
    assert!(ingress
        .resolve_remote(&source, "node", 0, Duration::from_secs(30))
        .await
        .unwrap_err()
        .to_string()
        .contains("limit"));
    drop(pending.pop());
    assert_eq!(ingress.pending_resolves(), 63);
    ingress.close();
    assert_eq!(ingress.pending_resolves(), 0);
    for request in pending {
        assert!(request.await.is_err());
    }
    let other = super_ingress();
    for timeout in [Duration::ZERO, Duration::MAX] {
        assert!(other
            .resolve_remote(&source, "node", 0, timeout)
            .await
            .is_err());
        assert_eq!(other.pending_resolves(), 0);
        assert!(rx.try_recv().is_err());
    }
    drop(rx);
    let error = other
        .resolve_remote(&source, "node", 0, Duration::from_secs(30))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("send failed"), "{error}");
    assert_eq!(other.pending_resolves(), 0);
}
// Avoid shadowing the helper by the local ingress owner.
fn super_ingress() -> Arc<Ingress> {
    ingress()
}

#[tokio::test]
async fn server_bounds_application_resolver_override_and_releases_its_future() {
    struct Resolver(Arc<std::sync::atomic::AtomicBool>);
    struct Dropped(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl LocalLookup for Resolver {
        async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
            None
        }
        async fn resolve_directory(&self, _: &str, _: u64) -> Result<Option<DirEntry>, String> {
            let _guard = Dropped(self.0.clone());
            std::future::pending().await
        }
    }
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = Arc::new(
        Ingress::new(
            Arc::new(Resolver(dropped.clone())),
            Arc::new(CallbackRegistry::default()),
        )
        .with_control_policy(ControlPolicy::InsecureDevelopment),
    );
    let client = ingress();
    let (source, mut rx) = sender("server", 1);
    let (back, mut replies) = sender("client", 1);
    let mut pending = Box::pin(client.resolve_remote(&source, "node", 0, Duration::from_secs(5)));
    assert!(futures::poll!(&mut pending).is_pending());
    let query = rx.recv().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        server.dispatch(query, &back, "client"),
    )
    .await
    .unwrap();
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    let response = replies.recv().await.unwrap();
    assert_eq!(response.header.frame_type, frame_type::REPLY_ERR);
    let (code, _) = parrot_remote::error::decode_err_payload(&response.payload).unwrap();
    assert_eq!(code, parrot_remote::ErrCode::Timeout);
    client.dispatch(response, &source, "server").await;
    assert!(pending.await.is_err());
    assert_eq!(client.pending_resolves(), 0);
}

#[tokio::test]
async fn wrong_physical_connection_cannot_consume_pending_and_authorities_are_isolated() {
    let ingress = ingress();
    let (a, mut a_rx) = sender("a", 4);
    let (impostor, _) = sender("a", 4); // even the same claimed node is a different connection
    let (b, _) = sender("b", 4);
    let mut lookup = Box::pin(ingress.resolve_remote(&a, "node", 5, Duration::from_secs(1)));
    assert!(futures::poll!(&mut lookup).is_pending());
    let query = a_rx.recv().await.unwrap();
    ingress.dispatch(reply(&query, 5, 60), &impostor, "a").await;
    assert_eq!(ingress.pending_resolves(), 1);
    assert!(futures::poll!(&mut lookup).is_pending());
    ingress.dispatch(invalidation("node", 99), &b, "b").await;
    ingress.dispatch(reply(&query, 5, 60), &a, "a").await;
    assert_eq!(lookup.await.unwrap().unwrap().version, 5);
    assert!(ingress.cached_resolve(&a, "node", Instant::now()).is_some());
    assert!(ingress.cached_resolve(&b, "node", Instant::now()).is_none());
    ingress.dispatch(invalidation("node", 100), &b, "b").await;
    assert!(ingress.cached_resolve(&a, "node", Instant::now()).is_some());
}

#[tokio::test]
async fn malformed_bound_reply_and_minimum_version_fail_without_caching() {
    let ingress = ingress();
    let (source, mut rx) = sender("a", 1);
    for case in 0..4 {
        let mut lookup =
            Box::pin(ingress.resolve_remote(&source, "node", 10, Duration::from_secs(1)));
        assert!(futures::poll!(&mut lookup).is_pending());
        let mut response = reply(&rx.recv().await.unwrap(), 10, 60);
        match case {
            0 => response.path = "wrong-key".into(),
            1 => response.header.seq = 1,
            2 => response = reply(&response, 9, 60),
            _ => response.payload = bytes::Bytes::from_static(b"bad payload"),
        }
        ingress.dispatch(response, &source, "a").await;
        assert!(lookup.await.is_err());
        assert_eq!(ingress.pending_resolves(), 0);
        assert!(ingress
            .cached_resolve(&source, "node", Instant::now())
            .is_none());
    }
}

#[tokio::test]
async fn invalidation_of_inflight_ticket_survives_tombstone_eviction_and_revalidation_works() {
    let ingress = ingress();
    let (source, mut rx) = sender("a", 1);
    let mut lookup = Box::pin(ingress.resolve_remote(&source, "node", 0, Duration::from_secs(30)));
    assert!(futures::poll!(&mut lookup).is_pending());
    let query = rx.recv().await.unwrap();
    ingress
        .dispatch(invalidation("node", 7), &source, "a")
        .await;
    // More distinct fences than the cache can retain, while the original
    // request is still pending. Correctness must live on its ticket.
    for index in 0..65_537 {
        ingress
            .dispatch(invalidation(&format!("other-{index}"), 7), &source, "a")
            .await;
    }
    ingress.dispatch(reply(&query, 6, 60), &source, "a").await;
    assert!(lookup
        .await
        .unwrap_err()
        .to_string()
        .contains("DirectoryStale"));
    assert!(ingress
        .cached_resolve(&source, "node", Instant::now())
        .is_none());
    let mut lookup = Box::pin(ingress.resolve_remote(&source, "node", 7, Duration::from_secs(1)));
    assert!(futures::poll!(&mut lookup).is_pending());
    ingress
        .dispatch(reply(&rx.recv().await.unwrap(), 7, 60), &source, "a")
        .await;
    assert_eq!(lookup.await.unwrap().unwrap().version, 7);
    assert!(ingress
        .cached_resolve(&source, "node", Instant::now())
        .is_some());
    let mut lookup = Box::pin(ingress.resolve_remote(&source, "node", 7, Duration::from_secs(1)));
    assert!(futures::poll!(&mut lookup).is_pending());
    ingress
        .dispatch(reply(&rx.recv().await.unwrap(), 7, 0), &source, "a")
        .await;
    assert_eq!(lookup.await.unwrap().unwrap().ttl_s, 0);
    assert!(ingress
        .cached_resolve(&source, "node", Instant::now())
        .is_none());
}
