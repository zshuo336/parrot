// Parrot Actor Framework Implementation
//
// This crate provides an implementation of the Parrot Actor Framework API
// using the Actix actor framework as the underlying runtime.

pub mod actix;
mod children;
mod constructor;
mod context_message;
mod context_tasks;
mod error_receipt;
pub mod logging;
pub mod mirror;
mod owned_payload;
mod receive_timeout;
mod streams;
mod supervision_policy;
pub mod system;
pub mod thread;
mod typed_supervision;

// Re-export commonly used types
pub use actix::*;
pub use parrot_api_derive::*;
pub use system::*; // Re-export all macros from parrot-api-derive

pub mod typed_context;

mod escalation_pause;
