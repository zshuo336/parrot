use parrot::thread::{
    SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorRef, RetirementStatus},
    engine::AsyncEngineRuntime,
    errors::ActorError,
    system::{ActorSystem, SystemError, SystemState},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    error::Error,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Counts {
    display: AtomicUsize,
    debug: AtomicUsize,
    source: AtomicUsize,
    errors: AtomicUsize,
    payloads: AtomicUsize,
    actors: AtomicUsize,
    hooks: AtomicUsize,
    address: AtomicUsize,
    entered: tokio::sync::Notify,
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("shutdown panic payload destructor");
    }
}
struct BusinessError {
    counts: Arc<Counts>,
    recursive: bool,
}
impl fmt::Display for BusinessError {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.display.fetch_add(1, Ordering::SeqCst);
        panic!("shutdown business Display must not run");
    }
}
impl fmt::Debug for BusinessError {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.debug.fetch_add(1, Ordering::SeqCst);
        panic!("shutdown business Debug must not run");
    }
}
impl Error for BusinessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.counts.source.fetch_add(1, Ordering::SeqCst);
        panic!("shutdown business source must not run");
    }
}
impl Drop for BusinessError {
    fn drop(&mut self) {
        self.counts.errors.fetch_add(1, Ordering::SeqCst);
        if self.recursive {
            std::panic::panic_any(Recursive(self.counts.clone()));
        }
    }
}
struct Probe {
    counts: Arc<Counts>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
    recursive: bool,
    known: u8,
    healthy: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Probe {
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
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.hooks.fetch_add(1, Ordering::SeqCst);
            self.counts.entered.notify_one();
            if let Some(release) = self.release.take() {
                release.await.unwrap();
            }
            if self.healthy {
                return Ok(());
            }
            if self.known == 1 {
                return Err(ActorError::InternalError("known shutdown witness".into()));
            }
            if self.known == 2 {
                return Err(ActorError::TimeoutDetail(
                    "known shutdown deadline witness".into(),
                ));
            }
            let error = anyhow::Error::new(BusinessError {
                counts: self.counts.clone(),
                recursive: self.recursive,
            });
            self.counts.address.store(
                error.downcast_ref::<BusinessError>().unwrap() as *const BusinessError as usize,
                Ordering::SeqCst,
            );
            Err(ActorError::Other(error))
        })
    }
}
fn original(error: &(dyn Error + 'static), counts: &Arc<Counts>) {
    let mut current = Some(error);
    for _ in 0..64 {
        let error = current.expect("public shutdown lost original business cause");
        if let Some(business) = error.downcast_ref::<BusinessError>() {
            assert!(Arc::ptr_eq(&business.counts, counts));
            assert_eq!(
                business as *const BusinessError as usize,
                counts.address.load(Ordering::SeqCst),
                "public shutdown reconstructed the business error"
            );
            return;
        }
        current = error.source();
    }
    panic!("public shutdown cause chain grew without bound");
}
fn no_callbacks(counts: &Counts) {
    assert_eq!(counts.display.load(Ordering::SeqCst), 0);
    assert_eq!(counts.debug.load(Ordering::SeqCst), 0);
    assert_eq!(counts.source.load(Ordering::SeqCst), 0);
}
async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown ownership did not retire");
}
async fn unique(mut system: Arc<ThreadActorSystem>) -> ThreadActorSystem {
    // Publication workers may still be releasing temporary owners after the
    // actor is runnable. Consuming shutdown requires unique ownership first.
    tokio::time::timeout(Duration::from_secs(5), async move {
        loop {
            match Arc::try_unwrap(system) {
                Ok(system) => return system,
                Err(shared) => system = shared,
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture retained a strong system owner")
}
async fn contract(
    consuming: bool,
    scheduler: u8,
    scenario: u8,
) -> Option<(SystemError, Arc<Counts>)> {
    let counts = Arc::new(Counts::default());
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_secs(10),
        ..Default::default()
    });
    let mode = match scheduler {
        0 => SchedulingMode::SharedPool {
            max_messages_per_run: 8,
        },
        1 => SchedulingMode::DedicatedThread,
        _ => SchedulingMode::Sharded {
            affinity_key: "public-shutdown".into(),
        },
    };
    let (release, gate) = tokio::sync::oneshot::channel();
    let actor = system
        .spawn_typed_with_config(
            Probe {
                counts: counts.clone(),
                release: (scenario == 2).then_some(gate),
                recursive: matches!(scenario, 1..=4),
                known: if scenario == 5 {
                    1
                } else {
                    u8::from(scenario == 7) * 2
                },
                healthy: scenario == 6,
            },
            EmptyConfig,
            ThreadActorConfig {
                scheduling_mode: Some(mode),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        *actor
            .send(Box::new(73_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        73
    );
    let mut borrowed = None;
    let shutdown: BoxedFuture<'static, Result<(), SystemError>> = if consuming {
        Box::pin(ActorSystem::shutdown(unique(system).await))
    } else {
        borrowed = Some(system.clone());
        Box::pin(async move { AsyncEngineRuntime::shutdown(system.as_ref()).await })
    };
    let mut caller = tokio::spawn(shutdown);
    let mut known_text = None;
    if scenario == 2 {
        tokio::time::timeout(Duration::from_secs(5), counts.entered.notified())
            .await
            .unwrap();
        assert!(
            !caller.is_finished(),
            "blocked shutdown reported completion"
        );
        assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        eventually(|| !matches!(actor.retirement_status(), RetirementStatus::Pending)).await;
        match actor.retirement_status() {
            RetirementStatus::Retired(Err(error)) => original(&error, &counts),
            _ => panic!("cancelled shutdown lost actual retirement failure"),
        }
        if let Some(system) = borrowed.as_ref() {
            let error = AsyncEngineRuntime::shutdown(system.as_ref())
                .await
                .unwrap_err();
            original(&error, &counts);
        }
    } else if scenario == 3 {
        eventually(|| caller.is_finished()).await;
        drop(caller);
    } else {
        let result = tokio::time::timeout(Duration::from_secs(5), &mut caller)
            .await
            .unwrap()
            .unwrap();
        if scenario == 6 {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            if matches!(scenario, 5 | 7) {
                let system = borrowed.as_ref().unwrap();
                let native = system.shutdown_internal().await.unwrap_err();
                known_text = Some(native.to_string());
                assert_eq!(error.to_string(), *known_text.as_ref().unwrap());
                let mut current = error.source();
                let mut found = false;
                while let Some(cause) = current {
                    if scenario == 5
                        && matches!(cause.downcast_ref::<ActorError>(), Some(ActorError::InternalError(text)) if text == "known shutdown witness")
                    {
                        found = true;
                        break;
                    }
                    if scenario == 7
                        && matches!(cause.downcast_ref::<ActorError>(), Some(ActorError::TimeoutDetail(text)) if text == "known shutdown deadline witness")
                    {
                        found = true;
                        break;
                    }
                    current = cause.source();
                }
                assert!(
                    found,
                    "public shutdown lost the known native error category"
                );
                assert!(matches!(error, SystemError::Other(_)));
            } else {
                original(&error, &counts);
                assert!(matches!(error, SystemError::Other(_)));
            }
            if scenario == 4 {
                drop(actor);
                drop(borrowed);
                return Some((error, counts));
            }
            if let Some(system) = borrowed.as_ref() {
                let again = AsyncEngineRuntime::shutdown(system.as_ref())
                    .await
                    .unwrap_err();
                if !matches!(scenario, 5 | 7) {
                    original(&again, &counts);
                } else {
                    assert_eq!(again.to_string(), *known_text.as_ref().unwrap());
                }
            }
            assert_eq!(counts.errors.load(Ordering::SeqCst), 0);
            drop(error);
        }
    }
    if let Some(system) = borrowed {
        eventually(|| AsyncEngineRuntime::status(system.as_ref()).state == SystemState::Stopped)
            .await;
        let status = AsyncEngineRuntime::status(system.as_ref());
        assert_eq!(status.active_actors, 0);
        assert_eq!(status.resources.thread_count, 0);
        assert_eq!(system.actor_count(), 0);
        let final_result = ActorSystem::shutdown(unique(system).await).await;
        if scenario == 6 {
            final_result.unwrap();
        } else {
            let error = final_result.unwrap_err();
            if !matches!(scenario, 5 | 7) {
                original(&error, &counts);
            } else {
                assert_eq!(error.to_string(), known_text.unwrap());
            }
        }
    }
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.hooks.load(Ordering::SeqCst), 1);
    drop(actor);
    eventually(|| counts.errors.load(Ordering::SeqCst) == usize::from(scenario < 5)).await;
    eventually(|| counts.payloads.load(Ordering::SeqCst) == usize::from(matches!(scenario, 1..=4)))
        .await;
    no_callbacks(&counts);
    None
}
async fn isolated(name: &str, consuming: bool, scheduler: u8, scenario: u8) {
    const KEY: &str = "PARROT_PUBLIC_SHUTDOWN_WORKER";
    if std::env::var_os(KEY).is_some() {
        if scenario == 4 {
            let (error, counts) = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime
                    .block_on(contract(consuming, scheduler, scenario))
                    .unwrap()
            })
            .join()
            .unwrap();
            original(&error, &counts);
            assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
            assert_eq!(counts.hooks.load(Ordering::SeqCst), 1);
            assert_eq!(counts.errors.load(Ordering::SeqCst), 0);
            drop(error);
            eventually(|| {
                counts.errors.load(Ordering::SeqCst) == 1
                    && counts.payloads.load(Ordering::SeqCst) == 1
            })
            .await;
            no_callbacks(&counts);
        } else {
            contract(consuming, scheduler, scenario).await;
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
    .expect("public shutdown worker stalled")
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success()
            && stdout
                .lines()
                .filter(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"))
                .count()
                == 1,
        "public shutdown worker did not execute exactly one successful contract: status={}\n{}\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}
macro_rules! case {
    ($name:ident, $consuming:expr, $scheduler:expr, $scenario:expr) => {
        #[tokio::test]
        async fn $name() {
            isolated(stringify!($name), $consuming, $scheduler, $scenario).await;
        }
    };
}
case!(
    runtime_shared_preserves_cause_without_callbacks,
    false,
    0,
    0
);
case!(
    runtime_dedicated_preserves_cause_without_callbacks,
    false,
    1,
    0
);
case!(
    runtime_sharded_preserves_cause_without_callbacks,
    false,
    2,
    0
);
case!(
    consuming_shared_preserves_cause_without_callbacks,
    true,
    0,
    0
);
case!(
    consuming_dedicated_preserves_cause_without_callbacks,
    true,
    1,
    0
);
case!(
    consuming_sharded_preserves_cause_without_callbacks,
    true,
    2,
    0
);
case!(runtime_recursive_error_drop_is_isolated, false, 0, 1);
case!(consuming_recursive_error_drop_is_isolated, true, 0, 1);
case!(
    cancelled_runtime_waiter_keeps_actual_retirement,
    false,
    0,
    2
);
case!(
    cancelled_consuming_waiter_keeps_actual_retirement,
    true,
    0,
    2
);
case!(unobserved_runtime_output_is_protected, false, 0, 3);
case!(unobserved_consuming_output_is_protected, true, 0, 3);
case!(runtime_error_outlives_host_runtime, false, 0, 4);
case!(consuming_error_outlives_host_runtime, true, 0, 4);
case!(known_failure_preserves_text_and_outer_category, false, 0, 5);
case!(healthy_public_shutdown_is_repeatable, false, 0, 6);
case!(
    known_timeout_preserves_text_and_native_category,
    false,
    0,
    7
);
