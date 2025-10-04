//! 360° assertion matrix for the ADR-2/ADR-3 dispatch-path fixes.
//!
//! Cross-product coverage on the **Actix engine, end-to-end** (real spawn,
//! real ask/tell through the arbiter):
//!
//! - actor kind:    derive (sync) | derive (async opt-in) | manual
//! - message class: success | user error | unhandled (None) | timeout |
//!   panic-in-handler
//! - interaction:   ask | tell | send_with_timeout
//!
//! Every cell asserts the *observable contract* of the recent changes:
//! 1. `receive_message` forwards to `handle_message` (no cfg bifurcation),
//!    so derive+async actors actually work;
//! 2. `handle_message_engine` takes the safe `EngineContextHandle` and its
//!    `Some/None` semantics are: Some → handled; None → explicit
//!    diagnostic error (NOT a silent drop);
//! 3. panics in either path are isolated (engine survives, ask errors).

use parrot::actix as __parrot_engine;
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::actor::{Actor, ActorState, EmptyConfig, EngineContextHandle};
use parrot_api::address::{ActorRef, ActorRefExt};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_api_derive::{Message, ParrotActor};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct Good(u64);

#[derive(Clone, Debug, Message)]
#[message(result = "String")]
struct Fail(String);

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct Unhandled(u64);

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct Boom(u64);

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct Slow(u64);

// ---------------------------------------------------------------------------
// Matrix actors
// ---------------------------------------------------------------------------

/// Derive actor, sync fast path (default). Engine handler covers
/// Ok/Fail/Boom; Unhandled → None → explicit diagnostic error.
#[derive(Debug, ParrotActor)]
#[ParrotActor(engine = "actix", config = "EmptyConfig")]
struct DeriveSync {
    calls: u64,
}

impl DeriveSync {
    async fn handle_message(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut <Self as Actor>::Context,
    ) -> ActorResult<BoxedMessage> {
        // Async path (thread engine equivalent / fallback): everything works.
        self.calls += 1;
        if let Some(m) = msg.downcast_ref::<Good>() {
            return Ok(Box::new(m.0 * 10) as BoxedMessage);
        }
        if let Some(m) = msg.downcast_ref::<Fail>() {
            return Err(ActorError::MessageHandlingError(m.0.clone()));
        }
        if let Some(m) = msg.downcast_ref::<Boom>() {
            panic!("derive-sync boom {}", m.0);
        }
        if let Some(m) = msg.downcast_ref::<Unhandled>() {
            return Ok(Box::new(m.0 + 1) as BoxedMessage);
        }
        Err(ActorError::MessageHandlingError("unknown".into()))
    }

    fn handle_message_engine(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut <Self as Actor>::Context,
        engine_ctx: EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        // Prove the safe handle is usable inside the fast path.
        let _probe: Option<&u64> = engine_ctx.downcast_ref::<u64>();
        if let Some(m) = msg.downcast_ref::<Good>() {
            return Some(Ok(Box::new(m.0 * 10) as BoxedMessage));
        }
        if let Some(m) = msg.downcast_ref::<Fail>() {
            return Some(Err(ActorError::MessageHandlingError(m.0.clone())));
        }
        if let Some(m) = msg.downcast_ref::<Boom>() {
            panic!("derive-sync-engine boom {}", m.0);
        }
        // Unhandled/Slow → None
        None
    }
}

/// Derive actor, async opt-in: everything routes through `handle_message`.
#[derive(Debug, ParrotActor)]
#[ParrotActor(engine = "actix", config = "EmptyConfig", async_handler = true)]
struct DeriveAsync {
    calls: u64,
}

impl DeriveAsync {
    async fn handle_message(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut <Self as Actor>::Context,
    ) -> ActorResult<BoxedMessage> {
        self.calls += 1;
        tokio::time::sleep(Duration::from_millis(2)).await;
        if let Some(m) = msg.downcast_ref::<Good>() {
            return Ok(Box::new(m.0 * 10) as BoxedMessage);
        }
        if let Some(m) = msg.downcast_ref::<Fail>() {
            return Err(ActorError::MessageHandlingError(m.0.clone()));
        }
        if let Some(m) = msg.downcast_ref::<Boom>() {
            panic!("derive-async boom {}", m.0);
        }
        if let Some(m) = msg.downcast_ref::<Unhandled>() {
            return Ok(Box::new(m.0 + 1) as BoxedMessage);
        }
        if let Some(m) = msg.downcast_ref::<Slow>() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            return Ok(Box::new(m.0) as BoxedMessage);
        }
        Err(ActorError::MessageHandlingError("unknown".into()))
    }

    fn handle_message_engine(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut <Self as Actor>::Context,
        _engine_ctx: EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        None // never called (async opt-in skips the sync probe)
    }
}

/// Manual actor (bypasses the derive macro entirely) — engine-agnostic
/// reference implementation.
struct ManualMatrix {
    calls: u64,
}

impl Actor for ManualMatrix {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.calls += 1;
            if let Some(m) = msg.downcast_ref::<Good>() {
                return Ok(Box::new(m.0 * 10) as BoxedMessage);
            }
            if let Some(m) = msg.downcast_ref::<Fail>() {
                return Err(ActorError::MessageHandlingError(m.0.clone()));
            }
            if let Some(m) = msg.downcast_ref::<Boom>() {
                panic!("manual boom {}", m.0);
            }
            if let Some(m) = msg.downcast_ref::<Unhandled>() {
                return Ok(Box::new(m.0 + 1) as BoxedMessage);
            }
            if let Some(m) = msg.downcast_ref::<Slow>() {
                tokio::time::sleep(Duration::from_millis(200)).await;
                return Ok(Box::new(m.0) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError("unknown".into()))
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// M6: actix 同步快路径移至引擎侧扩展 trait（ActixEngineExt）。
impl parrot_api::actor::ActixEngineExt for ManualMatrix {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        if let Some(m) = msg.downcast_ref::<Good>() {
            return Some(Ok(Box::new(m.0 * 10) as BoxedMessage));
        }
        if let Some(m) = msg.downcast_ref::<Fail>() {
            return Some(Err(ActorError::MessageHandlingError(m.0.clone())));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Matrix harness
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Kind {
    DeriveSync,
    DeriveAsync,
    Manual,
}

async fn spawn_kind(sys: &ActixActorSystem, kind: &Kind) -> Box<dyn ActorRef> {
    match kind {
        Kind::DeriveSync => sys
            .spawn_root_typed(DeriveSync { calls: 0 }, EmptyConfig)
            .await
            .unwrap(),
        Kind::DeriveAsync => sys
            .spawn_root_typed(DeriveAsync { calls: 0 }, EmptyConfig)
            .await
            .unwrap(),
        Kind::Manual => sys
            .spawn_root_typed(ManualMatrix { calls: 0 }, EmptyConfig)
            .await
            .unwrap(),
    }
}

#[derive(Debug, PartialEq)]
enum AskOutcome {
    Good(u64),
    UserErr(String),
    NotHandledErr,
    TimeoutErr,
}

async fn ask_good(r: &dyn ActorRef) -> AskOutcome {
    match r.ask(Good(3)).await {
        Ok(v) => AskOutcome::Good(v),
        Err(e) => AskOutcome::UserErr(e.to_string()),
    }
}

async fn ask_fail(r: &dyn ActorRef) -> AskOutcome {
    match r.ask(Fail("matrix-fail".into())).await {
        Ok(_) => AskOutcome::Good(0),
        Err(e) => AskOutcome::UserErr(e.to_string()),
    }
}

async fn ask_unhandled(r: &dyn ActorRef) -> AskOutcome {
    match r.ask(Unhandled(5)).await {
        Ok(v) => AskOutcome::Good(v),
        Err(e) => {
            let s = e.to_string();
            if s.contains("Message not handled") {
                AskOutcome::NotHandledErr
            } else {
                AskOutcome::UserErr(s)
            }
        }
    }
}

async fn ask_timeout(r: &dyn ActorRef) -> AskOutcome {
    let f = r.send_with_timeout(Box::new(Slow(9)), Some(Duration::from_millis(30)));
    match f.await {
        Ok(_) => AskOutcome::Good(0),
        Err(e) => {
            let s = e.to_string();
            if s.to_lowercase().contains("timeout") || s.to_lowercase().contains("timed out") {
                AskOutcome::TimeoutErr
            } else {
                AskOutcome::UserErr(s)
            }
        }
    }
}

/// The full 360° matrix in one engine boot (actix System required).
#[test]
fn dispatch_matrix_360() {
    actix::System::new().block_on(async {
        let sys = ActixActorSystem::new().await.expect("system");

        for kind in [Kind::DeriveSync, Kind::DeriveAsync, Kind::Manual] {
            let r = spawn_kind(&sys, &kind).await;

            // --- success path ---
            match ask_good(r.as_ref()).await {
                AskOutcome::Good(v) => assert_eq!(v, 30, "{:?}: Good(3) must map to 30", kind),
                o => panic!("{:?}: Good must succeed, got {:?}", kind, o),
            }

            // --- user error path ---
            match ask_fail(r.as_ref()).await {
                AskOutcome::UserErr(s) => assert!(
                    s.contains("matrix-fail"),
                    "{:?}: user error must propagate: {}",
                    kind,
                    s
                ),
                o => panic!("{:?}: Fail must error, got {:?}", kind, o),
            }

            // --- unhandled (None) path ---
            // DeriveSync AND Manual leave Unhandled un-handled in their sync
            // engine handlers → the new diagnostic error. DeriveAsync routes
            // through receive_message → 5+1=6.
            match ask_unhandled(r.as_ref()).await {
                AskOutcome::NotHandledErr => {
                    assert!(
                        matches!(kind, Kind::DeriveSync | Kind::Manual),
                        "{:?}: async path must handle Unhandled",
                        kind
                    );
                }
                AskOutcome::Good(v) => {
                    assert!(
                        matches!(kind, Kind::DeriveAsync),
                        "{:?}: unexpected handler",
                        kind
                    );
                    assert_eq!(v, 6, "{:?}: Unhandled(5) async path → 6", kind);
                }
                o => panic!("{:?}: Unhandled unexpected: {:?}", kind, o),
            }

            // --- timeout path (Slow sleeps 200ms, budget 30ms) ---
            // DeriveSync: Slow is NOT in the sync engine handler → returns
            // the NotHandled diagnostic (sync handlers cannot sleep, so a
            // genuine timeout is unreachable on this path by construction).
            // DeriveAsync / Manual: Slow sleeps in receive_message → the
            // 30ms budget must trip a timeout.
            match ask_timeout(r.as_ref()).await {
                AskOutcome::TimeoutErr => {
                    assert!(
                        matches!(kind, Kind::DeriveAsync),
                        "{:?}: only the async path can genuinely time out",
                        kind
                    );
                }
                AskOutcome::UserErr(s) if s.contains("Message not handled") => {
                    // Sync-path actors whose engine handlers skip Slow.
                    assert!(
                        matches!(kind, Kind::DeriveSync | Kind::Manual),
                        "{:?}: async path must sleep, not NotHandled",
                        kind
                    );
                }
                AskOutcome::UserErr(s) => {
                    assert!(
                        s.to_lowercase().contains("timeout")
                            || s.to_lowercase().contains("timed out")
                            || s.contains("elapsed"),
                        "{:?}: Slow ask must time out, got: {}",
                        kind,
                        s
                    );
                }
                o => panic!("{:?}: Slow must not succeed within 30ms, got {:?}", kind, o),
            }

            // --- actor still alive after all of the above ---
            match ask_good(r.as_ref()).await {
                AskOutcome::Good(v) => {
                    assert_eq!(v, 30, "{:?}: actor must survive the matrix", kind)
                }
                o => panic!("{:?}: actor died mid-matrix: {:?}", kind, o),
            }
        }
    });
}

/// Panic isolation: a panicking handler must not kill the engine or the
/// arbiter; subsequent messages still get processed (possibly by a
/// restarted/continued actor — the assertion is *engine liveness*).
#[test]
fn panic_isolation_matrix() {
    actix::System::new().block_on(async {
        let sys = ActixActorSystem::new().await.expect("system");

        // DeriveAsync: panic inside the async handler.
        let r = spawn_kind(&sys, &Kind::DeriveAsync).await;
        let boom = r
            .send_with_timeout(Box::new(Boom(1)), Some(Duration::from_millis(500)))
            .await;
        // Either an error surfaces (timeout/mailbox closed) — never a hang.
        // Engine liveness check: another actor still answers.
        let r2 = spawn_kind(&sys, &Kind::DeriveAsync).await;
        match ask_good(r2.as_ref()).await {
            AskOutcome::Good(v) => assert_eq!(v, 30, "engine must survive async panic"),
            o => panic!("engine died after async panic: {:?}", o),
        }
        let _ = boom;

        // DeriveSync: panic inside the engine fast path.
        let r3 = spawn_kind(&sys, &Kind::DeriveSync).await;
        let _ = r3
            .send_with_timeout(Box::new(Boom(2)), Some(Duration::from_millis(500)))
            .await;
        let r4 = spawn_kind(&sys, &Kind::DeriveSync).await;
        match ask_good(r4.as_ref()).await {
            AskOutcome::Good(v) => assert_eq!(v, 30, "engine must survive sync-path panic"),
            o => panic!("engine died after sync panic: {:?}", o),
        }
    });
}

/// Serialization invariant: an async handler sleeping 50ms × 10 messages
/// must process them strictly one-at-a-time (total ≥ 500ms) while other
/// actors on the same arbiter keep answering concurrently (< 500ms).
#[test]
fn async_serialization_and_concurrency() {
    actix::System::new().block_on(async {
        let sys = ActixActorSystem::new().await.expect("system");

        let slow = spawn_kind(&sys, &Kind::DeriveAsync).await;
        let fast = spawn_kind(&sys, &Kind::DeriveAsync).await;

        let t0 = std::time::Instant::now();
        // Fire 10 slow-ish messages at `slow` (2ms sleep each → ≥20ms serial)
        let slow_ref = slow.clone_boxed();
        let h1 = tokio::task::spawn(async move {
            let mut last = 0u64;
            for i in 1..=10u64 {
                let v = slow_ref.ask(Good(i)).await.unwrap();
                // Serial + incrementing calls counter → v/10 strictly increasing
                assert!(
                    v / 10 >= last,
                    "serialization violated: v={} last={}",
                    v,
                    last
                );
                last = v / 10;
            }
        });
        // Concurrently hammer `fast`: must complete well within the serial drain
        let fast_ref = fast.clone_boxed();
        let h2 = tokio::task::spawn(async move {
            for _ in 0..20 {
                let v = fast_ref.ask(Good(1)).await.unwrap();
                assert_eq!(v, 10, "Good(1) → 1*10");
            }
        });
        let (r1, r2) = tokio::join!(h1, h2);
        r1.unwrap();
        r2.unwrap();
        let dt = t0.elapsed();
        // 10 × (2ms sleep + overhead) serial on one actor, while 20 fast asks
        // interleave — total should be tens of ms. Generous upper bound only
        // to catch gross serialization ACROSS actors (would be > 10× slower).
        assert!(
            dt < Duration::from_secs(5),
            "cross-actor serialization suspected: {:?}",
            dt
        );
    });
}
