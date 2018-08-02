// Parrot-Actix: An adapter for integrating the Parrot actor system with Actix runtime
//
// This module provides the necessary components to build an actor system using
// Actix as the underlying execution engine, while maintaining the ParrotActor API.

pub mod actor;
pub mod context;
pub mod message;
pub mod reference;
pub mod system;
pub mod types;

pub use actor::ActixActor;
pub use actor::{ActorBase, IntoActorBase};
pub use context::ActixContext;
pub use message::*;
pub use reference::ActixActorRef;
pub use system::ActixActorSystem;
pub use types::*; 