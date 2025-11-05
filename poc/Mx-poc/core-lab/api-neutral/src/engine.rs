//! Concrete runtimes implement this face without changing business actors.
use std::sync::Arc;
use std::time::Duration;
pub type BoxedMessage = Box<dyn std::any::Any + Send>;
pub type BoxedResult = Result<BoxedMessage, String>;
pub type BoxedActorRef = Arc<dyn ActorRefErased>;
pub struct SendError {
    pub reason: String,
    pub message: BoxedMessage,
}
impl std::fmt::Debug for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendError")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
/// Runtime hooks default to no-op. Implementations must call pre_start before
/// publishing a ready address, and post_stop exactly once after any attempted
/// initialization, including failure, before releasing the actor.
pub trait ErasedActor: Send + 'static {
    fn pre_start(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn receive(&mut self, msg: BoxedMessage) -> BoxedResult;
    fn post_stop(&mut self) {}
}
pub trait ActorRefErased: Send + Sync {
    fn tell(&self, msg: BoxedMessage) -> Result<(), SendError>;
    fn ask(&self, msg: BoxedMessage) -> BoxedResult {
        self.ask_with_timeout(msg, Duration::from_secs(5))
    }
    fn ask_with_timeout(&self, msg: BoxedMessage, timeout: Duration) -> BoxedResult;
    fn path(&self) -> String;
    fn generation(&self) -> u64;
    fn stop(&self);
    fn wait_stopped(&self, timeout: Duration) -> Result<(), String>;
}
/// Successful spawn means pre_start succeeded. Failed initialization never
/// dispatches receive, but still calls post_stop exactly once before releasing
/// the actor, as required by ErasedActor. Shutdown closes admission and joins
/// the runtime's workers; synchronous callbacks must return for shutdown to finish.
pub trait EngineRuntime: Send + Sync {
    fn name(&self) -> &'static str;
    fn spawn_erased(
        &self,
        actor: Box<dyn ErasedActor>,
        path: &str,
    ) -> Result<BoxedActorRef, String>;
    fn shutdown(&self) -> Result<(), String>;
    fn worker_counts(&self) -> (usize, usize);
}
