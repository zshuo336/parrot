use super::*;
use std::{any::Any, panic::AssertUnwindSafe, sync::atomic::AtomicUsize};

mod diagnostic_contracts;

#[derive(Default)]
struct Counts {
    references: AtomicUsize,
    stops: AtomicUsize,
    payloads: AtomicUsize,
    clones: AtomicUsize,
    locked: AtomicUsize,
    diagnostics: AtomicUsize,
    active: AtomicBool,
    probe_drop: AtomicBool,
    probe_clone: AtomicBool,
    clone_panic_at: AtomicUsize,
    gate: Mutex<
        Option<(
            std::sync::mpsc::SyncSender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
    scope: Mutex<Weak<ChildScope>>,
}
impl Counts {
    fn probe(&self) {
        let scope = self.scope.lock().unwrap().upgrade();
        if let Some(scope) = scope {
            let free = scope.children.try_lock().is_ok();
            if free {
                // Exercise actual registry reentry after observing the boundary.
                drop(scope.children.lock().unwrap());
            } else {
                self.locked.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("registry panic payload destructor");
    }
}
fn fail_drop(counts: &Arc<Counts>, kind: u8) {
    if counts.active.load(Ordering::SeqCst) {
        match kind {
            1 => panic!("registry input destructor"),
            2 => std::panic::panic_any(Recursive(counts.clone())),
            _ => {}
        }
    }
}
struct Reference {
    counts: Arc<Counts>,
    token: Arc<()>,
    kind: u8,
}
impl Drop for Reference {
    fn drop(&mut self) {
        self.counts.references.fetch_add(1, Ordering::SeqCst);
        if self.counts.active.load(Ordering::SeqCst)
            && self.counts.probe_drop.load(Ordering::SeqCst)
        {
            self.counts.probe();
        }
        fail_drop(&self.counts, self.kind);
    }
}
impl std::fmt::Debug for Reference {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.counts.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("registry must not format user reference");
    }
}
impl parrot_api::address::ActorRef for Reference {
    fn send(
        &self,
        _: parrot_api::types::BoxedMessage,
    ) -> BoxedFuture<'_, ActorResult<parrot_api::types::BoxedMessage>> {
        panic!("registry must not send through user reference");
    }
    fn send_with_timeout(
        &self,
        message: parrot_api::types::BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'_, ActorResult<parrot_api::types::BoxedMessage>> {
        self.send(message)
    }
    fn deliver(&self, _: parrot_api::types::BoxedMessage) -> BoxedFuture<'_, ActorResult<()>> {
        panic!("registry must not deliver through user reference");
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        panic!("registry must not stop through user reference");
    }
    fn path(&self) -> String {
        self.counts.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("registry must not format user path");
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        panic!("registry must not inspect user liveness");
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        if self.counts.active.load(Ordering::SeqCst) {
            let call = self.counts.clones.fetch_add(1, Ordering::SeqCst) + 1;
            if self.counts.probe_clone.load(Ordering::SeqCst) {
                self.counts.probe();
            }
            if self.counts.clone_panic_at.load(Ordering::SeqCst) == call {
                panic!("planned registry clone failure");
            }
            let gate = self.counts.gate.lock().unwrap().take();
            if let Some((entered, release)) = gate {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
        Box::new(Self {
            counts: self.counts.clone(),
            token: self.token.clone(),
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
        fail_drop(&self.0, self.1);
    }
}
fn dispose<T>(value: T) {
    let _ = crate::owned_payload::PublicationInput::new(value).retire();
}
fn reference(counts: &Arc<Counts>, token: &Arc<()>, kind: u8) -> BoxedActorRef {
    Box::new(Reference {
        counts: counts.clone(),
        token: token.clone(),
        kind,
    })
}
fn fixture() -> (Arc<ChildScope>, Arc<Counts>) {
    let scope = ChildScope::new(Duration::from_secs(2), Arc::new(Semaphore::new(1)));
    let counts = Arc::new(Counts::default());
    *counts.scope.lock().unwrap() = Arc::downgrade(&scope);
    (scope, counts)
}
fn admit(
    scope: &Arc<ChildScope>,
    counts: &Arc<Counts>,
    token: &Arc<()>,
    kind: u8,
    stop_kind: u8,
) -> Arc<ChildLifetime> {
    let mut lifetime = None;
    let capture = StopCapture(counts.clone(), stop_kind);
    let stop: StopChild = Arc::new(move || {
        let _ = &capture;
        panic!("registry must not invoke stop endpoint");
    });
    let result = scope.spawn(|child| {
        lifetime = Some(child);
        Ok((reference(counts, token, kind), stop))
    });
    assert!(result.is_ok());
    // The old private admission returned an unused clone. Its disposal is not
    // the registry-owned reference whose later cleanup these tests measure.
    dispose(result);
    lifetime.unwrap()
}
fn identity(view: &ReadOnlyChildrenVec, token: &Arc<()>) {
    let entries = view.read_all();
    assert_eq!(entries.len(), 1);
    let concrete = entries[0].as_any().downcast_ref::<Reference>().unwrap();
    assert!(Arc::ptr_eq(&concrete.token, token));
}
fn caught(work: impl FnOnce()) -> Option<String> {
    std::panic::catch_unwind(AssertUnwindSafe(work))
        .err()
        .map(crate::owned_payload::panic_message)
}

fn exercise(mode: u8, kind: u8) {
    let (scope, counts) = fixture();
    let token = Arc::new(());
    if matches!(mode, 1 | 2) {
        let child = ChildLifetime::new(Arc::downgrade(&scope));
        if mode == 2 {
            drop(scope);
        }
        counts.active.store(true, Ordering::SeqCst);
        let escaped = caught(|| child.publish_reference(reference(&counts, &token, kind)));
        assert!(
            escaped.is_none(),
            "rejected registry input escaped: {escaped:?}"
        );
        assert_eq!(counts.references.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.payloads.load(Ordering::SeqCst),
            usize::from(kind == 2)
        );
        return;
    }
    if mode == 6 {
        counts.active.store(true, Ordering::SeqCst);
        counts.probe_clone.store(true, Ordering::SeqCst);
    }
    let lifetime = admit(
        &scope,
        &counts,
        &token,
        kind,
        if mode == 3 { kind } else { 0 },
    );
    if matches!(mode, 3 | 5) {
        admit(
            &scope,
            &counts,
            &token,
            kind,
            if mode == 3 { kind } else { 0 },
        );
    }
    let before = counts.references.load(Ordering::SeqCst);
    let payloads = counts.payloads.load(Ordering::SeqCst);
    counts.active.store(true, Ordering::SeqCst);
    if mode == 0 {
        counts.probe_drop.store(true, Ordering::SeqCst);
        let next = Arc::new(Counts::default());
        let next_token = Arc::new(());
        let escaped = caught(|| lifetime.publish_reference(reference(&next, &next_token, 0)));
        assert!(escaped.is_none(), "replacement escaped: {escaped:?}");
        let view = scope.view();
        identity(&view, &next_token);
        drop(view);
        drop(lifetime.finish(Ok(())));
        assert_eq!(counts.references.load(Ordering::SeqCst) - before, 1);
        assert_eq!(next.references.load(Ordering::SeqCst), 2);
    } else if mode == 3 {
        assert!(caught(|| drop(scope)).is_none(), "scope inputs escaped");
        assert_eq!(counts.references.load(Ordering::SeqCst) - before, 2);
        assert_eq!(counts.stops.load(Ordering::SeqCst), 2);
        assert_eq!(
            counts.payloads.load(Ordering::SeqCst) - payloads,
            if kind == 2 { 4 } else { 0 }
        );
        return;
    } else if mode == 4 {
        counts.probe_clone.store(true, Ordering::SeqCst);
        let view = scope.view();
        identity(&view, &token);
        dispose(view);
        drop(lifetime.finish(Ok(())));
        assert_eq!(counts.references.load(Ordering::SeqCst) - before, 2);
    } else if mode == 5 {
        counts.clone_panic_at.store(2, Ordering::SeqCst);
        let escaped = caught(|| dispose(scope.view()));
        assert_eq!(escaped.as_deref(), Some("planned registry clone failure"));
        assert!(
            !scope.children.is_poisoned(),
            "clone callback poisoned topology"
        );
        drop(scope);
        assert_eq!(counts.references.load(Ordering::SeqCst) - before, 3);
        assert_eq!(
            counts.payloads.load(Ordering::SeqCst) - payloads,
            if kind == 2 { 3 } else { 0 }
        );
        return;
    } else {
        drop(lifetime.finish(Ok(())));
        assert_eq!(counts.references.load(Ordering::SeqCst) - before, 1);
    }
    assert_eq!(
        counts.locked.load(Ordering::SeqCst),
        0,
        "user code ran under topology lock"
    );
    assert_eq!(counts.diagnostics.load(Ordering::SeqCst), 0);
    assert_eq!(counts.stops.load(Ordering::SeqCst), 1);
    assert_eq!(scope.slots.available_permits(), MAX_CHILDREN);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst) - payloads,
        if kind == 2 {
            if mode == 4 { 2 } else { 1 }
        } else {
            0
        }
    );
}

struct Recipient(Arc<()>, Arc<AtomicUsize>);
impl ChildRecipient for Recipient {
    fn deliver(&self, event: ChildTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        let actor = event.into_actor();
        let concrete = actor.as_any().downcast_ref::<Reference>().unwrap();
        assert!(Arc::ptr_eq(&concrete.token, &self.0));
        self.1.fetch_add(1, Ordering::SeqCst);
        dispose(actor);
        Box::pin(async { Ok(()) })
    }
}
fn concurrent(mode: u8, kind: u8) {
    let (scope, counts) = fixture();
    let token = Arc::new(());
    let kind = if mode == 9 { 0 } else { kind };
    let lifetime = admit(&scope, &counts, &token, kind, 0);
    let before = counts.references.load(Ordering::SeqCst);
    let deliveries = Arc::new(AtomicUsize::new(0));
    if mode == 9 {
        scope.bind(Arc::new(Recipient(token.clone(), deliveries.clone())));
    }
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    *counts.gate.lock().unwrap() = Some((entered_tx, release_rx));
    counts.active.store(true, Ordering::SeqCst);
    let reader = scope.clone();
    let view = std::thread::spawn(move || dispose(reader.view()));
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let next = Arc::new(Counts::default());
    let next_token = Arc::new(());
    let replacement = reference(&next, &next_token, 0);
    let writer = lifetime.clone();
    let write = std::thread::spawn(move || {
        if mode == 8 {
            writer.publish_reference(replacement);
        } else if mode == 9 {
            dispose(replacement);
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(writer.finish(Ok(())));
        } else {
            dispose(replacement);
            drop(writer.finish(Ok(())));
        }
        done_tx.send(()).unwrap();
    });
    let completed = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let held = scope.slots.available_permits();
    let retained = counts.references.load(Ordering::SeqCst) - before;
    let delivered = deliveries.load(Ordering::SeqCst);
    release_tx.send(()).unwrap();
    view.join().unwrap();
    write.join().unwrap();
    assert!(
        completed,
        "registry operation waited for user clone callback"
    );
    assert_eq!(
        held,
        MAX_CHILDREN - 1,
        "snapshot owner lost input cleanup capacity"
    );
    assert_eq!(retained, usize::from(mode == 9));
    assert_eq!(delivered, usize::from(mode == 9));
    if mode == 8 {
        let view = scope.view();
        identity(&view, &next_token);
        drop(view);
        drop(lifetime.finish(Ok(())));
    }
    assert_eq!(scope.slots.available_permits(), MAX_CHILDREN);
    assert_eq!(
        counts.references.load(Ordering::SeqCst) - before,
        if mode == 9 { 3 } else { 2 }
    );
    assert_eq!(counts.diagnostics.load(Ordering::SeqCst), 0);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        if kind == 2 { 2 } else { 0 }
    );
}
async fn isolated(name: &str, mode: u8) {
    const KEY: &str = "PARROT_CHILD_REGISTRY_INPUT_KIND";
    if let Ok(kind) = std::env::var(KEY) {
        let kind = kind.parse().unwrap();
        if mode < 7 {
            exercise(mode, kind);
        } else {
            concurrent(mode, kind);
        }
        return;
    }
    for kind in 0..3 {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture"])
            .env(KEY, kind.to_string())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .expect("registry worker stalled")
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
macro_rules! contract {
    ($name:ident, $mode:expr) => {
        #[tokio::test]
        async fn $name() {
            isolated(
                concat!("children::registry_contracts::", stringify!($name)),
                $mode,
            )
            .await;
        }
    };
}
contract!(replacement_retires_displaced_input_outside_registry, 0);
contract!(missing_member_retires_rejected_reference, 1);
contract!(missing_parent_retires_rejected_reference, 2);
contract!(scope_drop_isolates_each_registered_input, 3);
contract!(view_clones_user_references_outside_registry, 4);
contract!(failed_view_clone_preserves_registry_and_partial_inputs, 5);
contract!(admission_does_not_clone_under_registry_lock, 6);
contract!(retirement_does_not_wait_for_view_clone, 7);
contract!(replacement_does_not_wait_for_view_clone, 8);
contract!(notification_preserves_identity_during_view_clone, 9);

#[tokio::test]
async fn healthy_identity_and_lifecycle_remain_valid() {
    let (scope, counts) = fixture();
    let token = Arc::new(());
    let lifetime = admit(&scope, &counts, &token, 0, 0);
    identity(&scope.view(), &token);
    lifetime.finish(Ok(())).await;
    lifetime.wait().await.unwrap();
    assert!(scope.view().is_empty());
    assert_eq!(scope.slots.available_permits(), MAX_CHILDREN);
}
