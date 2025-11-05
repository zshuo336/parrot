//! Recursive panic payloads must preserve managed lifecycle and Resume semantics.
use parrot::{
    actix::{ActixActor, ActixActorSystem, ActixContext},
    thread::{ThreadActorConfig, ThreadActorSystem, ThreadContext, config::SupervisorStrategy},
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig, EngineContextHandle},
    address::ActorRef,
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

struct Payload;
impl Drop for Payload {
    fn drop(&mut self) {
        panic!("secondary lifecycle payload destructor");
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Init,
    Message,
    SyncMessage,
    Eligibility,
    Batch,
    Stop,
    Drop,
}
struct Data {
    phase: Phase,
    drops: Arc<AtomicUsize>,
}
impl Data {
    fn message(&self, message: BoxedMessage) -> ActorResult<BoxedMessage> {
        if message.is::<bool>() && matches!(self.phase, Phase::Message | Phase::SyncMessage) {
            std::panic::panic_any(Payload);
        }
        Ok(message)
    }
}
impl Drop for Data {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.phase == Phase::Drop {
            std::panic::panic_any(Payload);
        }
    }
}
struct ThreadProbe(Data);
struct ActixProbe(Data);
macro_rules! actor {
    ($name:ident,$context:ty) => {
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn init<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    if self.0.phase == Phase::Init {
                        std::panic::panic_any(Payload);
                    }
                    Ok(())
                })
            }
            fn use_async_handler(&self) -> bool {
                self.0.phase != Phase::SyncMessage
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { self.0.message(message) })
            }
            fn can_batch_message(&self, message: &BoxedMessage) -> bool {
                if message.is::<bool>() && self.0.phase == Phase::Eligibility {
                    std::panic::panic_any(Payload);
                }
                message.is::<bool>() && self.0.phase == Phase::Batch
            }
            fn receive_batch<'a>(
                &'a mut self,
                _: Vec<BoxedMessage>,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
                Box::pin(async { std::panic::panic_any(Payload) })
            }
            fn before_stop<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    if self.0.phase == Phase::Stop {
                        std::panic::panic_any(Payload);
                    }
                    Ok(())
                })
            }
        }
    };
}
actor!(ThreadProbe, ThreadContext<Self>);
actor!(ActixProbe, ActixContext<ActixActor<Self>>);
impl ActixEngineExt for ActixProbe {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
        _: EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        Some(self.0.message(message))
    }
}
fn isolated(name: &str, body: impl FnOnce()) {
    const FLAG: &str = "PARROT_LIFECYCLE_PAYLOAD_CASE";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(FLAG, name)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: {status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name}: deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn diagnostic(error: impl std::fmt::Display) {
    let text = error.to_string();
    assert!(
        text.contains("panic payload destructor panicked"),
        "lost recursive panic diagnostic: {text}"
    );
}
async fn exercise(reference: &dyn ActorRef, phase: Phase, drops: &AtomicUsize) {
    if phase == Phase::Init {
        tokio::time::timeout(Duration::from_secs(3), async {
            while drops.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed initialization never retired actor");
        for _ in 0..2 {
            diagnostic(reference.stop().await.unwrap_err());
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        return;
    }

    let ping = reference.send(Box::new(42_u32)).await.unwrap();
    assert_eq!(*ping.downcast::<u32>().unwrap(), 42);
    if matches!(
        phase,
        Phase::Message | Phase::SyncMessage | Phase::Eligibility | Phase::Batch
    ) {
        let result = reference
            .send_with_timeout(Box::new(true), Some(Duration::from_secs(3)))
            .await;
        let error = result.unwrap_err();
        assert!(
            matches!(error, ActorError::Panic(_)),
            "wrong handler error: {error}"
        );
        diagnostic(error);
        // Resume keeps this actual instance alive and usable, with no silent restart.
        let reply = reference.send(Box::new(43_u32)).await.unwrap();
        assert_eq!(*reply.downcast::<u32>().unwrap(), 43);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }
    let result = tokio::time::timeout(Duration::from_secs(4), reference.stop())
        .await
        .expect("stop receipt stranded");
    if matches!(phase, Phase::Stop | Phase::Drop) {
        diagnostic(result.unwrap_err());
    } else {
        result.unwrap();
    }
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "stop preceded actual actor disposal"
    );
}
async fn run(actix: bool, phase: Phase) {
    let drops = Arc::new(AtomicUsize::new(0));
    let data = Data {
        phase,
        drops: drops.clone(),
    };
    if actix {
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let reference = system
            .spawn_root_typed_with_strategy(
                ActixProbe(data),
                EmptyConfig,
                parrot_api::supervisor::SupervisorStrategyType::OneForOne(OneForOneStrategy {
                    max_restarts: 1,
                    within: Duration::from_secs(60),
                    decider: BasicDecisionFn::new(|_| SupervisionDecision::Resume),
                }),
            )
            .await
            .unwrap();
        exercise(&*reference, phase, &drops).await;
        system.shutdown().await.unwrap();
    } else {
        let system = ThreadActorSystem::shared(Default::default());
        let reference = system
            .spawn_typed_with_config(
                ThreadProbe(data),
                EmptyConfig,
                ThreadActorConfig {
                    message_batch_size: Some(4),
                    supervisor_strategy: Some(SupervisorStrategy::Resume),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        exercise(&reference, phase, &drops).await;
        system.shutdown_internal().await.unwrap();
    }
}
macro_rules! case {
    ($name:ident,$actix:expr,$phase:ident) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                actix::System::new().block_on(run($actix, Phase::$phase))
            });
        }
    };
}
case!(
    thread_message_secondary_panic_preserves_resume,
    false,
    Message
);
case!(
    thread_batch_eligibility_secondary_panic_replies,
    false,
    Eligibility
);
case!(thread_batch_handler_secondary_panic_replies, false, Batch);
case!(thread_stop_secondary_panic_retires_actor, false, Stop);
case!(
    thread_actor_drop_secondary_panic_retires_storage,
    false,
    Drop
);
case!(
    actix_message_secondary_panic_preserves_resume,
    true,
    Message
);
case!(
    actix_sync_message_secondary_panic_preserves_resume,
    true,
    SyncMessage
);
case!(actix_stop_secondary_panic_retires_actor, true, Stop);
case!(actix_actor_drop_secondary_panic_retires_storage, true, Drop);

case!(
    actix_init_secondary_panic_preserves_stop_receipt,
    true,
    Init
);
