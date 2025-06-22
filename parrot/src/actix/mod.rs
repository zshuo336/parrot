// Actix implementation of the Parrot Actor Framework API

mod actor;
mod address;
mod context;
mod runtime;
mod system;
mod message;

// Export public API
pub use actor::{ActorBase, IntoActorBase};
pub use context::ActixContext;
pub use system::ActixActorSystem;
pub use message::{
    ActixMessageWrapper, 
    MessageConverter,
    ActixMessageHandler,
    MessageEnvelopeExt
};
pub use address::ActixActorRef;
pub use runtime::ActixRuntime; 