use super::*;
use crate::thread::system::{ErasedSpawnable, TypedSpawnPayload};
use std::{fmt, sync::atomic::AtomicUsize, time::Duration};

#[derive(Default)]
struct Counts {
    display: AtomicUsize,
    debug: AtomicUsize,
    source: AtomicUsize,
    drops: AtomicUsize,
    payloads: AtomicUsize,
    actor_drops: AtomicUsize,
    policy_drops: AtomicUsize,
    gate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    lifetime: Mutex<Option<Arc<crate::children::ChildLifetime>>>,
    finished: Mutex<Option<watch::Receiver<Option<StopOutcome>>>>,
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("bridge error panic payload destructor");
    }
}
struct BusinessError {
    counts: Arc<Counts>,
    recursive: bool,
}
impl fmt::Display for BusinessError {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.display.fetch_add(1, Ordering::SeqCst);
        panic!("bridge business error Display");
    }
}
impl fmt::Debug for BusinessError {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.debug.fetch_add(1, Ordering::SeqCst);
        panic!("bridge business error Debug");
    }
}
impl std::error::Error for BusinessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.counts.source.fetch_add(1, Ordering::SeqCst);
        panic!("bridge business error source");
    }
}
impl Drop for BusinessError {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        if self.recursive {
            std::panic::panic_any(Recursive(self.counts.clone()));
        }
    }
}
struct Echo(Option<Arc<Counts>>);
struct PolicyDrop(Arc<Counts>);
impl Drop for PolicyDrop {
    fn drop(&mut self) {
        self.0.policy_drops.fetch_add(1, Ordering::SeqCst);
        panic!("bridge policy destructor");
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        if let Some(counts) = &self.0 {
            counts.actor_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
}
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
}
struct Bridge {
    counts: Arc<Counts>,
    publish: bool,
    recursive: bool,
    known: u8,
}
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
        receipt: ChildSpawnReceipt,
    ) -> ActorResult<BoxedActorRef> {
        *self.counts.lifetime.lock().unwrap() = Some(receipt.0.lifetime.clone());
        *self.counts.finished.lock().unwrap() = Some(receipt.0.finished.subscribe());
        if self.publish {
            let reference = Box::new(TypedSpawnPayload::new(Echo(Some(self.counts.clone()))))
                .spawn_child_on(system, engine, parent, receipt)?;
            crate::owned_payload::PublicationInput::new(reference).retire()?;
        }
        if let Some((entered, release)) = self.counts.gate.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        if self.known == 2 {
            return Err(ActorError::Timeout);
        }
        if self.known == 1 {
            return Err(ActorError::InitializationError(
                "bridge known failure".into(),
            ));
        }
        Err(ActorError::Other(anyhow::Error::new(BusinessError {
            counts: self.counts.clone(),
            recursive: self.recursive,
        })))
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
        .spawn_root_typed_thread(Echo(None), EmptyConfig)
        .await
        .unwrap();
    let scope = match system.registry.read().unwrap().get(&parent.path()).unwrap() {
        ActorRegistryEntry::Dynamic(entry) => entry.children.clone(),
        _ => panic!("parent was not published"),
    };
    (system, parent, scope)
}
fn original<'a>(error: &'a ActorError, counts: &Arc<Counts>) -> &'a BusinessError {
    let mut current: &(dyn std::error::Error + 'static) = error;
    // Stop at the concrete business error: its own source is intentionally hostile.
    for _ in 0..16 {
        if let Some(business) = current.downcast_ref::<BusinessError>() {
            assert!(
                Arc::ptr_eq(&business.counts, counts),
                "original error replaced"
            );
            return business;
        }
        current = current.source().expect("original error source lost");
    }
    panic!("original business error was not reachable");
}
fn known_error(error: &ActorError, kind: u8) {
    match kind {
        1 => assert!(
            matches!(error, ActorError::InitializationError(message) if message == "bridge known failure")
        ),
        2 => assert!(
            matches!(error, ActorError::TimeoutDetail(message) if message == "child bridge creation timed out")
        ),
        _ => unreachable!(),
    }
}
async fn check(publish: bool, recursive: bool, unobserved: bool, known: u8, policy: bool) {
    let (system, parent, scope) = setup(Duration::from_secs(3)).await;
    let counts = Arc::new(Counts::default());
    let strategy = policy.then(|| {
        use parrot_api::supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision};
        let capture = PolicyDrop(counts.clone());
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(1),
            decider: BasicDecisionFn::new(move |_| {
                let _ = &capture;
                SupervisionDecision::Stop
            }),
        })
    });
    let mut creation = Box::pin(system.spawn_child_erased(
        Box::new(ErasedSpawnBox(Box::new(Bridge {
            counts: counts.clone(),
            publish,
            recursive,
            known,
        }))),
        Box::new(EmptyConfig),
        strategy,
        parent.clone_boxed(),
        &scope,
    ));
    if unobserved {
        std::future::poll_fn(|cx| {
            assert!(creation.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let finished = counts.finished.lock().unwrap().clone();
                if finished.is_some_and(|finished| finished.borrow().is_some()) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("creation did not finish");
        drop(creation);
    } else {
        let error = match tokio::time::timeout(Duration::from_secs(5), creation)
            .await
            .unwrap()
        {
            Err(error) => error,
            Ok(reference) => {
                crate::owned_payload::PublicationInput::new(reference)
                    .retire()
                    .unwrap();
                panic!("bridge failure returned success");
            }
        };
        if policy {
            assert!(
                matches!(&error, ActorError::Panic(message) if message.contains("bridge policy destructor"))
            );
        } else if known != 0 {
            known_error(&error, known);
        } else {
            original(&error, &counts);
        }
        let lifetime = counts.lifetime.lock().unwrap().clone().unwrap();
        let retired = tokio::time::timeout(Duration::from_secs(3), lifetime.wait())
            .await
            .expect("publication barrier leaked");
        if publish {
            let retired = retired.expect_err("publication failure lost from retirement");
            if known != 0 {
                known_error(&retired, known);
            } else {
                let created = original(&error, &counts) as *const BusinessError;
                let retired = original(&retired, &counts) as *const BusinessError;
                assert_eq!(
                    created, retired,
                    "retirement replaced the concrete error object"
                );
                let completion = counts
                    .finished
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .borrow()
                    .clone()
                    .unwrap();
                let StopOutcome::Failed(completion) = completion else {
                    panic!("creation completion lost the bridge cause");
                };
                let completion = ActorError::from(completion.into_error());
                assert_eq!(
                    created,
                    original(&completion, &counts) as *const BusinessError
                );
            }
        } else if policy {
            assert!(
                matches!(retired, Err(ActorError::Panic(ref message)) if message.contains("bridge policy destructor"))
            );
        } else {
            retired.unwrap();
        }
        drop(error);
    }
    assert!(scope.view().is_empty());
    assert_eq!(
        system.actor_count(),
        1,
        "failed bridge left an actor registered"
    );
    assert_eq!(
        counts.actor_drops.load(Ordering::SeqCst),
        usize::from(publish)
    );
    let reply = parent.send(Box::new(91_u64)).await.unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 91);
    system.shutdown_internal().await.unwrap();
    counts.lifetime.lock().unwrap().take();
    counts.finished.lock().unwrap().take();
    drop(scope);
    drop(parent);
    drop(system);
    if known == 0 {
        tokio::time::timeout(Duration::from_secs(3), async {
            while counts.drops.load(Ordering::SeqCst) != 1
                || counts.payloads.load(Ordering::SeqCst) != usize::from(recursive)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("original error or recursive panic payload leaked");
    }
    assert_eq!(counts.display.load(Ordering::SeqCst), 0);
    assert_eq!(counts.debug.load(Ordering::SeqCst), 0);
    assert_eq!(counts.source.load(Ordering::SeqCst), 0);
    assert_eq!(
        counts.policy_drops.load(Ordering::SeqCst),
        usize::from(policy)
    );
}
async fn delayed_error(cancel: bool) {
    let (system, parent, scope) = setup(Duration::from_millis(300)).await;
    let counts = Arc::new(Counts::default());
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    *counts.gate.lock().unwrap() = Some((entered, released));
    let (ready, waiting) = oneshot::channel();
    let (ack, acknowledged) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        entering.recv_timeout(Duration::from_secs(5)).unwrap();
        ready.send(()).unwrap();
        let responsive = acknowledged.recv_timeout(Duration::from_secs(3)).is_ok();
        release.send(()).unwrap();
        responsive
    });
    let owner = system.clone();
    let ancestor = parent.clone();
    let children = scope.clone();
    let input = counts.clone();
    let caller = tokio::spawn(async move {
        owner
            .spawn_child_erased(
                Box::new(ErasedSpawnBox(Box::new(Bridge {
                    counts: input,
                    publish: true,
                    recursive: true,
                    known: 0,
                }))),
                Box::new(EmptyConfig),
                None,
                ancestor.clone_boxed(),
                &children,
            )
            .await
    });
    waiting.await.unwrap();
    if cancel {
        caller.abort();
        match caller.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("bridge returned before caller cancellation"),
        }
    } else {
        assert!(matches!(
            caller.await.unwrap(),
            Err(ActorError::TimeoutDetail(_))
        ));
    }
    let owned = scope.view().len();
    let lifetime = counts.lifetime.lock().unwrap().clone().unwrap();
    let pending = matches!(
        lifetime.retirement_status(),
        parrot_api::address::RetirementStatus::Pending
    );
    ack.send(()).unwrap();
    assert!(
        observer.join().unwrap(),
        "bridge blocked async deadline/cancellation"
    );
    assert_eq!(owned, 1, "pending bridge lost parent ownership");
    assert!(
        pending,
        "blocked bridge published a final retirement receipt"
    );
    let error = tokio::time::timeout(Duration::from_secs(3), lifetime.wait())
        .await
        .unwrap()
        .expect_err("late bridge failure lost its retirement cause");
    original(&error, &counts);
    assert!(scope.view().is_empty());
    assert_eq!(system.actor_count(), 1);
    assert_eq!(counts.actor_drops.load(Ordering::SeqCst), 1);
    drop(error);
    drop(lifetime);
    counts.lifetime.lock().unwrap().take();
    counts.finished.lock().unwrap().take();
    assert_eq!(
        *parent
            .send(Box::new(17_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        17
    );
    system.shutdown_internal().await.unwrap();
    drop(scope);
    drop(parent);
    drop(system);
    tokio::time::timeout(Duration::from_secs(3), async {
        while counts.drops.load(Ordering::SeqCst) != 1
            || counts.payloads.load(Ordering::SeqCst) != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late bridge error cleanup leaked");
    assert_eq!(counts.display.load(Ordering::SeqCst), 0);
    assert_eq!(counts.debug.load(Ordering::SeqCst), 0);
    assert_eq!(counts.source.load(Ordering::SeqCst), 0);
}
async fn isolated(name: &str, mode: u8) {
    const KEY: &str = "PARROT_CHILD_ERROR_WORKER";
    if std::env::var_os(KEY).is_some() {
        if mode >= 32 {
            delayed_error(mode == 32).await;
        } else {
            check(
                mode & 1 != 0,
                mode & 2 != 0,
                mode & 4 != 0,
                u8::from(mode & 8 != 0),
                mode & 16 != 0,
            )
            .await;
            if mode & 8 != 0 {
                check(mode & 1 != 0, false, false, 2, false).await;
            }
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
    .expect("bridge error worker stalled")
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
                    "thread::child_instance::error_contracts::",
                    stringify!($name)
                ),
                $mode,
            )
            .await;
        }
    };
}
contract!(
    unpublished_error_retains_original_cause_without_diagnostics,
    0
);
contract!(published_error_retains_cause_and_retires_actual_actor, 1);
contract!(unpublished_error_recursive_drop_is_isolated, 2);
contract!(published_error_recursive_drop_is_isolated, 3);
contract!(unobserved_unpublished_error_is_protected, 6);
contract!(unobserved_published_error_is_protected, 7);
contract!(known_unpublished_error_preserves_class_and_text, 8);
contract!(known_published_error_preserves_class_and_text, 9);
contract!(
    policy_cleanup_failure_safely_supersedes_recursive_business_error,
    18
);
contract!(cancelled_bridge_retains_late_error_and_retires_actor, 32);
contract!(timed_out_bridge_retains_late_error_and_retires_actor, 64);
