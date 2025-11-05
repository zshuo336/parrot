use parrot_api::message::RemoteMessage;
use parrot_remote::{
    frame::Frame, transport::FrameSender, LocalLookup, RemoteActorSystem, RemoteConfig,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Tell(u64);
impl parrot_api::Message for Tell {
    type Result = ();
}

#[derive(Default)]
struct Lookup {
    entered: tokio::sync::Notify,
    active: AtomicUsize,
    calls: AtomicUsize,
    release: tokio::sync::Notify,
}
struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        self.active.fetch_add(1, Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _active = Active(&self.active);
        self.entered.notify_one();
        self.release.notified().await;
        None
    }
}
fn frame(seq: u32) -> Frame {
    Frame::tell_with_seq(
        "/test",
        Tell::TYPE_KEY,
        parrot_api::message::serde_remote_serialize(&Tell(1))
            .unwrap()
            .into(),
        seq,
    )
}

async fn check_close(explicit: bool) {
    let lookup = Arc::new(Lookup::default());
    let sys = RemoteActorSystem::new(
        RemoteConfig::mem(if explicit {
            "tell-explicit"
        } else {
            "tell-drop"
        }),
        lookup.clone(),
    )
    .unwrap();
    sys.start().await.unwrap();
    let ingress = sys.ingress.clone();
    let back = FrameSender::detached();
    ingress.dispatch(frame(1), &back, "source").await;
    tokio::time::timeout(Duration::from_secs(2), lookup.entered.notified())
        .await
        .expect("valid sequenced TELL must reach lookup");
    assert_eq!(lookup.active.load(Ordering::SeqCst), 1);
    let weak = Arc::downgrade(&sys);
    if explicit {
        sys.shutdown().await.unwrap();
    }
    drop(sys);
    let drained = tokio::time::timeout(Duration::from_secs(2), async {
        while lookup.active.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    if !drained {
        lookup.release.notify_one();
    }
    if let Some(leaked) = weak.upgrade() {
        leaked.shutdown().await.unwrap();
    }
    assert!(
        drained,
        "system close must drop a reorder worker's blocked lookup future"
    );
    // Retaining the public ingress must not reopen admission after system close.
    ingress.dispatch(frame(2), &back, "source").await;
    let immediate = tokio::time::timeout(
        Duration::from_millis(200),
        ingress.dispatch(frame(0), &back, "source"),
    )
    .await;
    assert!(
        immediate.is_ok(),
        "closed ingress admitted an unsequenced TELL"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), lookup.entered.notified())
            .await
            .is_err()
    );
    assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn shutdown_cancels_and_joins_blocked_tell_reorder_work() {
    check_close(true).await;
}
#[tokio::test]
async fn drop_cancels_blocked_tell_even_with_ingress_retained() {
    check_close(false).await;
}
