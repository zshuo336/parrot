use bytes::Bytes;
use parrot_api::{
    address::ActorRef,
    message::RemoteMessage,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    frame::Frame, transport::FrameSender, LocalLookup, RemoteActorSystem, RemoteConfig,
};
use std::{sync::Arc, time::Duration};

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Probe(u64);
impl parrot_api::Message for Probe {
    type Result = ();
}

#[derive(Debug)]
struct State {
    entered: tokio::sync::Notify,
    retired: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    received: tokio::sync::mpsc::UnboundedSender<u64>,
}

#[derive(Debug, Clone)]
struct Target(Arc<State>);
impl Drop for Target {
    fn drop(&mut self) {
        self.0.retired.notify_one();
    }
}
impl ActorRef for Target {
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let value = message.downcast_ref::<Probe>().unwrap().0;
            if value == 0 {
                self.0.entered.notify_one();
                self.0.release.acquire().await.unwrap().forget();
            }
            assert_ne!(value, 99, "controlled business delivery panic");
            self.0.received.send(value).unwrap();
            Ok(())
        })
    }
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/budget".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[async_trait::async_trait]
impl LocalLookup for Target {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        Some(self.clone_boxed())
    }
}

fn frame(value: u64, sequence: u32) -> Frame {
    Frame::tell_with_seq(
        "/budget",
        Probe::TYPE_KEY,
        Bytes::from(parrot_api::message::serde_remote_serialize(&Probe(value)).unwrap()),
        sequence,
    )
}

fn connection() -> (FrameSender, tokio::sync::mpsc::Receiver<Frame>) {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    (FrameSender::anon(tx), rx)
}

async fn receive(rx: &mut tokio::sync::mpsc::UnboundedReceiver<u64>, expected: u64) {
    let actual = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap();
    assert_eq!(actual, Some(expected));
}

#[tokio::test]
async fn file_configured_frame_and_byte_limits_cover_active_delivery_and_recover() {
    let mut encoded = bytes::BytesMut::new();
    frame(0, 1).encode(&mut encoded).unwrap();
    for (name, frames, bytes) in [("frame", 2, 4096), ("byte", 100, encoded.len() * 2)] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("parrot.toml");
        std::fs::write(&path, format!(
            "[remote.node]\nscheme='mem'\nnode_id='reorder-{name}'\n[remote.reorder]\nstream_capacity=4\nframe_capacity={frames}\nbyte_capacity={bytes}\n"
        )).unwrap();
        let resolved = parrot_config::ParrotConfig::builder()
            .load_file(path)
            .unwrap()
            .build()
            .unwrap();
        let (received, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let target = Target(Arc::new(State {
            entered: tokio::sync::Notify::new(),
            retired: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
            received,
        }));
        let system = RemoteActorSystem::new(
            RemoteConfig::from_resolved(&resolved).unwrap(),
            Arc::new(target.clone()),
        )
        .unwrap();
        system.start().await.unwrap();
        let (back, _rx) = connection();
        system.ingress.dispatch(frame(0, 1), &back, "source").await;
        tokio::time::timeout(Duration::from_secs(2), target.0.entered.notified())
            .await
            .unwrap();
        system.ingress.dispatch(frame(1, 2), &back, "source").await;
        assert!(!back.is_closed());
        let (overflow, _overflow_rx) = connection();
        system
            .ingress
            .dispatch(frame(2, 1), &overflow, "overflow")
            .await;
        assert!(
            overflow.is_closed(),
            "{name} budget omitted active delivery or queued work"
        );
        assert!(!back.is_closed());
        target.0.release.add_permits(1);
        receive(&mut rx, 0).await;
        receive(&mut rx, 1).await;
        // Observe actual release of the final deliver future before readmission.
        tokio::task::yield_now().await;
        system.ingress.dispatch(frame(3, 3), &back, "source").await;
        receive(&mut rx, 3).await;
        assert!(!back.is_closed());
        assert!(rx.try_recv().is_err(), "rejected work reached the actor");
        system.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn business_panic_returns_reorder_frame_and_byte_capacity() {
    let (received, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let target = Target(Arc::new(State {
        entered: tokio::sync::Notify::new(),
        retired: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        received,
    }));
    let state = target.0.clone();
    let ingress = Arc::new(
        parrot_remote::ingress::Ingress::new(
            Arc::new(target),
            Arc::new(parrot_remote::registry::CallbackRegistry::new(4)),
        )
        .with_reorder_limits(4, 1, 4096)
        .unwrap(),
    );
    let (back, _rx) = connection();
    ingress.dispatch(frame(99, 1), &back, "source").await;
    tokio::time::timeout(Duration::from_secs(2), state.retired.notified())
        .await
        .unwrap();
    ingress.dispatch(frame(1, 2), &back, "source").await;
    receive(&mut rx, 1).await;
    assert!(!back.is_closed());
    ingress.close();
    ingress.wait_closed().await;
}

#[tokio::test]
async fn cancelled_queue_wait_returns_capacity_for_another_live_source() {
    let (received, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let state = Arc::new(State {
        entered: tokio::sync::Notify::new(),
        retired: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        received,
    });
    let ingress = Arc::new(
        parrot_remote::ingress::Ingress::new(
            Arc::new(Target(state.clone())),
            Arc::new(parrot_remote::registry::CallbackRegistry::new(4)),
        )
        .with_reorder_limits(4, 258, 1024 * 1024)
        .unwrap(),
    );
    let (back, _rx) = connection();
    ingress.dispatch(frame(0, 1), &back, "blocked").await;
    tokio::time::timeout(Duration::from_secs(2), state.entered.notified())
        .await
        .unwrap();
    for sequence in 2..258 {
        ingress.dispatch(frame(1, sequence), &back, "blocked").await;
    }
    let mut waiting = Box::pin(ingress.dispatch(frame(1, 258), &back, "blocked"));
    assert!(
        futures::poll!(waiting.as_mut()).is_pending(),
        "the real source queue must be full"
    );
    drop(waiting);
    let (other, _other_rx) = connection();
    ingress.dispatch(frame(2, 1), &other, "other").await;
    receive(&mut rx, 2).await;
    assert!(
        !other.is_closed(),
        "cancelled queue admission leaked its node frame charge"
    );
    ingress.close();
    ingress.wait_closed().await;
    // Closing dropped the actual blocked actor delivery and all queued frames.
    assert_eq!(state.release.available_permits(), 0);
    assert!(rx.try_recv().is_err());
}
