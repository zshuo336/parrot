use crate::common::{deadline, finish_shutdown, ActorOwner, Command, Errors};
use api_neutral::{
    ActorRefErased, BoxedActorRef, BoxedMessage, BoxedResult, EngineRuntime, ErasedActor, SendError,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use typed_static::{SimpleEngine, TypedActorRef, TypedAskRef, TypedReceive};
struct Actor(ActorOwner);
impl TypedReceive<Command> for Actor {
    type Reply = BoxedMessage;
    fn receive_typed(&mut self, message: Command) -> BoxedResult {
        self.0.apply(message)
    }
}
pub struct DedicatedEngine {
    engine: Arc<SimpleEngine>,
    errors: Errors,
    start_timeout: Duration,
}
impl DedicatedEngine {
    pub fn new() -> Self {
        Self::with_start_timeout(Duration::from_secs(5)).unwrap()
    }
    pub fn with_start_timeout(timeout: Duration) -> Result<Self, String> {
        deadline(timeout)?;
        Ok(Self {
            engine: SimpleEngine::new(),
            errors: Arc::new(Mutex::new(vec![])),
            start_timeout: timeout,
        })
    }
}
impl Default for DedicatedEngine {
    fn default() -> Self {
        Self::new()
    }
}
impl EngineRuntime for DedicatedEngine {
    fn name(&self) -> &'static str {
        "dedicated-thread"
    }
    fn spawn_erased(
        &self,
        actor: Box<dyn ErasedActor>,
        path: &str,
    ) -> Result<BoxedActorRef, String> {
        let reference = self
            .engine
            .actor(Actor(ActorOwner::new(actor, self.errors.clone())), path)?;
        if let Err(error) = reference.ask_with_timeout(Command::Start, self.start_timeout) {
            reference.stop();
            let cleanup = reference.wait_stopped(self.start_timeout);
            return Err(match cleanup {
                Ok(()) => error,
                Err(e) => format!("{error}; cleanup pending: {e}"),
            });
        }
        Ok(Arc::new(Reference(reference)))
    }
    fn shutdown(&self) -> Result<(), String> {
        finish_shutdown(self.engine.shutdown(), &self.errors)
    }
    fn worker_counts(&self) -> (usize, usize) {
        self.engine.worker_counts()
    }
}
impl Drop for DedicatedEngine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
struct Reference(TypedActorRef<Command, BoxedMessage>);
impl ActorRefErased for Reference {
    fn tell(&self, message: BoxedMessage) -> Result<(), SendError> {
        self.0
            .tell(Command::Message(message))
            .map_err(|error| match error.message {
                Command::Message(message) => SendError {
                    reason: error.reason,
                    message,
                },
                Command::Start => unreachable!("business command changed"),
            })
    }
    fn ask_with_timeout(&self, message: BoxedMessage, timeout: Duration) -> BoxedResult {
        self.0.ask_with_timeout(Command::Message(message), timeout)
    }
    fn path(&self) -> String {
        self.0.path().into()
    }
    fn generation(&self) -> u64 {
        self.0.generation()
    }
    fn stop(&self) {
        self.0.stop();
    }
    fn wait_stopped(&self, timeout: Duration) -> Result<(), String> {
        self.0.wait_stopped(timeout)
    }
}
