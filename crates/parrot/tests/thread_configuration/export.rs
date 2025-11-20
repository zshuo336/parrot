use super::*;

#[derive(Clone, Copy)]
enum Export {
    Panic,
    WrongType,
    WrongTypeDropPanic,
    InvalidCapacity,
    Default,
}

struct Configuration {
    input: Input,
    export: Export,
    calls: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
}
impl ActorConfig for Configuration {
    fn engine_config(&self) -> Option<Box<dyn std::any::Any + Send>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.entered.notify_one();
            let (_guard, result) = gate
                .changed
                .wait_timeout_while(
                    gate.released.lock().unwrap(),
                    Duration::from_secs(10),
                    |released| !*released,
                )
                .unwrap();
            assert!(
                !result.timed_out(),
                "configuration conversion gate not released"
            );
        }
        match self.export {
            Export::Panic => panic!("configuration conversion panic"),
            Export::WrongType => Some(Box::new(42_u64)),
            Export::WrongTypeDropPanic => Some(Box::new(input(&self.input.counts, true))),
            Export::InvalidCapacity => Some(Box::new(ThreadActorConfig {
                mailbox_capacity: Some(0),
                ..Default::default()
            })),
            Export::Default => None,
        }
    }
}

struct Configured(Input);
impl Actor for Configured {
    type Config = Configuration;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.counts.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}

#[tokio::test]
async fn conversion_failures_retire_inputs_without_initialization() {
    for export in [
        Export::Panic,
        Export::WrongType,
        Export::WrongTypeDropPanic,
        Export::InvalidCapacity,
    ] {
        let system = system();
        let counts = Arc::new(Counts::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let result = bounded(
            AssertUnwindSafe(system.spawn_root_typed_thread(
                Configured(input(&counts, false)),
                Configuration {
                    input: input(&counts, false),
                    export,
                    calls: calls.clone(),
                    gate: None,
                },
            ))
            .catch_unwind(),
        )
        .await
        .expect("conversion unwound caller");
        match export {
            Export::Panic => assert!(
                matches!(result, Err(SpawnError::InitializationFailed(ref message)) if message.contains("configuration conversion panic")),
                "{result:?}"
            ),
            Export::WrongType | Export::InvalidCapacity => assert!(
                matches!(result, Err(SpawnError::InvalidConfig(_))),
                "{result:?}"
            ),
            Export::WrongTypeDropPanic => assert!(
                matches!(result, Err(SpawnError::DisposalFailed(_))),
                "{result:?}"
            ),
            Export::Default => unreachable!(),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(
            counts.disposed.load(Ordering::SeqCst),
            if matches!(export, Export::WrongTypeDropPanic) {
                3
            } else {
                2
            }
        );
        assert_eq!(system.actor_count(), 0);
        let healthy = bounded(system.spawn_root_typed_thread(
            Configured(input(&counts, false)),
            Configuration {
                input: input(&counts, false),
                export: Export::Default,
                calls: calls.clone(),
                gate: None,
            },
        ))
        .await
        .unwrap();
        let reply = bounded(healthy.send(Box::new(37_u64))).await.unwrap();
        assert_eq!(*reply.downcast::<u64>().unwrap(), 37);
        bounded(healthy.stop()).await.unwrap();
        bounded(system.shutdown_internal()).await.unwrap();
    }
}

#[tokio::test]
async fn unpolled_and_closed_creation_never_calls_the_conversion_hook() {
    let system = system();
    let counts = Arc::new(Counts::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let make = || Configuration {
        input: input(&counts, false),
        export: Export::Panic,
        calls: calls.clone(),
        gate: None,
    };
    drop(system.spawn_root_typed_thread(Configured(input(&counts, false)), make()));
    bounded(async {
        while counts.disposed.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(system.shutdown_internal()).await.unwrap();
    let result =
        bounded(system.spawn_root_typed_thread(Configured(input(&counts, false)), make())).await;
    assert!(
        matches!(result, Err(SpawnError::SystemShutdown)),
        "{result:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(counts.disposed.load(Ordering::SeqCst), 4);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_blocking_conversion_retires_late_inputs_without_publication() {
    let system = system();
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let owner = system.clone();
    let observed = counts.clone();
    let blocked = gate.clone();
    let waiter = tokio::spawn(async move {
        owner
            .spawn_root_typed_thread(
                Configured(input(&observed, false)),
                Configuration {
                    input: input(&observed, false),
                    export: Export::Default,
                    calls: Arc::default(),
                    gate: Some(blocked),
                },
            )
            .await
    });
    bounded(gate.entered.notified()).await;
    // Reaching this assertion on a current-thread runtime proves the callback
    // did not block the async driver, even while synchronous user code waits.
    assert_eq!(system.actor_count(), 0);
    waiter.abort();
    assert!(bounded(waiter).await.unwrap_err().is_cancelled());
    drop(release);
    bounded(async {
        while counts.disposed.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 0);
    bounded(system.shutdown_internal()).await.unwrap();
}
