use parrot::{
    system::{ActorSystemImpl, ParrotActorSystem},
    thread::{
        SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig,
        ThreadContext,
        system::{ErasedSpawnBox, ErasedSpawnable, TypedSpawnPayload},
    },
};
use parrot_api::{
    actor::{Actor, ActorConfig, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, InstanceConfig, RuntimeActor},
    errors::ActorError,
    system::{ActorSystem, SystemError},
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[path = "support/lifecycle_identity.rs"]
mod lifecycle_identity;

type Events = Arc<Mutex<Vec<(&'static str, Option<TraceContext>)>>>;

fn note(events: &Events, phase: &'static str) {
    events
        .lock()
        .unwrap()
        .push((phase, TraceContext::current()));
}

struct Config(Events);
impl ActorConfig for Config {}
impl Drop for Config {
    fn drop(&mut self) {
        note(&self.0, "config-drop");
    }
}

struct Probe {
    events: Events,
    fail: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        note(&self.events, "actor-drop");
    }
}
impl Actor for Probe {
    type Config = InstanceConfig;
    type Context = dyn ActorContext;

    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        note(&self.events, "init-call");
        Box::pin(async move {
            note(&self.events, "init-poll");
            tokio::task::yield_now().await;
            note(&self.events, "init-resumed");
            if self.fail {
                Err(ActorError::InitializationError(
                    "deferred root initialization rejected".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(Box::new(TraceContext::current()) as BoxedMessage) })
    }
}

struct Native(Probe);
impl Actor for Native {
    type Config = Config;
    type Context = ThreadContext<Self>;

    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.init(ctx)
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.receive_message(message, ctx)
    }
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    SystemTrait,
    SystemImpl,
    Runtime,
    Facade,
    Erased,
    ErasedPath,
    Payload,
    PayloadPath,
}
impl Entry {
    fn has_config(self) -> bool {
        !matches!(self, Self::ErasedPath | Self::PayloadPath)
    }
}

async fn scenario(entry: Entry, mode: SchedulingMode, traced: bool, polling: bool, fail: bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            default_scheduling_mode: mode.clone(),
            shutdown_timeout: Duration::from_secs(5),
            ..Default::default()
        });
        let implementation = ActorSystemImpl::Thread(system.clone());
        let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
        facade
            .register_runtime("thread".into(), system.clone(), true)
            .unwrap();
        let events = Events::default();
        let origin = traced.then(TraceContext::root);
        let ambient = polling.then(TraceContext::root);
        let actor = Probe {
            events: events.clone(),
            fail,
        };
        let work: BoxedFuture<'_, Result<BoxedActorRef, SystemError>> =
            trace::in_scope(origin.clone(), || {
                let engine = ThreadActorConfig {
                    scheduling_mode: Some(mode.clone()),
                    ..Default::default()
                };
                match entry {
                    Entry::SystemTrait | Entry::SystemImpl | Entry::Runtime | Entry::Facade => {
                        let actor: RuntimeActor = Box::new(actor);
                        let config: InstanceConfig = Box::new(Config(events.clone()));
                        match entry {
                            Entry::SystemTrait => <ThreadActorSystem as ActorSystem>::spawn_root_boxed(
                                &system, actor, config,
                            ),
                            Entry::SystemImpl => implementation.spawn_root_boxed(actor, config),
                            Entry::Runtime => system.spawn(actor, config, Default::default()),
                            Entry::Facade => <ParrotActorSystem as ActorSystem>::spawn_root_boxed(
                                &facade, actor, config,
                            ),
                            _ => unreachable!(),
                        }
                    }
                    entry => {
                        let native = Native(actor);
                        let native_work = match entry {
                            Entry::Erased => system.spawn_erased_actor(
                                ErasedSpawnBox::from_typed_boxed(native),
                                Box::new(Config(events.clone())),
                                None,
                            ),
                            Entry::ErasedPath => ErasedSpawnBox::from_typed(native).spawn_at_path(
                                system.clone(), "/deferred-root".into(), engine,
                            ),
                            Entry::Payload => Box::new(TypedSpawnPayload::new(native)).spawn_on(
                                system.clone(), Box::new(Config(events.clone())), engine,
                            ),
                            Entry::PayloadPath => Box::new(TypedSpawnPayload::new(native))
                                .spawn_on_path(system.clone(), "/deferred-root".into(), engine),
                            _ => unreachable!(),
                        };
                        Box::pin(async move { native_work.await.map_err(SystemError::ActorError) })
                    }
                }
            });
        assert_eq!(system.actor_count(), 0);
        assert!(events.lock().unwrap().is_empty());
        let result = trace::scope(ambient.clone(), work).await;
        assert!(TraceContext::current().is_none());
        let reference = result.unwrap();
        if matches!(entry, Entry::ErasedPath | Entry::PayloadPath) {
            assert_eq!(reference.path(), "/deferred-root");
        }
        if fail {
            // Thread creation publishes before asynchronous initialization completes.
            while system.actor_count() != 0 {
                tokio::task::yield_now().await;
            }
            assert!(reference.send(Box::new(())).await.is_err());
        } else {
            let work = trace::in_scope(None, || reference.send(Box::new(())));
            let observed = trace::scope(ambient.clone(), work)
                .await
                .unwrap()
                .downcast::<Option<TraceContext>>()
                .unwrap();
            assert!(observed.is_none(), "creation trace leaked into a later message");
            let message = TraceContext::root();
            let work = message.in_scope(|| reference.send(Box::new(())));
            let observed = trace::scope(ambient.clone(), work)
                .await
                .unwrap()
                .downcast::<Option<TraceContext>>()
                .unwrap();
            lifecycle_identity::assert_child(observed.as_ref().as_ref(), Some(&message));
            trace::scope(ambient.clone(), reference.stop()).await.unwrap();
        }
        facade.internal_shutdown().await.unwrap();
        assert_eq!(system.actor_count(), 0);
        assert!(TraceContext::current().is_none());
        let rows = events.lock().unwrap().clone();
        let phase = |name| {
            let values: Vec<_> = rows
                .iter()
                .filter(|(phase, _)| *phase == name)
                .map(|(_, context)| context.clone())
                .collect();
            assert_eq!(values.len(), 1, "phase {name} must occur once: {rows:?}");
            values[0].clone()
        };
        if entry.has_config() {
            assert_eq!(phase("config-drop"), origin, "config retirement borrowed polling identity");
        } else {
            assert!(!rows.iter().any(|(phase, _)| *phase == "config-drop"));
        }
        let initialization = phase("init-call");
        lifecycle_identity::assert_child(initialization.as_ref(), origin.as_ref());
        assert_eq!(phase("init-poll"), initialization);
        assert_eq!(phase("init-resumed"), initialization);
        phase("actor-drop");
        println!(
            "PARROT_DEFERRED_ROOT_IDENTITY_OK entry={entry:?} mode={mode:?} traced={traced} polling={polling} fail={fail}"
        );
    })
    .await
    .expect("deferred root identity independent liveness bound");
}

async fn matrix(entry: Entry) {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 8,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "deferred-root".into(),
        },
    ] {
        for traced in [false, true] {
            for polling in [false, true] {
                for fail in [false, true] {
                    scenario(entry, mode.clone(), traced, polling, fail).await;
                }
            }
        }
    }
}

macro_rules! contract {
    ($name:ident, $entry:ident) => {
        #[tokio::test]
        async fn $name() {
            matrix(Entry::$entry).await;
        }
    };
}
contract!(
    legacy_thread_root_preserves_invocation_identity,
    SystemTrait
);
contract!(enum_thread_root_preserves_invocation_identity, SystemImpl);
contract!(runtime_thread_root_preserves_invocation_identity, Runtime);
contract!(facade_thread_root_preserves_invocation_identity, Facade);
contract!(erased_thread_root_preserves_invocation_identity, Erased);
contract!(
    erased_path_thread_root_preserves_invocation_identity,
    ErasedPath
);
contract!(payload_thread_root_preserves_invocation_identity, Payload);
contract!(
    payload_path_thread_root_preserves_invocation_identity,
    PayloadPath
);
