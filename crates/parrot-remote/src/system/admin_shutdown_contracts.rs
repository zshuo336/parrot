use super::*;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Weak,
    },
    task::{Context, Wake, Waker},
};

struct Probe {
    system: Weak<RemoteActorSystem>,
    calls: AtomicUsize,
    locked: AtomicBool,
    rejected: AtomicUsize,
    reentered: AtomicUsize,
}
impl Wake for Probe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let system = self.system.upgrade().unwrap();
        let unlocked = system.link_gate.try_lock().is_ok();
        self.locked.fetch_or(!unlocked, Ordering::SeqCst);
        if unlocked {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            self.rejected.fetch_add(
                usize::from(system.admin_pending.insert(42, tx).is_err()),
                Ordering::SeqCst,
            );
            let (tx, _rx) = tokio::sync::oneshot::channel();
            self.rejected.fetch_add(
                usize::from(system.admin_pending_v2.insert(42, tx).is_err()),
                Ordering::SeqCst,
            );
            system.begin_close();
            self.reentered.fetch_add(1, Ordering::SeqCst);
        }
    }
}
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
fn close<R>(system: &Arc<RemoteActorSystem>, rx: &mut tokio::sync::oneshot::Receiver<R>) {
    let probe = Arc::new(Probe {
        system: Arc::downgrade(system),
        calls: 0.into(),
        locked: false.into(),
        rejected: 0.into(),
        reentered: 0.into(),
    });
    assert!(Pin::new(rx)
        .poll(&mut Context::from_waker(&Waker::from(probe.clone())))
        .is_pending());
    system.begin_close();
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert!(
        !probe.locked.load(Ordering::SeqCst),
        "administrative shutdown wake held connection gate"
    );
    assert_eq!(
        probe.rejected.load(Ordering::SeqCst),
        2,
        "administrative shutdown woke before fencing both protocol versions"
    );
    assert_eq!(probe.reentered.load(Ordering::SeqCst), 1);
    assert!(system.admin_pending.is_empty());
    assert!(system.admin_pending_v2.is_empty());
}
#[tokio::test]
async fn shutdown_fences_both_admin_versions_before_waking_v1() {
    let system =
        RemoteActorSystem::new(RemoteConfig::tcp("admin-close-v1", None), Arc::new(Empty)).unwrap();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    system.admin_pending.insert(1, tx).unwrap();
    close(&system, &mut rx);
    assert!(
        matches!(rx.try_recv().unwrap(), crate::admin::AdminReply::Failed { req_id: 1, code, detail }
        if code == crate::ErrCode::ConnectionLost as u16 && detail == "system shutdown")
    );
    system.shutdown().await.unwrap();
}
#[tokio::test]
async fn shutdown_fences_both_admin_versions_before_waking_v2() {
    let system =
        RemoteActorSystem::new(RemoteConfig::tcp("admin-close-v2", None), Arc::new(Empty)).unwrap();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    system.admin_pending_v2.insert(1, tx).unwrap();
    close(&system, &mut rx);
    assert!(
        matches!(rx.try_recv().unwrap(), crate::admin_v2::AdminReplyV2::Failed { req_id: 1, code, detail }
        if code == crate::ErrCode::ConnectionLost as u16 && detail == "system shutdown")
    );
    system.shutdown().await.unwrap();
}
