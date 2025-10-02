//! Shared harness for the Akka-parity suite.

use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRefExt;
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use parrot_api_derive::Message;
use std::future::Future;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
pub struct IncN(pub u64);

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
pub struct GetTotal;

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
pub struct EchoU64(pub u64);

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
pub struct SlowEchoU64(pub u64);

// ---------------------------------------------------------------------------
// Actors
// ---------------------------------------------------------------------------

/// Plain counter used across scenarios.
#[derive(Debug, Default)]
pub struct Counter {
    total: u64,
}

impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(i) = msg.downcast_ref::<IncN>() {
                self.total += i.0;
                return Ok(Box::new(self.total) as BoxedMessage);
            }
            if msg.downcast_ref::<GetTotal>().is_some() {
                return Ok(Box::new(self.total) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError("unknown".into()))
        })
    }

    fn receive_message_with_engine<'a>(
        &'a mut self,
        _msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _e: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        None
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// Sleeps for SlowEchoU64.0 ms before replying (timeout scenarios).
#[derive(Debug, Default)]
pub struct Slowpoke;

impl Actor for Slowpoke {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(e) = msg.downcast_ref::<EchoU64>() {
                return Ok(Box::new(e.0) as BoxedMessage);
            }
            if let Some(s) = msg.downcast_ref::<SlowEchoU64>() {
                tokio::time::sleep(Duration::from_millis(s.0)).await;
                return Ok(Box::new(s.0) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError("unknown".into()))
        })
    }

    fn receive_message_with_engine<'a>(
        &'a mut self,
        _msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _e: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        None
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// Minimal watched-target actor: replies "pong" to Ping, ignores the rest.
#[derive(Debug, Default)]
pub struct SimpleWatched {
    pub pings: u64,
}

impl Actor for SimpleWatched {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.downcast_ref::<Ping>().is_some() {
                self.pings += 1;
                return Ok(Box::new("pong".to_string()) as BoxedMessage);
            }
            // Engine-internal messages (e.g. Terminated notices to others)
            // are ignored by this actor.
            Ok(Box::new("ignored".to_string()) as BoxedMessage)
        })
    }

    fn receive_message_with_engine<'a>(
        &'a mut self,
        _msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _e: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        None
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[derive(Clone, Debug, Message)]
#[message(result = "String")]
pub struct Ping;

/// The `SimpleActor` trait alias used by some tests: sync-probe dispatch.
pub trait SimpleActor: Sized + Actor<Context = ThreadContext<Self>> + Send + Sync + 'static {
    fn handle(&mut self, msg: &BoxedMessage) -> Option<ActorResult<BoxedMessage>>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let r = self.handle(&msg);
        Box::pin(async move {
            match r {
                Some(res) => res,
                None => Ok(msg), // pass-through by default
            }
        })
    }

    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _e: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        self.handle(&msg)
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// ---------------------------------------------------------------------------
// System helpers
// ---------------------------------------------------------------------------

pub fn mk_system() -> std::sync::Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig::default())
}

/// Config with dedicated-thread scheduler capacity enabled.
pub fn mk_system_with_dedicated() -> std::sync::Arc<ThreadActorSystem> {
    let mut cfg = ThreadActorSystemConfig::default();
    cfg.max_dedicated_threads = 8;
    ThreadActorSystem::shared(cfg)
}

/// Poll `cond` until it holds or the deadline lapses (fails otherwise).
pub async fn eventually<F, Fut>(timeout: Duration, cond: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if cond().await {
            return;
        }
        assert!(Instant::now() < deadline, "condition not met within {:?}", timeout);
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

/// Box a typed thread ref into the erased ref type.
pub fn erased(r: impl parrot_api::address::ActorRef + 'static) -> BoxedActorRef {
    Box::new(r)
}

/// Typed ask through the extension trait, bypassing the inherent
/// BoxedMessage-based `ask` on ThreadActorRef.
pub async fn ask<M>(r: &impl parrot_api::address::ActorRef, m: M) -> ActorResult<M::Result>
where
    M: Message + 'static,
{
    parrot_api::address::ActorRefExt::ask(r, m).await
}
