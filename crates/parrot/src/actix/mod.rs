// Parrot-Actix: An adapter for integrating the Parrot actor system with Actix runtime
//
// This module provides the necessary components to build an actor system using
// Actix as the underlying execution engine, while maintaining the ParrotActor API.

pub mod actor;
pub mod context;
pub mod message;
pub mod reference;
pub mod system;
pub mod typed;
pub mod types;

pub use actor::ActixActor;
pub use actor::{ActorBase, IntoActorBase};
pub use context::ActixContext;
pub use message::*;
pub use reference::ActixActorRef;
pub use system::{ActixActorSystem, ArbiterPool};
pub use typed::{DEFAULT_ASK_TIMEOUT as TYPED_ASK_TIMEOUT, TypedActorRef as ActixTypedActorRef};
pub use types::*;

/// M1 derive-decouple: the type alias of the engine's neutral Context face.
///
/// The derive macro generates references via `__parrot_engine::EngineContext<Self>`,
/// and the user side binds with `use parrot::actix as __parrot_engine;`.
/// This way the generated code contains zero concrete engine symbol paths (parrot::actix:: ...).
pub use context::EngineContext;

/// M1 derive-decouple: the engine-binding module shape expected by the derive macro.
///
/// The user side binds with `use parrot::actix::__parrot_engine_binding::*;` (or directly
/// imports `EngineContext` from this module and re-exports it). This module is the "engine face"
/// consumed by the macro-generated code — everything in it is engine-agnostic.
pub mod __parrot_engine_binding {
    pub use super::EngineContext;
}
