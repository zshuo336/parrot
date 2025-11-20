use super::*;
use crate::thread::system::{ErasedSpawnable, TypedSpawnPayload};
use std::{any::Any, sync::atomic::AtomicUsize, time::Duration};

#[derive(Default)]
struct Counts {
    clones: AtomicUsize,
    drops: AtomicUsize,
    payloads: AtomicUsize,
    clone_kind: u8,
    drop_kind: u8,
    gate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    lifetime: Mutex<Option<Arc<crate::children::ChildLifetime>>>,
    finished: Mutex<Option<watch::Receiver<Option<StopOutcome>>>>,
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("reference panic payload destructor");
    }
}
fn fail(counts: &Arc<Counts>, kind: u8, message: &'static str) {
    match kind {
        1 => panic!("{message}"),
        2 => std::panic::panic_any(Recursive(counts.clone())),
        _ => {}
    }
}
struct Reference {
    inner: Arc<dyn ActorRef>,
    counts: Arc<Counts>,
}
impl std::fmt::Debug for Reference {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("creation must not format the user reference");
    }
}
impl Drop for Reference {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        fail(
            &self.counts,
            self.counts.drop_kind,
            "creation reference destructor",
        );
    }
}
impl ActorRef for Reference {
    fn send(&self, message: BoxedMessage) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        self.inner.send(message)
    }
    fn send_with_timeout(
        &self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        self.inner.send_with_timeout(message, timeout)
    }
    fn deliver(&self, message: BoxedMessage) -> BoxedFuture<'_, ActorResult<()>> {
        self.inner.deliver(message)
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        self.inner.stop()
    }
    fn path(&self) -> String {
        self.inner.path()
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        self.inner.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        self.counts.clones.fetch_add(1, Ordering::SeqCst);
        let gate = self.counts.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        fail(
            &self.counts,
            self.counts.clone_kind,
            "creation reference clone",
        );
        Box::new(Self {
            inner: self.inner.clone(),
            counts: self.counts.clone(),
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
    fn handle_child_terminated<'a>(
        &'a mut self,
        child: BoxedActorRef,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let _ = crate::owned_payload::PublicationInput::new(child).retire();
        Box::pin(async { Ok(()) })
    }
}
struct Bridge(Arc<Counts>);
#[async_trait::async_trait]
impl ErasedSpawnable for Bridge {
    fn accepts_child_config(&self, config: &BoxedMessage) -> bool {
        config.is::<EmptyConfig>()
    }
    fn spawn_child_on(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        engine: ThreadActorConfig,
        parent: BoxedActorRef,
        lifetime: ChildSpawnReceipt,
    ) -> ActorResult<BoxedActorRef> {
        *self.0.lifetime.lock().unwrap() = Some(lifetime.0.lifetime.clone());
        *self.0.finished.lock().unwrap() = Some(lifetime.0.finished.subscribe());
        let inner = Box::new(TypedSpawnPayload::new(Echo))
            .spawn_child_on(system, engine, parent, lifetime)?;
        Ok(Box::new(Reference {
            inner: Arc::from(inner),
            counts: self.0.clone(),
        }))
    }
    async fn spawn_on(
        self: Box<Self>,
        _: Arc<ThreadActorSystem>,
        _: BoxedMessage,
        _: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        Err(ActorError::Stopped)
    }
}
async fn setup(
    timeout: Duration,
) -> (
    Arc<ThreadActorSystem>,
    ThreadActorRef<Echo>,
    Arc<crate::children::ChildScope>,
) {
    let system = ThreadActorSystem::shared(crate::thread::config::ThreadActorSystemConfig {
        shutdown_timeout: timeout,
        ..Default::default()
    });
    let parent = system
        .spawn_root_typed_thread(Echo, EmptyConfig)
        .await
        .unwrap();
    let scope = match system.registry.read().unwrap().get(&parent.path()).unwrap() {
        ActorRegistryEntry::Dynamic(entry) => entry.children.clone(),
        _ => panic!("parent not published"),
    };
    (system, parent, scope)
}
async fn create(
    system: &Arc<ThreadActorSystem>,
    parent: &ThreadActorRef<Echo>,
    scope: &Arc<crate::children::ChildScope>,
    counts: Arc<Counts>,
) -> ActorResult<BoxedActorRef> {
    system
        .spawn_child_erased(
            Box::new(ErasedSpawnBox(Box::new(Bridge(counts)))),
            Box::new(EmptyConfig),
            None,
            parent.clone_boxed(),
            scope,
        )
        .await
}
async fn healthy_parent(system: &Arc<ThreadActorSystem>, parent: &ThreadActorRef<Echo>) {
    let reply = parent.send(Box::new(42_u64)).await.unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 42);
    system.shutdown_internal().await.unwrap();
}
async fn cleanup(counts: &Counts, drops: usize, payloads: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while counts.drops.load(Ordering::SeqCst) != drops
            || counts.payloads.load(Ordering::SeqCst) != payloads
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reference or recursive payload cleanup did not finish");
}
async fn clone_failure(clone_kind: u8, drop_kind: u8) {
    let (system, parent, scope) = setup(Duration::from_secs(2)).await;
    let counts = Arc::new(Counts {
        clone_kind,
        drop_kind,
        ..Default::default()
    });
    let result = create(&system, &parent, &scope, counts.clone()).await;
    let message = match result {
        Err(ActorError::Panic(message)) => message,
        Err(error) => panic!("clone failure lost its category: {error}"),
        Ok(reference) => {
            let _ = crate::owned_payload::PublicationInput::new(reference).retire();
            panic!("clone failure was published as success");
        }
    };
    assert!(
        message.contains(if clone_kind == 1 {
            "creation reference clone"
        } else {
            "non-string panic payload"
        }),
        "{message}"
    );
    assert!(!message.contains("lost its receipt"));
    if drop_kind == 1 {
        assert!(
            message.contains("creation reference destructor"),
            "{message}"
        );
    } else if drop_kind == 2 {
        assert_eq!(
            message.matches("panic payload destructor panicked").count(),
            2,
            "both clone and reference cleanup failures must survive: {message}"
        );
    }
    let lifetime = counts.lifetime.lock().unwrap().clone().unwrap();
    let retired = tokio::time::timeout(Duration::from_secs(2), lifetime.wait())
        .await
        .expect("publication barrier leaked");
    assert!(retired.is_err());
    assert_eq!(
        system.actor_count(),
        1,
        "failed creation left the real child alive"
    );
    assert!(
        scope.view().is_empty(),
        "failed child stayed in parent tree"
    );
    assert_eq!(counts.clones.load(Ordering::SeqCst), 1);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        usize::from(clone_kind == 2) + usize::from(drop_kind == 2)
    );
    healthy_parent(&system, &parent).await;
}
async fn blocked_clone(cancel: bool, drop_kind: u8) {
    let (system, parent, scope) = setup(Duration::from_millis(200)).await;
    let (entered, observed) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    let counts = Arc::new(Counts {
        drop_kind,
        gate: Mutex::new(Some((entered, held))),
        ..Default::default()
    });
    let (visible, ready) = oneshot::channel();
    let (receipt, receipt_seen) = std::sync::mpsc::channel();
    // An independent watchdog always releases the worker, including when the
    // old implementation blocks the only async executor thread.
    let observer = std::thread::spawn(move || {
        observed.recv_timeout(Duration::from_secs(3)).unwrap();
        let _ = visible.send(());
        let responsive = receipt_seen.recv_timeout(Duration::from_secs(4)).is_ok();
        release.send(()).unwrap();
        responsive
    });
    let owner = system.clone();
    let ancestor = parent.clone();
    let children = scope.clone();
    let input = counts.clone();
    let caller = tokio::spawn(async move { create(&owner, &ancestor, &children, input).await });
    ready.await.unwrap();
    let result = if cancel {
        caller.abort();
        match caller.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(result) => {
                let _ = crate::owned_payload::PublicationInput::new(result).retire();
                panic!("creation completed before caller could cancel a blocked clone");
            }
        }
        None
    } else {
        Some(caller.await.unwrap())
    };
    if cancel {
        // Native actor cleanup may remove registration before the creation
        // worker returns. Ownership is the parent entry and publication receipt.
        tokio::time::timeout(Duration::from_secs(2), async {
            while system.actor_count() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled native actor did not finish cleanup while clone was blocked");
    }
    let still_held = scope.view().len();
    let lifetime = counts.lifetime.lock().unwrap().clone().unwrap();
    let still_pending = matches!(
        lifetime.retirement_status(),
        parrot_api::address::RetirementStatus::Pending
    );
    let _ = receipt.send(());
    let responsive = observer.join().unwrap();
    let timed_out = match result {
        None => true,
        Some(Err(ActorError::TimeoutDetail(_))) => true,
        Some(Err(_)) => false,
        Some(Ok(reference)) => {
            let _ = crate::owned_payload::PublicationInput::new(reference).retire();
            false
        }
    };
    let retirement = tokio::time::timeout(Duration::from_secs(3), lifetime.wait())
        .await
        .expect("cancelled publication never retired");
    cleanup(&counts, 2, if drop_kind == 2 { 2 } else { 0 }).await;
    // Cleanup panics are failures, while a clean creation cancellation is not
    // an actor retirement failure.
    assert_eq!(retirement.is_err(), drop_kind != 0);
    assert!(
        responsive,
        "user clone blocked creation deadline/cancellation"
    );
    assert!(
        timed_out,
        "blocked clone escaped the real creation deadline"
    );
    assert_eq!(
        still_held, 1,
        "unfinished creation released parent ownership"
    );
    assert!(
        still_pending,
        "unfinished creation published a final retirement receipt"
    );
    assert!(scope.view().is_empty());
    assert_eq!(system.actor_count(), 1);
    assert_eq!(counts.clones.load(Ordering::SeqCst), 1);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 2);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        if drop_kind == 2 { 2 } else { 0 }
    );
    healthy_parent(&system, &parent).await;
}
async fn unobserved_success() {
    use std::{future::Future, panic::AssertUnwindSafe, task::Poll};
    let (system, parent, scope) = setup(Duration::from_secs(2)).await;
    let counts = Arc::new(Counts {
        drop_kind: 2,
        ..Default::default()
    });
    let mut creation = Box::pin(create(&system, &parent, &scope, counts.clone()));
    std::future::poll_fn(|cx| {
        assert!(creation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let mut finished = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(finished) = counts.finished.lock().unwrap().clone() {
                break finished;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if finished.borrow_and_update().is_some() {
                break;
            }
            finished.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(matches!(*finished.borrow(), Some(StopOutcome::Stopped)));
    let escaped = std::panic::catch_unwind(AssertUnwindSafe(|| drop(creation)))
        .err()
        .map(crate::owned_payload::panic_message);
    assert!(
        escaped.is_none(),
        "queued creation result escaped on cancellation: {escaped:?}"
    );
    let lifetime = counts.lifetime.lock().unwrap().clone().unwrap();
    tokio::time::timeout(Duration::from_secs(3), lifetime.wait())
        .await
        .unwrap()
        .unwrap();
    cleanup(&counts, 2, 2).await;
    assert_eq!(counts.clones.load(Ordering::SeqCst), 1);
    assert!(scope.view().is_empty());
    assert_eq!(system.actor_count(), 1);
    healthy_parent(&system, &parent).await;
}
async fn isolated(name: &str, mode: u8) {
    const KEY: &str = "PARROT_CHILD_CREATION_REFERENCE_WORKER";
    if std::env::var_os(KEY).is_some() {
        match mode {
            0 => clone_failure(1, 0).await,
            1 => clone_failure(2, 0).await,
            2 => clone_failure(1, 1).await,
            3 => clone_failure(2, 2).await,
            4 => blocked_clone(false, 0).await,
            5 => blocked_clone(true, 0).await,
            6 => blocked_clone(false, 2).await,
            7 => blocked_clone(true, 2).await,
            8 => unobserved_success().await,
            _ => unreachable!(),
        }
        return;
    }
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(KEY, "1")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("creation reference worker stalled")
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let executed = stdout
        .lines()
        .filter(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"))
        .count();
    assert!(
        output.status.success() && executed == 1,
        "worker={}\n{}\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}
macro_rules! contract {
    ($name:ident, $mode:expr) => {
        #[tokio::test]
        async fn $name() {
            isolated(
                concat!(
                    "thread::child_instance::reference_contracts::",
                    stringify!($name)
                ),
                $mode,
            )
            .await;
        }
    };
}
contract!(clone_panic_retires_published_child, 0);
contract!(recursive_clone_panic_retires_published_child, 1);
contract!(clone_and_reference_drop_panic_are_isolated, 2);
contract!(recursive_clone_and_reference_drop_panic_are_isolated, 3);
contract!(blocked_clone_preserves_real_creation_deadline, 4);
contract!(blocked_clone_does_not_block_caller_cancellation, 5);
contract!(deadline_refusal_preserves_reference_cleanup_failure, 6);
contract!(cancelled_creation_preserves_reference_cleanup_failure, 7);
contract!(unobserved_success_protects_queued_reference, 8);

#[tokio::test]
async fn healthy_bridge_preserves_reference_type_identity_and_messages() {
    let (system, parent, scope) = setup(Duration::from_secs(2)).await;
    let counts = Arc::new(Counts::default());
    let child = create(&system, &parent, &scope, counts.clone())
        .await
        .unwrap();
    let concrete = child.as_any().downcast_ref::<Reference>().unwrap();
    assert!(Arc::ptr_eq(&concrete.counts, &counts));
    let view = scope.view();
    let snapshot = view.get(0).unwrap();
    assert!(Arc::ptr_eq(
        &snapshot
            .as_any()
            .downcast_ref::<Reference>()
            .unwrap()
            .counts,
        &counts
    ));
    assert_eq!(
        *child
            .send(Box::new(7_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        7
    );
    drop(snapshot);
    drop(view);
    drop(child);
    healthy_parent(&system, &parent).await;
}
