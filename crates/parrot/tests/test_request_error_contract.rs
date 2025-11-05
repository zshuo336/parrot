//! Dynamic reference contracts must not depend on the engine or a retained mailbox.
use parrot::actix::{
    actor::ActixActor,
    context::ActixContext,
    system::{ActixActorSystem, ActixSystemSettings},
};
use parrot::thread::{
    config::{BackpressureStrategy, SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::time::Duration;
struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
}
#[tokio::test]
async fn stopped_thread_references_preserve_the_lifecycle_error_across_all_send_paths() {
    let mut errors = Vec::new();
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 32,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "request-contract".into(),
        },
    ] {
        for retained in [false, true] {
            let sys = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
            let actor = sys
                .spawn_at(
                    Echo,
                    "/retired",
                    None,
                    ThreadActorConfig {
                        scheduling_mode: Some(mode.clone()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            actor
                .send_with_timeout(Box::new(1u64), Some(Duration::from_secs(1)))
                .await
                .unwrap();
            let mailbox = retained.then(|| sys.get_mailbox("/retired").unwrap());
            actor.stop().await.unwrap();
            let results = [
                ("send", actor.send(Box::new(1u64)).await.map(|_| ())),
                (
                    "send-none",
                    actor
                        .send_with_timeout(Box::new(1u64), None)
                        .await
                        .map(|_| ()),
                ),
                (
                    "send-deadline",
                    actor
                        .send_with_timeout(Box::new(1u64), Some(Duration::from_secs(1)))
                        .await
                        .map(|_| ()),
                ),
                ("deliver", actor.deliver(Box::new(1u64)).await),
                ("send-msg", actor.send_msg(Box::new(1u64)).await),
                (
                    "send-strategy",
                    actor
                        .send_with_strategy(Box::new(1u64), BackpressureStrategy::Block)
                        .await,
                ),
                (
                    "priority",
                    actor
                        .send_with_priority(Box::new(1u64), BackpressureStrategy::Block, true)
                        .await,
                ),
                ("ask", actor.ask(Box::new(1u64)).await.map(|_| ())),
            ];
            for (name, result) in results {
                if !matches!(result, Err(ActorError::Stopped)) {
                    errors.push(format!("{mode:?}, retained={retained}, {name}: {result:?}"));
                }
            }
            let alive = actor.is_alive().await;
            sys.shutdown_internal().await.unwrap();
            drop(mailbox);
            assert!(!alive);
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}
struct Slow;
impl ActixEngineExt for Slow {}
impl Actor for Slow {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            Ok(msg)
        })
    }
}
#[actix::test]
async fn dynamic_actix_send_without_a_deadline_does_not_inherit_the_context_ask_timeout() {
    let mut failures = Vec::new();
    for factory in [false, true] {
        let sys = ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            ask_timeout: Duration::from_millis(10),
            ..Default::default()
        })
        .await
        .unwrap();
        let actor = if factory {
            sys.spawn_root_factory(|| Slow).await.unwrap()
        } else {
            sys.spawn_root_typed(Slow, EmptyConfig).await.unwrap()
        };
        for explicit_none in [false, true] {
            let request = if explicit_none {
                actor.send_with_timeout(Box::new(7u64), None)
            } else {
                actor.send(Box::new(7u64))
            };
            let reply = tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .unwrap();
            if !matches!(reply, Ok(value) if value.downcast_ref::<u64>() == Some(&7)) {
                failures.push(format!("factory={factory}, explicit_none={explicit_none}: unbounded request did not return its value"));
            }
        }
        let bounded = actor
            .send_with_timeout(Box::new(7u64), Some(Duration::from_millis(5)))
            .await;
        let stopped = sys.shutdown().await;
        assert!(matches!(
            bounded,
            Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
        ));
        stopped.unwrap();
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
