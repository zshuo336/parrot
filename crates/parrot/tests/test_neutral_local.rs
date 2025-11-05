//! Application behavior depends only on the public engine-neutral API.
use futures::FutureExt;
use parrot_api::{
    engine::{
        AsyncEngineCatalog, AsyncEngineRuntime, AsyncLocalRuntime, FactoryOptions, LocalActor,
        LocalRuntimeFactory, SpawnOptions,
    },
    errors::ActorError,
    local_typed::LocalBoxedFuture,
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedMessage},
};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
async fn bounded<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(8), f)
        .await
        .expect("contract deadline")
}
#[derive(Default)]
struct Counts {
    built: AtomicUsize,
    dropped: AtomicUsize,
    captures: AtomicUsize,
    owners: Mutex<Vec<std::thread::ThreadId>>,
    seen: Mutex<Vec<u64>>,
}
impl Counts {
    fn owner(&self) {
        self.owners
            .lock()
            .unwrap()
            .push(std::thread::current().id());
    }
}
struct Capture(Arc<Counts>, bool);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.captures.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "late capture drop");
    }
}
struct Probe {
    local: Rc<Cell<usize>>,
    counts: Arc<Counts>,
    events: tokio::sync::mpsc::UnboundedSender<String>,
    init_gate: Option<tokio::sync::oneshot::Receiver<()>>,
    stop_gate: Option<tokio::sync::oneshot::Receiver<()>>,
    fail_hook: bool,
    fail_drop: bool,
}
struct Own;
struct Watch(BoxedActorRef);
impl LocalActor for Probe {
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.owner();
            let local = self.local.clone();
            let _ = self.events.send(format!("init:{}", local.get()));
            if let Some(gate) = self.init_gate.take() {
                let _ = gate.await;
            }
            tokio::task::yield_now().await;
            assert!(Rc::ptr_eq(&local, &self.local));
            self.counts.owner();
            Ok(())
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.owner();
            let local = self.local.clone();
            let _ = self.events.send("hook".into());
            if let Some(gate) = self.stop_gate.take() {
                let _ = gate.await;
            }
            tokio::task::yield_now().await;
            assert!(Rc::ptr_eq(&local, &self.local));
            self.counts.owner();
            if self.fail_hook {
                Err(ActorError::InternalError("first hook error".into()))
            } else {
                Ok(())
            }
        })
    }
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.counts.owner();
            let local = self.local.clone();
            tokio::task::yield_now().await;
            self.counts.owner();
            if message.is::<bool>() {
                return Err(ActorError::InternalError("failed message".into()));
            }
            if message.is::<Own>() {
                return Ok(Box::new(context.actor_ref().unwrap()) as BoxedMessage);
            }
            if message.is::<()>() {
                return Ok(Box::new(local.get()) as BoxedMessage);
            }
            if let Some(number) = message.downcast_ref::<u64>() {
                self.counts.seen.lock().unwrap().push(*number);
            }
            if let Some(Watch(target)) = message.downcast_ref::<Watch>() {
                target
                    .death_watch()
                    .unwrap()
                    .watch(&context.actor_ref().unwrap().death_watch().unwrap())?;
                return Ok(Box::new(()) as BoxedMessage);
            }
            if let Some(event) = message.downcast_ref::<parrot_api::context::ActorTerminated>() {
                let _ = self.events.send(format!("terminated:{}", event.path));
                return Ok(Box::new(()) as BoxedMessage);
            }
            Ok(message)
        })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.owner();
        self.counts.dropped.fetch_add(1, Ordering::SeqCst);
        let _ = self.events.send(format!("drop:{}", self.local.get()));
        assert!(!self.fail_drop, "first actor drop");
    }
}
#[derive(Default)]
struct Gates {
    init: Option<tokio::sync::oneshot::Receiver<()>>,
    stop: Option<tokio::sync::oneshot::Receiver<()>>,
    fail_hook: bool,
    fail_drop: bool,
    fail_capture: bool,
}
fn factory(
    counts: &Arc<Counts>,
    gates: Gates,
) -> (
    LocalRuntimeFactory,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let capture = Capture(counts.clone(), gates.fail_capture);
    let generation = Cell::new(0); // Send and NOT Sync: a required constructor contract.
    let gates = Mutex::new(gates);
    let (events, receive) = tokio::sync::mpsc::unbounded_channel();
    (
        Box::new(move || {
            let capture = &capture;
            capture.0.owner();
            capture.0.built.fetch_add(1, Ordering::SeqCst);
            generation.set(generation.get() + 1);
            let mut gates = gates.lock().unwrap();
            Box::new(Probe {
                local: Rc::new(Cell::new(generation.get())),
                counts: capture.0.clone(),
                events: events.clone(),
                init_gate: gates.init.take(),
                stop_gate: gates.stop.take(),
                fail_hook: gates.fail_hook,
                fail_drop: gates.fail_drop,
            })
        }),
        receive,
    )
}
async fn engine() -> Arc<dyn AsyncEngineRuntime> {
    Arc::new(
        parrot::actix::ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
            arbiter_count: 1,
            mailbox_capacity: 1,
            ..Default::default()
        })
        .await
        .unwrap(),
    )
}
async fn facade() -> parrot::system::ParrotActorSystem {
    parrot::system::ParrotActorSystem::new(Default::default())
        .await
        .unwrap()
}
fn options(path: &str, max: u32) -> FactoryOptions {
    FactoryOptions {
        spawn: SpawnOptions::at(path),
        max_restarts: max,
        ..Default::default()
    }
}
async fn next(events: &mut tokio::sync::mpsc::UnboundedReceiver<String>, expected: &str) {
    assert_eq!(bounded(events.recv()).await.as_deref(), Some(expected));
}
async fn round_trip(runtime: &dyn AsyncEngineRuntime) {
    let counts = Arc::new(Counts::default());
    let (f, mut events) = factory(&counts, Gates::default());
    let actor = runtime
        .local_runtime()
        .unwrap()
        .spawn_local_factory(f, options("/local", 1))
        .await
        .unwrap();
    next(&mut events, "init:1").await;
    let registry = runtime.get_actor("/local").await.unwrap();
    let own = *actor
        .send(Box::new(Own))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    let id = actor.death_watch().unwrap().id();
    for reference in [&actor, &registry, &own] {
        assert_eq!(reference.death_watch().unwrap().id(), id);
        assert_eq!(reference.path(), "/local");
        assert_eq!(
            *reference
                .send(Box::new(42_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
        assert_eq!(
            *reference
                .send(Box::new(String::from("raw")))
                .await
                .unwrap()
                .downcast::<String>()
                .unwrap(),
            "raw"
        );
    }
    actor.deliver(Box::new(7_u64)).await.unwrap();
    actor.send(Box::new(())).await.unwrap();
    assert_eq!(*counts.seen.lock().unwrap(), vec![42, 42, 42, 7]);
    assert!(actor.send(Box::new(false)).await.is_err());
    next(&mut events, "drop:1").await;
    next(&mut events, "init:2").await;
    for reference in [&actor, &registry, &own] {
        assert_eq!(
            *reference
                .send(Box::new(()))
                .await
                .unwrap()
                .downcast::<usize>()
                .unwrap(),
            2
        );
    }
    assert_eq!(
        runtime
            .get_actor("/local")
            .await
            .unwrap()
            .death_watch()
            .unwrap()
            .id(),
        id
    );
    own.stop().await.unwrap();
    assert!(!registry.is_alive().await);
    assert!(runtime.get_actor("/local").await.is_none());
    assert_eq!(counts.built.load(Ordering::SeqCst), 2);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    let owners = counts.owners.lock().unwrap();
    assert_ne!(owners[0], std::thread::current().id());
    assert!(owners.iter().all(|owner| owner == &owners[0]));
}
#[actix::test]
async fn catalog_raw_messages_self_registry_and_restarts_share_one_owner() {
    bounded(async {
        let catalog = AsyncEngineCatalog::new();
        let token = catalog
            .register("custom-local-name", engine().await)
            .unwrap();
        let runtime = catalog.lookup(token.name()).unwrap();
        round_trip(runtime.as_ref()).await;
        runtime.shutdown().await.unwrap();
    })
    .await;
}
#[actix::test]
async fn facade_preserves_local_capability_and_shared_shutdown() {
    bounded(async {
        let facade = facade().await;
        let runtime = engine().await;
        facade
            .register_runtime("local".into(), runtime.clone(), true)
            .unwrap();
        round_trip(&facade).await;
        AsyncEngineRuntime::shutdown(&facade).await.unwrap();
        runtime.shutdown().await.unwrap();
    })
    .await;
}
#[actix::test]
async fn local_death_watch_uses_original_dynamic_event_and_logical_identity() {
    bounded(async {
        let runtime = engine().await;
        let (f, mut events) = factory(&Arc::new(Counts::default()), Gates::default());
        let watcher = runtime
            .local_runtime()
            .unwrap()
            .spawn_local_factory(f, options("/watcher", 0))
            .await
            .unwrap();
        next(&mut events, "init:1").await;
        let (f, mut target_events) = factory(&Arc::new(Counts::default()), Gates::default());
        let target = runtime
            .local_runtime()
            .unwrap()
            .spawn_local_factory(f, options("/target", 1))
            .await
            .unwrap();
        next(&mut target_events, "init:1").await;
        watcher
            .send(Box::new(Watch(target.clone_boxed())))
            .await
            .unwrap();
        assert!(target.send(Box::new(false)).await.is_err());
        next(&mut target_events, "drop:1").await;
        next(&mut target_events, "init:2").await;
        assert!(events.try_recv().is_err());
        target.stop().await.unwrap();
        next(&mut events, "terminated:/target").await;
        runtime.shutdown().await.unwrap();
    })
    .await;
}
#[actix::test]
async fn deadline_and_cancelled_capacity_wait_do_not_deliver_unadmitted_messages() {
    bounded(async {
        let runtime = engine().await;
        let counts = Arc::new(Counts::default());
        let (release, gate) = tokio::sync::oneshot::channel();
        let (f, mut events) = factory(
            &counts,
            Gates {
                init: Some(gate),
                ..Default::default()
            },
        );
        let actor = runtime
            .local_runtime()
            .unwrap()
            .spawn_local_factory(f, options("/busy", 0))
            .await
            .unwrap();
        next(&mut events, "init:1").await;
        actor.deliver(Box::new(1_u64)).await.unwrap();
        let result = actor
            .send_with_timeout(Box::new(2_u64), Some(Duration::from_millis(30)))
            .await;
        assert!(matches!(
            result,
            Err(ActorError::TimeoutDetail(_)) | Err(ActorError::Timeout)
        ));
        let mut pending = actor.deliver(Box::new(3_u64));
        assert!(pending.as_mut().now_or_never().is_none());
        drop(pending);
        release.send(()).unwrap();
        actor.send(Box::new(())).await.unwrap();
        assert_eq!(*counts.seen.lock().unwrap(), vec![1]);
        runtime.shutdown().await.unwrap();
    })
    .await;
}
#[actix::test]
async fn unpolled_factory_inputs_retire_without_invoking_constructor() {
    bounded(async {
        let runtime = engine().await;
        let facade = facade().await;
        facade
            .register_runtime("local".into(), runtime.clone(), true)
            .unwrap();
        for mode in 0..3 {
            let counts = Arc::new(Counts::default());
            let (f, _) = factory(
                &counts,
                Gates {
                    fail_capture: true,
                    ..Default::default()
                },
            );
            let pending = match mode {
                0 => runtime
                    .local_runtime()
                    .unwrap()
                    .spawn_local_factory(f, options("/unused", 0)),
                1 => facade.spawn_local_factory_on("local", f, options("/unused", 0)),
                _ => AsyncLocalRuntime::spawn_local_factory(&facade, f, options("/unused", 0)),
            };
            drop(pending);
            bounded(async {
                while counts.captures.load(Ordering::SeqCst) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert_eq!(counts.built.load(Ordering::SeqCst), 0);
            assert!(runtime.get_actor("/unused").await.is_none());
        }
        AsyncEngineRuntime::shutdown(&facade).await.unwrap();
    })
    .await;
}
#[actix::test]
async fn rejected_inputs_are_disposed_and_cleanup_errors_remain_visible() {
    bounded(async {
        let runtime = engine().await;
        for invalid in [
            options("", 1),
            FactoryOptions {
                within: Duration::ZERO,
                ..options("/zero", 1)
            },
        ] {
            let counts = Arc::new(Counts::default());
            let (f, _) = factory(
                &counts,
                Gates {
                    fail_capture: true,
                    ..Default::default()
                },
            );
            let error = runtime
                .local_runtime()
                .unwrap()
                .spawn_local_factory(f, invalid)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("late capture drop"), "{error}");
            assert_eq!(counts.built.load(Ordering::SeqCst), 0);
            assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
        }
        let (f, _) = factory(&Arc::new(Counts::default()), Gates::default());
        let original = runtime
            .local_runtime()
            .unwrap()
            .spawn_local_factory(f, options("/dup", 0))
            .await
            .unwrap();
        let id = original.death_watch().unwrap().id();
        let counts = Arc::new(Counts::default());
        let (f, _) = factory(&counts, Gates::default());
        assert!(
            runtime
                .local_runtime()
                .unwrap()
                .spawn_local_factory(f, options("/dup", 0))
                .await
                .is_err()
        );
        assert_eq!(counts.built.load(Ordering::SeqCst), 0);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime
                .get_actor("/dup")
                .await
                .unwrap()
                .death_watch()
                .unwrap()
                .id(),
            id
        );
        runtime.shutdown().await.unwrap();
        let (f, _) = factory(&counts, Gates::default());
        assert!(
            runtime
                .local_runtime()
                .unwrap()
                .spawn_local_factory(f, options("/closed", 0))
                .await
                .is_err()
        );
        assert_eq!(counts.built.load(Ordering::SeqCst), 0);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 2);
    })
    .await;
}
#[actix::test]
async fn thread_and_missing_destinations_reject_without_local_construction() {
    bounded(async {
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        assert!(AsyncEngineRuntime::local_runtime(thread.as_ref()).is_none());
        let facade = facade().await;
        facade
            .register_runtime("thread".into(), thread, true)
            .unwrap();
        assert!(facade.local_runtime().is_none());
        for destination in ["thread", "missing"] {
            let counts = Arc::new(Counts::default());
            let (f, _) = factory(
                &counts,
                Gates {
                    fail_capture: true,
                    ..Default::default()
                },
            );
            let error = facade
                .spawn_local_factory_on(destination, f, options("/no", 1))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("late capture drop"), "{error}");
            assert_eq!(counts.built.load(Ordering::SeqCst), 0);
            assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
        }
        AsyncEngineRuntime::shutdown(&facade).await.unwrap();
    })
    .await;
}
#[actix::test]
async fn named_and_default_selection_is_captured_before_polling() {
    bounded(async {
        let facade = facade().await;
        let first = engine().await;
        let second = engine().await;
        facade
            .register_runtime("local".into(), first.clone(), true)
            .unwrap();
        let (a, _) = factory(&Arc::new(Counts::default()), Gates::default());
        let (b, _) = factory(&Arc::new(Counts::default()), Gates::default());
        let named = facade.spawn_local_factory_on("local", a, options("/named", 0));
        let default = facade
            .local_runtime()
            .unwrap()
            .spawn_local_factory(b, options("/default", 0));
        facade
            .register_runtime("local".into(), second.clone(), true)
            .unwrap();
        let a = named.await.unwrap();
        let b = default.await.unwrap();
        for path in ["/named", "/default"] {
            assert!(first.get_actor(path).await.is_some());
            assert!(second.get_actor(path).await.is_none());
        }
        a.stop().await.unwrap();
        b.stop().await.unwrap();
        first.shutdown().await.unwrap();
        AsyncEngineRuntime::shutdown(&facade).await.unwrap();
    })
    .await;
}
#[actix::test]
async fn cancelled_stop_keeps_hook_and_capture_owned_until_actual_retirement() {
    bounded(async {
        let runtime = engine().await;
        let counts = Arc::new(Counts::default());
        let (release, gate) = tokio::sync::oneshot::channel();
        let (f, mut events) = factory(
            &counts,
            Gates {
                stop: Some(gate),
                ..Default::default()
            },
        );
        let actor = runtime
            .local_runtime()
            .unwrap()
            .spawn_local_factory(f, options("/retire", 0))
            .await
            .unwrap();
        next(&mut events, "init:1").await;
        // init event marks entry; a processed message proves init finished.
        actor.send(Box::new(())).await.unwrap();
        let reference = actor.clone_boxed();
        let waiter = tokio::spawn(async move { reference.stop().await });
        next(&mut events, "hook").await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(counts.dropped.load(Ordering::SeqCst), 0);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        actor.stop().await.unwrap();
        assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
        runtime.shutdown().await.unwrap();
    })
    .await;
}
#[actix::test]
async fn local_stop_receipts_keep_first_cleanup_error_through_neutral_reference() {
    bounded(async {
        for actor_drop in [false, true] {
            let runtime = engine().await;
            let counts = Arc::new(Counts::default());
            let (f, mut events) = factory(
                &counts,
                Gates {
                    fail_hook: !actor_drop,
                    fail_drop: actor_drop,
                    fail_capture: true,
                    ..Default::default()
                },
            );
            let actor = runtime
                .local_runtime()
                .unwrap()
                .spawn_local_factory(f, options("/errors", 0))
                .await
                .unwrap();
            next(&mut events, "init:1").await;
            // init event marks entry; a processed message proves init finished.
            actor.send(Box::new(())).await.unwrap();
            let expected = if actor_drop {
                "first actor drop"
            } else {
                "first hook error"
            };
            for _ in 0..2 {
                let error = actor.stop().await.unwrap_err().to_string();
                assert!(error.contains(expected), "{error}");
                assert!(!error.contains("late capture drop"), "{error}");
            }
            assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
            // This actor has retired and left the registry. Shutdown joins
            // current owners; its history is not an aggregate of prior stops.
            assert!(runtime.get_actor("/errors").await.is_none());
            runtime.shutdown().await.unwrap();
        }
    })
    .await;
}
#[actix::test]
async fn reused_path_has_distinct_identity_and_old_reference_cannot_stop_replacement() {
    bounded(async {
        let runtime = engine().await;
        let (f, _) = factory(&Arc::new(Counts::default()), Gates::default());
        let old = runtime
            .local_runtime()
            .unwrap()
            .spawn_local_factory(f, options("/reuse", 0))
            .await
            .unwrap();
        old.send(Box::new(())).await.unwrap();
        old.stop().await.unwrap();
        let (f, _) = factory(&Arc::new(Counts::default()), Gates::default());
        let fresh = runtime
            .local_runtime()
            .unwrap()
            .spawn_local_factory(f, options("/reuse", 0))
            .await
            .unwrap();
        assert_ne!(
            old.death_watch().unwrap().id(),
            fresh.death_watch().unwrap().id()
        );
        old.stop().await.unwrap();
        assert_eq!(
            *fresh
                .send(Box::new(8_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            8
        );
        runtime.shutdown().await.unwrap();
    })
    .await;
}

#[actix::test]
async fn stopping_pending_local_init_retires_state_without_normal_stop_hook() {
    bounded(async {
        let runtime = engine().await;
        let counts = Arc::new(Counts::default());
        let (_release, gate) = tokio::sync::oneshot::channel();
        let (f, mut events) = factory(
            &counts,
            Gates {
                init: Some(gate),
                ..Default::default()
            },
        );
        let actor = runtime
            .local_runtime()
            .unwrap()
            .spawn_local_factory(f, options("/init", 0))
            .await
            .unwrap();
        next(&mut events, "init:1").await;
        actor.stop().await.unwrap();
        next(&mut events, "drop:1").await;
        assert!(events.try_recv().is_err());
        assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
        runtime.shutdown().await.unwrap();
    })
    .await;
}

#[actix::test]
async fn zero_and_finite_restart_budgets_are_honored_through_neutral_options() {
    bounded(async {
        for max in [0, 1] {
            let runtime = engine().await;
            let counts = Arc::new(Counts::default());
            let (f, mut events) = factory(&counts, Gates::default());
            let actor = runtime
                .local_runtime()
                .unwrap()
                .spawn_local_factory(f, options("/budget", max))
                .await
                .unwrap();
            for generation in 1..=max + 1 {
                next(&mut events, &format!("init:{generation}")).await;
                assert!(actor.send(Box::new(false)).await.is_err());
                next(&mut events, &format!("drop:{generation}")).await;
            }
            bounded(async {
                while actor.is_alive().await {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert!(actor.stop().await.is_err());
            assert_eq!(counts.built.load(Ordering::SeqCst), max as usize + 1);
            assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
            assert!(runtime.get_actor("/budget").await.is_none());
            runtime.shutdown().await.unwrap();
        }
    })
    .await;
}
#[actix::test]
async fn constructor_panic_is_reported_after_admission_and_retires_its_capture() {
    bounded(async {
        let runtime = engine().await;
        let counts = Arc::new(Counts::default());
        let capture = Capture(counts.clone(), false);
        let actor = runtime
            .local_runtime()
            .unwrap()
            .spawn_local_factory(
                Box::new(move || {
                    let capture = &capture;
                    capture.0.built.fetch_add(1, Ordering::SeqCst);
                    panic!("local constructor failed")
                }),
                options("/construction", 3),
            )
            .await
            .unwrap();
        bounded(async {
            while actor.is_alive().await {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let error = actor.stop().await.unwrap_err().to_string();
        assert!(error.contains("local constructor failed"), "{error}");
        assert_eq!(counts.built.load(Ordering::SeqCst), 1);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
        assert_eq!(counts.dropped.load(Ordering::SeqCst), 0);
        assert!(runtime.get_actor("/construction").await.is_none());
        runtime.shutdown().await.unwrap();
    })
    .await;
}
