use super::*;
use std::{any::Any, panic::AssertUnwindSafe, sync::atomic::AtomicUsize};

#[derive(Default)]
struct Counts {
    references: AtomicUsize,
    stops: AtomicUsize,
    payloads: AtomicUsize,
    clones: AtomicUsize,
    deliveries: AtomicUsize,
    diagnostics: AtomicUsize,
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("notification panic payload destructor");
    }
}
struct Reference {
    counts: Arc<Counts>,
    registered: bool,
    kind: u8,
}
impl std::fmt::Debug for Reference {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.counts.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("notification must not format user reference");
    }
}
impl Drop for Reference {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        self.counts.references.fetch_add(1, Ordering::SeqCst);
        match self.kind {
            1 => panic!("notification reference destructor"),
            3 => std::panic::panic_any(Recursive(self.counts.clone())),
            _ => {}
        }
    }
}
impl parrot_api::address::ActorRef for Reference {
    fn send(
        &self,
        _: parrot_api::types::BoxedMessage,
    ) -> BoxedFuture<'_, ActorResult<parrot_api::types::BoxedMessage>> {
        panic!("notification must not send through child reference");
    }
    fn send_with_timeout(
        &self,
        message: parrot_api::types::BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'_, ActorResult<parrot_api::types::BoxedMessage>> {
        self.send(message)
    }
    fn deliver(&self, _: parrot_api::types::BoxedMessage) -> BoxedFuture<'_, ActorResult<()>> {
        panic!("notification must not deliver through child reference");
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        panic!("notification must not stop through child reference");
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        panic!("notification must not inspect child liveness");
    }
    fn path(&self) -> String {
        self.counts.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("notification must not format child path");
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        self.counts.clones.fetch_add(1, Ordering::SeqCst);
        Box::new(Self {
            counts: self.counts.clone(),
            registered: true,
            kind: self.kind,
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct StopCapture(Arc<Counts>, u8);
impl Drop for StopCapture {
    fn drop(&mut self) {
        self.0.stops.fetch_add(1, Ordering::SeqCst);
        match self.1 {
            2 => panic!("notification stop capture destructor"),
            3 => std::panic::panic_any(Recursive(self.0.clone())),
            _ => {}
        }
    }
}
struct Recipient(Arc<Counts>, u8);
impl ChildRecipient for Recipient {
    fn deliver(&self, event: ChildTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        self.0.deliveries.fetch_add(1, Ordering::SeqCst);
        let future = Box::pin(async move {
            if matches!(self.1, 4 | 5) {
                std::future::pending::<()>().await;
            }
            drop(event);
            Ok(())
        });
        if self.1 == 3 {
            drop(future);
            Box::pin(async { Ok(()) })
        } else {
            future
        }
    }
}

async fn exercise(mode: u8, kind: u8) {
    let counts = Arc::new(Counts::default());
    let scope = ChildScope::new(Duration::from_millis(20), Arc::new(Semaphore::new(1)));
    if mode >= 3 {
        scope.bind(Arc::new(Recipient(counts.clone(), mode)));
    }
    let mut lifetime = None;
    let capture = StopCapture(counts.clone(), kind);
    let stop: StopChild = Arc::new(move || {
        let _ = &capture;
        panic!("notification must not invoke stop closure");
    });
    let admission = scope.spawn(|child| {
        lifetime = Some(child);
        Ok((
            Box::new(Reference {
                counts: counts.clone(),
                registered: true,
                kind,
            }),
            stop,
        ))
    });
    assert!(admission.is_ok());
    let _ = crate::owned_payload::PublicationInput::new(admission).retire();
    let references_before = counts.references.load(Ordering::SeqCst);
    let payloads_before = counts.payloads.load(Ordering::SeqCst);
    let clones_before = counts.clones.load(Ordering::SeqCst);
    let lifetime = lifetime.unwrap();
    let stop_alias = (mode == 7).then(|| {
        scope
            .children
            .lock()
            .unwrap()
            .get(&lifetime.id)
            .unwrap()
            .stop
            .clone()
    });
    if mode == 1 {
        scope.close();
    }
    let future = lifetime.finish(Ok(()));
    assert!(scope.children.lock().unwrap().is_empty());
    assert_eq!(scope.slots.available_permits(), MAX_CHILDREN - 1);
    lifetime.wait().await.unwrap();
    let escaped = AssertUnwindSafe(async {
        if mode == 0 {
            drop(future);
        } else if mode == 5 {
            let mut future = future;
            assert!(futures::poll!(&mut future).is_pending());
            scope.close();
            future.await;
        } else {
            future.await;
        }
    })
    .catch_unwind()
    .await
    .err()
    .map(crate::owned_payload::panic_message);
    if mode == 7 {
        assert_eq!(
            counts.stops.load(Ordering::SeqCst),
            0,
            "a live stop alias lost its captures"
        );
    }
    let alias_escaped = std::panic::catch_unwind(AssertUnwindSafe(|| drop(stop_alias)))
        .err()
        .map(crate::owned_payload::panic_message);
    assert!(escaped.is_none(), "notification input escaped: {escaped:?}");
    assert!(
        alias_escaped.is_none(),
        "stop alias input escaped: {alias_escaped:?}"
    );
    assert_eq!(
        counts.references.load(Ordering::SeqCst),
        references_before + 1
    );
    assert_eq!(counts.stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        payloads_before + if kind == 3 { 2 } else { 0 }
    );
    assert_eq!(counts.clones.load(Ordering::SeqCst), clones_before);
    assert_eq!(counts.diagnostics.load(Ordering::SeqCst), 0);
    assert_eq!(
        counts.deliveries.load(Ordering::SeqCst),
        usize::from(mode >= 3)
    );
    assert_eq!(scope.slots.available_permits(), MAX_CHILDREN);
    scope.join().await.unwrap();
}

async fn isolated(name: &str, mode: u8) {
    const KEY: &str = "PARROT_CHILD_NOTIFICATION_INPUT_KIND";
    if let Ok(kind) = std::env::var(KEY) {
        exercise(mode, kind.parse().unwrap()).await;
        return;
    }
    for kind in 0..4 {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture"])
            .env(KEY, kind.to_string())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .expect("notification contract child stalled")
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let executed = stdout
            .lines()
            .filter(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"))
            .count();
        assert!(
            output.status.success() && executed == 1,
            "mode={mode} kind={kind} status={}\n{}\n{}",
            output.status,
            stdout,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn unpolled_finish_owns_notification_inputs() {
    isolated(
        "children::notification_contracts::unpolled_finish_owns_notification_inputs",
        0,
    )
    .await;
}
#[tokio::test]
async fn closed_parent_retires_notification_inputs() {
    isolated(
        "children::notification_contracts::closed_parent_retires_notification_inputs",
        1,
    )
    .await;
}
#[tokio::test]
async fn absent_recipient_retires_notification_inputs() {
    isolated(
        "children::notification_contracts::absent_recipient_retires_notification_inputs",
        2,
    )
    .await;
}
#[tokio::test]
async fn recipient_drops_unpolled_delivery_inputs() {
    isolated(
        "children::notification_contracts::recipient_drops_unpolled_delivery_inputs",
        3,
    )
    .await;
}
#[tokio::test]
async fn timed_out_recipient_retires_notification_inputs() {
    isolated(
        "children::notification_contracts::timed_out_recipient_retires_notification_inputs",
        4,
    )
    .await;
}
#[tokio::test]
async fn parent_close_cancels_inflight_notification_inputs() {
    isolated(
        "children::notification_contracts::parent_close_cancels_inflight_notification_inputs",
        5,
    )
    .await;
}
#[tokio::test]
async fn successful_delivery_retires_notification_inputs() {
    isolated(
        "children::notification_contracts::successful_delivery_retires_notification_inputs",
        6,
    )
    .await;
}

#[tokio::test]
async fn last_stop_alias_retires_captures_after_notification() {
    isolated(
        "children::notification_contracts::last_stop_alias_retires_captures_after_notification",
        7,
    )
    .await;
}

#[tokio::test]
async fn notification_capacity_is_held_until_capture_disposal_finishes() {
    struct Blocking {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl Drop for Blocking {
        fn drop(&mut self) {
            self.entered.send(()).unwrap();
            self.release
                .get_mut()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
        }
    }
    let counts = Arc::new(Counts::default());
    let scope = ChildScope::new(Duration::from_secs(1), Arc::new(Semaphore::new(1)));
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let capture = Blocking {
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    };
    let stop: StopChild = Arc::new(move || {
        let _ = &capture;
        panic!("notification must not invoke stop");
    });
    let mut lifetime = None;
    let admission = scope.spawn(|child| {
        lifetime = Some(child);
        Ok((
            Box::new(Reference {
                counts: counts.clone(),
                registered: true,
                kind: 0,
            }),
            stop,
        ))
    });
    assert!(admission.is_ok());
    let _ = crate::owned_payload::PublicationInput::new(admission).retire();
    let references_before = counts.references.load(Ordering::SeqCst);
    let future = lifetime.unwrap().finish(Ok(()));
    let worker = std::thread::spawn(move || drop(future));
    let entered = entered_rx.recv_timeout(Duration::from_secs(3));
    let available_during_disposal = scope.slots.available_permits();
    let released = release_tx.send(());
    worker.join().unwrap();
    entered.unwrap();
    released.unwrap();
    assert_eq!(available_during_disposal, MAX_CHILDREN - 1);
    assert_eq!(scope.slots.available_permits(), MAX_CHILDREN);
    assert_eq!(
        counts.references.load(Ordering::SeqCst),
        references_before + 1
    );
}
