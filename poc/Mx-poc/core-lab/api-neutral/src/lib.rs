//! Runtime-neutral actor contracts, optional lifecycle adaptation and owned bindings.
//! No concrete engine dependency belongs in this crate.
pub mod engine;
pub mod registry;
pub use engine::{
    ActorRefErased, BoxedActorRef, BoxedMessage, BoxedResult, EngineRuntime, ErasedActor, SendError,
};
pub use registry::{
    list_bindings, register_engine_binding, spawn_binding, BindingRegistry, Registration,
};

/// This transferable face deliberately requires Send + 'static. A runtime for
/// thread-local !Send actors would require a separate local face.
pub trait ActorBehaviour: Send + 'static {
    fn receive(&mut self, msg: BoxedMessage) -> BoxedResult;
}
/// Optional hooks. Opt in through Spawner::spawn or the derive's lifecycle flag.
pub trait Lifecycle {
    fn pre_start(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn post_stop(&mut self) {}
}
/// Adapt behaviour and lifecycle together on any EngineRuntime, including dyn.
pub trait Spawner {
    fn spawn<B: ActorBehaviour + Lifecycle>(
        &self,
        behaviour: B,
        path: &str,
    ) -> Result<BoxedActorRef, String>;
}
struct LifecycleActor<B>(B);
impl<B: ActorBehaviour + Lifecycle> ErasedActor for LifecycleActor<B> {
    fn pre_start(&mut self) -> Result<(), String> {
        self.0.pre_start()
    }
    fn receive(&mut self, msg: BoxedMessage) -> BoxedResult {
        self.0.receive(msg)
    }
    fn post_stop(&mut self) {
        self.0.post_stop();
    }
}
impl<E: EngineRuntime + ?Sized> Spawner for E {
    fn spawn<B: ActorBehaviour + Lifecycle>(
        &self,
        behaviour: B,
        path: &str,
    ) -> Result<BoxedActorRef, String> {
        self.spawn_erased(Box::new(LifecycleActor(behaviour)), path)
    }
}
pub(crate) fn safe_drop<T>(value: T) {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
            std::mem::forget(secondary);
        }
    }
}
pub(crate) fn panic_error(payload: BoxedMessage) -> String {
    let message = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "opaque panic".into()
    };
    safe_drop(payload);
    format!("binding factory panicked: {message}")
}
