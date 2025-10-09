//! Real arbiter regressions for owned asynchronous dispatch and cancellation.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use actix::Actor as _;
use parrot::actix::message::{ActixMessageWrapper, create_envelope};
use parrot::actix::{ActixActor, ActixContext};
use parrot_api::actor::{ActixEngineExt, Actor, ActorState, EmptyConfig};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

enum Command {
    Add(u64),
    Fail,
    Read,
    Wait,
}
impl Message for Command {
    type Result = u64;
}

fn message(command: Command) -> ActixMessageWrapper {
    ActixMessageWrapper {
        envelope: create_envelope(command),
    }
}

struct OwnedActor {
    value: u64,
    entered: Arc<tokio::sync::Notify>,
    drops: Arc<Mutex<Vec<&'static str>>>,
}

impl Drop for OwnedActor {
    fn drop(&mut self) {
        self.drops.lock().unwrap().push("actor");
    }
}

struct PendingGuard(Arc<Mutex<Vec<&'static str>>>);
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("handler");
    }
}

impl ActixEngineExt for OwnedActor {}
impl Actor for OwnedActor {
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
            match *msg.downcast::<Command>().unwrap() {
                Command::Add(n) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    self.value += n;
                }
                Command::Fail => {
                    self.value += 5;
                    tokio::task::yield_now().await;
                    return Err(ActorError::MessageHandlingError("expected failure".into()));
                }
                Command::Read => {}
                Command::Wait => {
                    let _guard = PendingGuard(self.drops.clone());
                    self.entered.notify_one();
                    std::future::pending::<()>().await;
                    // Retain a real borrow of actor storage across Pending.
                    self.value += 1;
                }
            }
            Ok(Box::new(self.value) as BoxedMessage)
        })
    }
}

#[test]
fn async_dispatch_preserves_state_serialization_and_error_recovery() {
    actix::System::new().block_on(async {
        let actor = ActixActor::new(OwnedActor {
            value: 10,
            entered: Default::default(),
            drops: Default::default(),
        })
        .start();
        let add = actor.send(message(Command::Add(1)));
        let read = actor.send(message(Command::Read));
        let (added, read) = tokio::join!(add, read);
        assert_eq!(
            *added.unwrap().unwrap().unwrap().downcast::<u64>().unwrap(),
            11
        );
        assert_eq!(
            *read.unwrap().unwrap().unwrap().downcast::<u64>().unwrap(),
            11
        );
        assert!(
            actor
                .send(message(Command::Fail))
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        let value = actor
            .send(message(Command::Read))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            *value.downcast::<u64>().unwrap(),
            16,
            "an error must restore the actual actor state"
        );
    });
}

#[test]
fn arbiter_cancellation_drops_handler_before_its_actor_storage() {
    actix::System::new().block_on(async {
        let drops = Arc::new(Mutex::new(vec![]));
        let entered = Arc::new(tokio::sync::Notify::new());
        let actor = OwnedActor {
            value: 0,
            entered: entered.clone(),
            drops: drops.clone(),
        };
        let arbiter = actix::Arbiter::new();
        let (send, receive) = tokio::sync::oneshot::channel();
        assert!(arbiter.spawn(async move {
            let _ = send.send(ActixActor::new(actor).start());
        }));
        let address = receive.await.unwrap();
        let request = address.send(message(Command::Wait));
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        arbiter.stop();
        arbiter.join().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(*drops.lock().unwrap(), vec!["handler", "actor"]);
    });
}
