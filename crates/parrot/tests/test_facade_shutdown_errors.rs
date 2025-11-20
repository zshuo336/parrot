use parrot::{
    system::{ActorSystemImpl, ParrotActorSystem},
    thread::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, InstanceConfig, RuntimeActor, SpawnOptions},
    errors::ActorError,
    system::{SystemError, SystemState, SystemStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Default)]
struct Counts {
    callbacks: AtomicUsize,
    errors: AtomicUsize,
    payloads: AtomicUsize,
    hooks: AtomicUsize,
    actors: AtomicUsize,
    futures: AtomicUsize,
    shutdowns: AtomicUsize,
    address: AtomicUsize,
    entered: tokio::sync::Notify,
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("facade recursive payload disposal");
    }
}
struct Business {
    counts: Arc<Counts>,
    recursive: bool,
}
impl fmt::Display for Business {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.callbacks.fetch_add(1, Ordering::SeqCst);
        panic!("facade must not format business errors");
    }
}
impl fmt::Debug for Business {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.callbacks.fetch_add(1, Ordering::SeqCst);
        panic!("facade must not debug business errors");
    }
}
impl Error for Business {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.counts.callbacks.fetch_add(1, Ordering::SeqCst);
        panic!("facade must not traverse business sources");
    }
}
impl Drop for Business {
    fn drop(&mut self) {
        self.counts.errors.fetch_add(1, Ordering::SeqCst);
        if self.recursive {
            std::panic::panic_any(Recursive(self.counts.clone()));
        }
    }
}
fn business(counts: &Arc<Counts>, recursive: bool) -> anyhow::Error {
    let error = anyhow::Error::new(Business {
        counts: counts.clone(),
        recursive,
    });
    counts.address.store(
        error.downcast_ref::<Business>().unwrap() as *const Business as usize,
        Ordering::SeqCst,
    );
    error
}
fn original(error: &(dyn Error + 'static), counts: &Arc<Counts>) {
    let mut next = Some(error);
    for _ in 0..64 {
        let error = next.expect("facade lost the original business cause");
        if let Some(business) = error.downcast_ref::<Business>() {
            assert!(Arc::ptr_eq(&business.counts, counts));
            assert_eq!(
                business as *const Business as usize,
                counts.address.load(Ordering::SeqCst)
            );
            return;
        }
        next = error.source();
    }
    panic!("facade cause chain exceeded its bound");
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(8), future)
        .await
        .expect("facade error contract deadline")
}
async fn eventually(mut condition: impl FnMut() -> bool) {
    bounded(async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}
struct Probe {
    counts: Arc<Counts>,
    faulty: bool,
    recursive: bool,
    known: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Probe {
    type Config = InstanceConfig;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.hooks.fetch_add(1, Ordering::SeqCst);
            if self.known {
                return Err(ActorError::MessageHandlingError(
                    "known native shutdown error".into(),
                ));
            }
            if self.faulty {
                Err(ActorError::Other(business(&self.counts, self.recursive)))
            } else {
                Ok(())
            }
        })
    }
}
async fn spawn(
    runtime: &dyn AsyncEngineRuntime,
    counts: &Arc<Counts>,
    faulty: bool,
    recursive: bool,
) {
    let reference = runtime
        .spawn(
            Box::new(Probe {
                counts: counts.clone(),
                faulty,
                recursive,
                known: false,
            }),
            Box::new(()),
            SpawnOptions::at("/error-contract"),
        )
        .await
        .unwrap();
    assert_eq!(
        *reference
            .send(Box::new(41_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        41
    );
}

// Independent callback construction, poll, and disposal failures. The custom
// runtime has no admitted actors; each facade also closes a real healthy actor.
struct Extension {
    engine: Arc<ThreadActorSystem>,
    counts: Arc<Counts>,
    mode: u8,
    gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}
struct Stop {
    counts: Arc<Counts>,
    mode: u8,
    gate: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Future for Stop {
    type Output = Result<(), SystemError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.counts.entered.notify_one();
        if let Some(gate) = &mut self.gate {
            if Pin::new(gate).poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.gate = None;
        }
        if matches!(self.mode, 2 | 4) {
            std::panic::panic_any(Recursive(self.counts.clone()));
        }
        Poll::Ready(match self.mode {
            0 | 5 | 6 => Err(SystemError::Other(business(&self.counts, self.mode != 0))),
            8 => Err(SystemError::InitializationError(
                "known runtime error".into(),
            )),
            9 => Err(SystemError::ActorError(ActorError::MessageHandlingError(
                "known actor error".into(),
            ))),
            10 => Err(SystemError::Other(anyhow::anyhow!("known string error"))),
            11 => Err(SystemError::Other(anyhow::Error::msg(
                "known owned string".to_owned(),
            ))),
            _ => Ok(()),
        })
    }
}
impl Drop for Stop {
    fn drop(&mut self) {
        self.counts.futures.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, 3..=5) {
            std::panic::panic_any(Recursive(self.counts.clone()));
        }
    }
}
impl AsyncEngineRuntime for Extension {
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.engine.spawn(actor, config, options)
    }
    fn get_actor<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        AsyncEngineRuntime::get_actor(self.engine.as_ref(), path)
    }
    fn status(&self) -> SystemStatus {
        self.engine.status()
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        self.counts.shutdowns.fetch_add(1, Ordering::SeqCst);
        if self.mode == 1 {
            std::panic::panic_any(Recursive(self.counts.clone()));
        }
        Box::pin(Stop {
            counts: self.counts.clone(),
            mode: self.mode,
            gate: self.gate.lock().unwrap().take(),
        })
    }
}
async fn direct(kind: u8) {
    let counts = Arc::new(Counts::default());
    let system = if kind == 2 {
        ActorSystemImpl::Actix(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        )
    } else {
        let engine = ThreadActorSystem::shared(Default::default());
        if kind == 0 {
            ActorSystemImpl::Thread(engine)
        } else {
            ActorSystemImpl::Runtime(engine)
        }
    };
    match &system {
        ActorSystemImpl::Actix(engine) => spawn(engine, &counts, true, true).await,
        ActorSystemImpl::Thread(engine) => spawn(engine.as_ref(), &counts, true, true).await,
        ActorSystemImpl::Runtime(engine) => spawn(engine.as_ref(), &counts, true, true).await,
    }
    let error = bounded(system.shutdown()).await.unwrap_err();
    original(&error, &counts);
    assert_eq!(counts.callbacks.load(Ordering::SeqCst), 0);
    drop(error);
    eventually(|| {
        counts.errors.load(Ordering::SeqCst) == 1 && counts.payloads.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
}
async fn facade(mode: u8, scenario: u8) -> Option<(SystemError, Arc<Counts>)> {
    let mut config = parrot_api::system::ActorSystemConfig::default();
    if scenario == 2 {
        config.timeouts.system_shutdown = Duration::from_millis(30);
    }
    let facade = Arc::new(ParrotActorSystem::new(config).await.unwrap());
    let counts = Arc::new(Counts::default());
    let healthy = Arc::new(Counts::default());
    let engine = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("z-healthy".into(), engine.clone(), false)
        .await
        .unwrap();
    spawn(engine.as_ref(), &healthy, false, false).await;
    let (release, gate) = tokio::sync::oneshot::channel();
    let faulty = Arc::new(Extension {
        engine: ThreadActorSystem::shared(Default::default()),
        counts: counts.clone(),
        mode,
        gate: Mutex::new((scenario == 1 || scenario == 2).then_some(gate)),
    });
    facade
        .register_runtime("a-faulty".into(), faulty.clone(), true)
        .unwrap();
    facade
        .register_runtime("b-alias".into(), faulty.clone(), false)
        .unwrap();
    let caller = facade.clone();
    let first = tokio::spawn(async move { caller.shutdown_runtime().await });
    let error = if scenario == 1 {
        bounded(counts.entered.notified()).await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        bounded(facade.shutdown_runtime()).await.unwrap_err()
    } else if scenario == 2 {
        let error = bounded(first).await.unwrap().unwrap_err();
        assert_eq!(
            error.to_string(),
            "facade shutdown timed out; cleanup remains owned"
        );
        assert_eq!(counts.errors.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        error
    } else {
        bounded(first).await.unwrap().unwrap_err()
    };
    if matches!(mode, 0 | 5 | 6) && scenario != 2 {
        original(&error, &counts);
    }
    // Formatting the framework aggregate must use cached safe diagnostics.
    let text = error.to_string();
    match mode {
        8 => assert_eq!(
            text,
            "engine \"a-faulty\": System initialization failed: known runtime error"
        ),
        9 => assert_eq!(
            text,
            "engine \"a-faulty\": Message handling failed: known actor error"
        ),
        10 => assert_eq!(text, "engine \"a-faulty\": known string error"),
        11 => assert_eq!(text, "engine \"a-faulty\": known owned string"),
        _ => {}
    }
    let SystemError::Other(outer) = &error else {
        panic!("expected Other")
    };
    let aggregate = outer
        .downcast_ref::<parrot::system::ShutdownErrors>()
        .expect("structured shutdown errors");
    assert_eq!(
        aggregate.failures().len(),
        if matches!(mode, 4 | 5) { 2 } else { 1 }
    );
    if matches!(mode, 4 | 5) {
        assert_eq!(
            aggregate.failures()[1].operation(),
            "shutdown future disposal"
        );
        assert!(matches!(
            aggregate.failures()[1].error(),
            SystemError::ActorError(ActorError::Panic(_))
        ));
    }
    if mode == 8 {
        assert!(matches!(
            aggregate.failures()[0].error(),
            SystemError::InitializationError(_)
        ));
    } else if mode == 9 {
        assert!(matches!(
            aggregate.failures()[0].error(),
            SystemError::ActorError(ActorError::MessageHandlingError(_))
        ));
    }
    let debug = format!("{aggregate:?}");
    assert!(!debug.is_empty());
    assert!(scenario == 2 || text.contains("engine \"a-faulty\""));
    assert!(!text.contains("b-alias"));
    let (second, third) = tokio::join!(facade.shutdown_runtime(), facade.shutdown_runtime());
    assert_eq!(second.unwrap_err().to_string(), text);
    assert_eq!(third.unwrap_err().to_string(), text);
    eventually(|| healthy.actors.load(Ordering::SeqCst) == 1).await;
    assert_eq!(healthy.hooks.load(Ordering::SeqCst), 1);
    assert_eq!(engine.status().state, SystemState::Stopped);
    eventually(|| counts.futures.load(Ordering::SeqCst) == usize::from(mode != 1)).await;
    assert_eq!(counts.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(counts.callbacks.load(Ordering::SeqCst), 0);
    faulty.engine.shutdown().await.unwrap();
    drop(faulty);
    drop(facade);
    if scenario == 3 {
        assert_eq!(counts.errors.load(Ordering::SeqCst), 0);
        return Some((error, counts));
    }
    drop(error);
    eventually(|| counts.errors.load(Ordering::SeqCst) == usize::from(matches!(mode, 0 | 5 | 6)))
        .await;
    let expected_payloads = match mode {
        1..=3 | 6 => 1,
        4 | 5 => 2,
        _ => 0,
    };
    eventually(|| counts.payloads.load(Ordering::SeqCst) == expected_payloads).await;
    assert_eq!(counts.callbacks.load(Ordering::SeqCst), 0);
    None
}

fn aggregate(error: &SystemError) -> &parrot::system::ShutdownErrors {
    let SystemError::Other(error) = error else {
        panic!("aggregate must retain Other category")
    };
    error
        .downcast_ref()
        .expect("public structured failure collection")
}
async fn mixed(unobserved: bool) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let thread = ThreadActorSystem::shared(Default::default());
    let actix = parrot::actix::ActixActorSystem::with_arbiter_count(1)
        .await
        .unwrap();
    let counts: Vec<_> = (0..4).map(|_| Arc::new(Counts::default())).collect();
    let external = Arc::new(Extension {
        engine: ThreadActorSystem::shared(Default::default()),
        counts: counts[0].clone(),
        mode: 6,
        gate: Mutex::new(None),
    });
    let healthy = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("z-thread".into(), thread.clone(), false)
        .await
        .unwrap();
    facade
        .register_actix_system("m-actix".into(), actix.clone(), false)
        .await
        .unwrap();
    facade
        .register_runtime("a-external".into(), external.clone(), true)
        .unwrap();
    facade
        .register_thread_system("zz-healthy".into(), healthy.clone(), false)
        .await
        .unwrap();
    facade
        .register_runtime("n-actix-alias".into(), Arc::new(actix.clone()), false)
        .unwrap();
    facade
        .register_runtime("zz-thread-alias".into(), thread.clone(), false)
        .unwrap();
    spawn(thread.as_ref(), &counts[2], true, true).await;
    spawn(&actix, &counts[1], true, true).await;
    spawn(healthy.as_ref(), &counts[3], false, false).await;
    if unobserved {
        let caller = facade.clone();
        let waiter = tokio::spawn(async move { caller.shutdown_runtime().await });
        eventually(|| waiter.is_finished()).await;
        drop(waiter);
        external.engine.shutdown().await.unwrap();
        drop(facade);
        drop(thread);
        drop(actix);
        drop(external);
    } else {
        let results = futures::future::join_all((0..8).map(|_| facade.shutdown_runtime())).await;
        let errors: Vec<_> = results.into_iter().map(Result::unwrap_err).collect();
        for error in &errors {
            original(error, &counts[0]);
            let failures = aggregate(error).failures();
            assert_eq!(failures.len(), 3);
            for (index, (failure, expected)) in failures
                .iter()
                .zip(["a-external", "m-actix", "z-thread"])
                .enumerate()
            {
                assert_eq!(failure.engine_name(), Some(expected));
                assert_eq!(failure.operation(), "shutdown");
                assert!(failure.summary().contains("original error retained"));
                original(failure, &counts[index]);
                assert!(std::ptr::eq(
                    failure.error(),
                    aggregate(&errors[0]).failures()[index].error()
                ));
                assert!(!format!("{failure:?}").is_empty());
            }
            assert_eq!(error.to_string(), errors[0].to_string());
        }
        let retained = aggregate(&errors[0]).clone();
        external.engine.shutdown().await.unwrap();
        assert_eq!(healthy.status().state, SystemState::Stopped);
        drop(errors);
        drop(facade);
        drop(thread);
        drop(actix);
        drop(external);
        for (index, failure) in retained.failures().iter().enumerate() {
            assert_eq!(counts[index].errors.load(Ordering::SeqCst), 0);
            original(failure, &counts[index]);
        }
        drop(retained);
    }
    for (index, count) in counts.iter().enumerate() {
        if index < 3 {
            eventually(|| {
                count.errors.load(Ordering::SeqCst) == 1
                    && count.payloads.load(Ordering::SeqCst) == 1
            })
            .await;
        }
        if index > 0 {
            assert_eq!(count.actors.load(Ordering::SeqCst), 1);
            assert_eq!(count.hooks.load(Ordering::SeqCst), 1);
        }
        assert_eq!(count.callbacks.load(Ordering::SeqCst), 0);
    }
    assert_eq!(counts[0].shutdowns.load(Ordering::SeqCst), 1);
}
async fn healthy() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    let counts = Arc::new(Counts::default());
    facade
        .register_thread_system("healthy".into(), engine.clone(), true)
        .await
        .unwrap();
    spawn(engine.as_ref(), &counts, false, false).await;
    let (a, b) = tokio::join!(facade.shutdown_runtime(), facade.shutdown_runtime());
    a.unwrap();
    b.unwrap();
    facade.shutdown_runtime().await.unwrap();
    assert_eq!(engine.status().state, SystemState::Stopped);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.hooks.load(Ordering::SeqCst), 1);
}
async fn native_text(actix: bool) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let counts = Arc::new(Counts::default());
    let engine: Arc<dyn AsyncEngineRuntime> = if actix {
        Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        )
    } else {
        ThreadActorSystem::shared(Default::default())
    };
    facade
        .register_runtime("known".into(), engine.clone(), true)
        .unwrap();
    let reference = engine
        .spawn(
            Box::new(Probe {
                counts: counts.clone(),
                faulty: true,
                recursive: false,
                known: true,
            }),
            Box::new(()),
            SpawnOptions::at("/known"),
        )
        .await
        .unwrap();
    reference.send(Box::new(1_u64)).await.unwrap();
    let native = bounded(engine.shutdown()).await.unwrap_err();
    let expected = native.to_string();
    assert!(expected.contains("known native shutdown error"));
    let error = bounded(facade.shutdown_runtime()).await.unwrap_err();
    assert_eq!(error.to_string(), format!("engine \"known\": {expected}"));
    let failure = &aggregate(&error).failures()[0];
    assert_eq!(failure.summary(), expected);
    let mut current = failure.source();
    let mut found = false;
    for _ in 0..64 {
        let Some(cause) = current else { break };
        if let Some(ActorError::MessageHandlingError(message)) = cause.downcast_ref::<ActorError>()
        {
            assert_eq!(message, "known native shutdown error");
            found = true;
            break;
        }
        current = cause.source();
    }
    assert!(
        found,
        "known native concrete category must survive aggregation"
    );
    assert_eq!(engine.status().state, SystemState::Stopped);
    assert_eq!(counts.hooks.load(Ordering::SeqCst), 1);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(
        facade.shutdown_runtime().await.unwrap_err().to_string(),
        error.to_string()
    );
}
async fn isolated(name: &str, kind: u8, mode: u8, scenario: u8) {
    const KEY: &str = "PARROT_FACADE_SHUTDOWN_ERROR_WORKER";
    if std::env::var_os(KEY).is_some() {
        if kind < 3 {
            direct(kind).await;
        } else if kind == 4 {
            mixed(scenario == 1).await;
        } else if kind == 5 {
            healthy().await;
        } else if kind == 6 {
            native_text(mode == 1).await;
        } else if scenario == 3 {
            let (error, counts) = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(facade(mode, scenario)).unwrap()
            })
            .join()
            .unwrap();
            original(&error, &counts);
            drop(error);
            eventually(|| {
                counts.errors.load(Ordering::SeqCst) == 1
                    && counts.payloads.load(Ordering::SeqCst) == 1
            })
            .await;
            assert_eq!(counts.callbacks.load(Ordering::SeqCst), 0);
        } else {
            facade(mode, scenario).await;
        }
        return;
    }
    let output = tokio::time::timeout(
        Duration::from_secs(25),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(KEY, "1")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("facade error worker stalled")
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success()
            && stdout
                .lines()
                .filter(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"))
                .count()
                == 1,
        "facade worker must execute exactly one passing contract: status={}\n{}\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}
macro_rules! case {
    ($name:ident, $kind:expr, $mode:expr, $scenario:expr) => {
        #[actix::test]
        async fn $name() {
            isolated(stringify!($name), $kind, $mode, $scenario).await;
        }
    };
}
case!(impl_thread_retains_business_cause, 0, 0, 0);
case!(impl_runtime_retains_business_cause, 1, 0, 0);
case!(impl_actix_retains_business_cause, 2, 0, 0);
case!(facade_retains_business_cause, 3, 0, 0);
case!(facade_isolates_callback_construction_panic, 3, 1, 0);
case!(facade_isolates_callback_poll_panic, 3, 2, 0);
case!(facade_isolates_callback_disposal_panic, 3, 3, 0);
case!(facade_isolates_poll_and_disposal_panics, 3, 4, 0);
case!(facade_retains_result_before_disposal_panic, 3, 5, 0);
case!(facade_cancelled_waiter_retains_operation, 3, 6, 1);
case!(facade_timeout_retains_late_error_cleanup, 3, 6, 2);
case!(facade_business_error_outlives_host, 3, 6, 3);
case!(facade_keeps_every_mixed_engine_failure, 4, 0, 0);
case!(facade_unobserved_mixed_output_is_protected, 4, 0, 1);
case!(facade_known_runtime_category_and_text, 3, 8, 0);
case!(facade_known_actor_category_and_text, 3, 9, 0);
case!(facade_known_static_string_text, 3, 10, 0);
case!(facade_known_owned_string_text, 3, 11, 0);
case!(facade_healthy_shutdown_is_repeatable, 5, 0, 0);
case!(facade_known_thread_text_matches_native_receipt, 6, 0, 0);
case!(facade_known_actix_text_matches_native_receipt, 6, 1, 0);
