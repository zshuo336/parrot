//! Integration & scenario tests for the ADR-2/ADR-3 fixes on the **thread
//! engine** — the engine whose primary path (`receive_message`) was broken
//! for derive actors before the fix.
//!
//! Scenarios:
//! 1. actor-to-actor ASK forwarding through a registered relay's mailbox;
//! 2. lifecycle: init → running → many messages → stop → post-stop send;
//! 3. concurrent producers hammering one actor: no lost/duplicated updates;
//! 4. mixed message kinds across multiple actors in one system;
//! 5. panic receipts and continued service on the same shared worker;
//! 6. derive and manual actors process identical requests through real mailboxes.

use parrot::thread as __parrot_engine;
use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRefExt;
use parrot_api::errors::ActorError;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_api_derive::{Message, ParrotActor};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct Add(u64);

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct Get;

#[derive(Clone, Debug, Message)]
#[message(result = "String")]
struct Ping(String);

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct Forward(u64);

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct GetHops;

// ---------------------------------------------------------------------------
// Actors
// ---------------------------------------------------------------------------

/// Async-style actor (the `receive_message` path the thread engine uses).
#[derive(Debug, Default)]
struct Counter {
    value: u64,
    pings: Vec<String>,
}

impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(m) = msg.downcast_ref::<Add>() {
                self.value += m.0;
                return Ok(Box::new(self.value) as BoxedMessage);
            }
            if msg.downcast_ref::<Get>().is_some() {
                return Ok(Box::new(self.value) as BoxedMessage);
            }
            if let Some(p) = msg.downcast_ref::<Ping>() {
                self.pings.push(p.0.clone());
                return Ok(Box::new(format!("pong:{}", p.0)) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "unknown".to_string(),
            ))
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// Relay actor: forwards Add messages to a target actor (ask), proving
/// actor-to-actor ask composition on the thread engine.
#[derive(Debug)]
struct Relay {
    target: Box<dyn parrot_api::address::ActorRef>,
    hops: u64,
}

impl Actor for Relay {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(Forward(value)) = message.downcast_ref::<Forward>() {
                let reply = self.target.ask(Add(*value)).await?;
                self.hops += 1;
                return Ok(Box::new(reply) as BoxedMessage);
            }
            if message.is::<GetHops>() {
                return Ok(Box::new(self.hops) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError(
                "unknown relay message".into(),
            ))
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn spawn(ts: &Arc<ThreadActorSystem>, path: &str) -> Box<dyn parrot_api::address::ActorRef> {
    Box::new(
        ts.spawn_at::<Counter>(Counter::default(), path, None, ThreadActorConfig::default())
            .await
            .expect("spawn counter"),
    )
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Scenario 1: actor-to-actor ask chain (relay → counter).
#[tokio::test]
async fn scenario_actor_to_actor_ask_chain() {
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    let counter = spawn(&ts, "/scenario/counter").await;
    let relay: Box<dyn parrot_api::address::ActorRef> = Box::new(
        ts.spawn_at::<Relay>(
            Relay {
                target: counter.clone_boxed(),
                hops: 0,
            },
            "/scenario/relay",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .expect("spawn relay"),
    );

    // Direct asks establish the baseline.
    assert_eq!(counter.ask(Add(5)).await.unwrap(), 5);

    // Actor-to-actor: relay asks counter and returns the counter's reply.
    let v = tokio::time::timeout(Duration::from_secs(3), relay.ask(Forward(10)))
        .await
        .expect("relay request must complete")
        .expect("relay must forward the request");
    assert_eq!(v, 15, "relay must observe counter's updated value");
    assert_eq!(relay.ask(GetHops).await.unwrap(), 1);
    assert_eq!(counter.ask(Get).await.unwrap(), 15);

    // Second hop accumulates.
    let v2 = tokio::time::timeout(Duration::from_secs(3), relay.ask(Forward(10)))
        .await
        .expect("second relay request must complete")
        .expect("relay must forward the second request");
    assert_eq!(v2, 25);
    assert_eq!(relay.ask(GetHops).await.unwrap(), 2);
    assert_eq!(counter.ask(Get).await.unwrap(), 25);
    ts.shutdown_internal()
        .await
        .expect("relay system shutdown must succeed");
}

/// Scenario 2: lifecycle — messages processed, stop, then post-stop send
/// must immediately report Stopped after successful retirement.
#[tokio::test]
async fn scenario_lifecycle_stop_semantics() {
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    let c = spawn(&ts, "/lifecycle/c1").await;
    for i in 1..=10u64 {
        assert_eq!(c.ask(Add(i)).await.unwrap(), (1..=i).sum::<u64>());
    }
    // Stop via the generic ActorRef trait.
    c.stop().await.expect("stop must succeed");

    let result = tokio::time::timeout(Duration::from_secs(3), c.ask(Get))
        .await
        .expect("post-stop ask must terminate");
    assert!(
        matches!(result, Err(ActorError::Stopped)),
        "post-stop ask must report Stopped, got {result:?}"
    );
    let delivery = tokio::time::timeout(Duration::from_secs(3), c.deliver(Box::new(Add(100))))
        .await
        .expect("post-stop delivery must terminate");
    assert!(
        matches!(delivery, Err(ActorError::Stopped)),
        "post-stop delivery must report Stopped, got {delivery:?}"
    );
    assert!(
        !c.is_alive().await,
        "successful stop must retire the reference"
    );
    assert_eq!(
        ts.actor_count(),
        0,
        "successful stop must unregister the actor"
    );
    ts.shutdown_internal()
        .await
        .expect("lifecycle system shutdown must succeed");
}

/// Scenario 3: 8 concurrent producers × 125 adds each = 1000 messages, no
/// lost updates (final value exactly 1000 × avg).
#[tokio::test]
async fn scenario_concurrent_no_lost_updates() {
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    let c = spawn(&ts, "/concurrent/c").await;

    const PRODUCERS: u64 = 8;
    const PER: u64 = 125;
    let c = Arc::new(c);
    let mut handles = Vec::new();
    for p in 0..PRODUCERS {
        let c = c.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..PER {
                let _ = c.ask(Add(1)).await.expect("add must succeed");
                if i % 37 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            p
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let final_v = c.ask(Get).await.unwrap();
    assert_eq!(
        final_v,
        PRODUCERS * PER,
        "every add must be applied exactly once"
    );
    ts.shutdown_internal()
        .await
        .expect("producer system shutdown must succeed");
}

/// Scenario 4: many actors, mixed load, interleaved — final states exact.
#[tokio::test]
async fn scenario_multi_actor_mixed_load() {
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    const N: u64 = 16;
    let mut refs = Vec::new();
    for i in 0..N {
        refs.push(spawn(&ts, &format!("/mixed/c{}", i)).await);
    }
    // Round-robin mixed pings and adds.
    for round in 0..10u64 {
        for (i, r) in refs.iter().enumerate() {
            let tag = format!("r{}a{}", round, i);
            let pong = r.ask(Ping(tag)).await.unwrap();
            assert_eq!(pong, format!("pong:r{}a{}", round, i));
            let expected: u64 = (1..=(round + 1)).sum();
            assert_eq!(r.ask(Add(round + 1)).await.unwrap(), expected);
        }
    }
    // Each actor saw exactly 10 adds of round+1 → sum 1..=10 per actor.
    let expect: u64 = (1..=10).sum();
    for r in &refs {
        assert_eq!(r.ask(Get).await.unwrap(), expect);
    }
    ts.shutdown_internal()
        .await
        .expect("mixed-load system shutdown must succeed");
}

/// Scenario 5: panic isolation on the thread engine — a panicking actor
/// must not take down workers or other actors.
#[derive(Debug, Default)]
struct SometimesPanic {
    count: u64,
}

impl Actor for SometimesPanic {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(m) = msg.downcast_ref::<Add>() {
                self.count += m.0;
                if m.0 == 13 {
                    panic!("thread-engine boom");
                }
                return Ok(Box::new(self.count) as BoxedMessage);
            }
            Ok(msg)
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[tokio::test]
async fn scenario_thread_panic_isolation() {
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    let victim: Box<dyn parrot_api::address::ActorRef> = Box::new(
        ts.spawn_at::<SometimesPanic>(
            SometimesPanic::default(),
            "/panic/victim",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .expect("spawn victim"),
    );
    let bystander = spawn(&ts, "/panic/bystander").await;

    // Normal traffic before the panic.
    assert_eq!(victim.ask(Add(1)).await.unwrap(), 1);

    // Trigger a panic inside the actor's handler (batch panic isolation in
    // the processor catches it). Preserve the actual panic receipt.
    let failed = tokio::time::timeout(Duration::from_secs(3), victim.ask(Add(13)))
        .await
        .expect("panic request must produce a terminal receipt");
    assert!(
        matches!(&failed, Err(ActorError::Panic(message)) if message.contains("thread-engine boom")),
        "handler panic must preserve its cause, got {failed:?}"
    );

    // Bystander unaffected.
    let v = tokio::time::timeout(Duration::from_secs(3), bystander.ask(Add(7)))
        .await
        .expect("engine must survive actor panic");
    assert_eq!(v.unwrap(), 7);
    ts.shutdown_internal()
        .await
        .expect("panic-isolation system shutdown must succeed");
}

#[derive(Debug, Default, ParrotActor)]
#[ParrotActor(engine = "thread", config = "EmptyConfig")]
struct DerivedCounter {
    value: u64,
}

impl DerivedCounter {
    async fn handle_message(
        &mut self,
        message: BoxedMessage,
        _ctx: &mut ThreadContext<Self>,
    ) -> ActorResult<BoxedMessage> {
        if let Some(Add(value)) = message.downcast_ref::<Add>() {
            self.value += *value;
            return Ok(Box::new(self.value));
        }
        if message.is::<Get>() {
            return Ok(Box::new(self.value));
        }
        if let Some(Ping(value)) = message.downcast_ref::<Ping>() {
            return Ok(Box::new(format!("pong:{value}")));
        }
        Err(ActorError::MessageHandlingError("unknown".into()))
    }
}

/// Scenario 6: compare actual derive and manual actors, with an independent
/// arithmetic reference so matching wrong outputs cannot establish equivalence.
#[tokio::test]
async fn scenario_dispatch_equivalence_manual_twin() {
    // Continuing with the same state after a handler error is a Resume
    // contract. Default Restart cannot reconstruct instance-spawned actors.
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_supervisor_strategy: parrot::thread::config::SupervisorStrategy::Resume,
        ..Default::default()
    });
    let c = spawn(&ts, "/equiv/twin").await;
    let derived: Box<dyn parrot_api::address::ActorRef> = Box::new(
        ts.spawn_at::<DerivedCounter>(
            DerivedCounter::default(),
            "/equiv/derived",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .expect("spawn derived actor"),
    );
    for i in 1..=25u64 {
        let manual = c.ask(Add(i)).await.unwrap();
        let generated = derived
            .ask(Add(i))
            .await
            .expect("derived actor must dispatch Add");
        assert_eq!(manual, (1..=i).sum::<u64>());
        assert_eq!(generated, manual, "derive and manual replies must agree");
        let ping = format!("iteration-{i}");
        assert_eq!(
            derived.ask(Ping(ping.clone())).await.unwrap(),
            format!("pong:{ping}")
        );
        assert_eq!(
            c.ask(Ping(ping.clone())).await.unwrap(),
            format!("pong:{ping}")
        );
        assert_eq!(derived.ask(Get).await.unwrap(), generated);
    }
    assert_eq!(c.ask(Get).await.unwrap(), 325);
    assert_eq!(derived.ask(Get).await.unwrap(), 325);
    for actor in [&c, &derived] {
        let unknown = actor.ask(Forward(1000)).await;
        assert!(
            matches!(unknown, Err(ActorError::MessageHandlingError(ref cause)) if cause == "unknown")
        );
        assert_eq!(
            actor
                .ask(Get)
                .await
                .expect("Resume must keep actor available after rejected message"),
            325,
            "rejected message must preserve state"
        );
        assert_eq!(
            actor.ask(Add(7)).await.unwrap(),
            332,
            "resumed actor must process subsequent work exactly once"
        );
    }
    ts.shutdown_internal()
        .await
        .expect("dispatch system shutdown must succeed");
}
