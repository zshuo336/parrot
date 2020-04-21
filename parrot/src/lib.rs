// Parrot Actor Framework Implementation
//
// This crate provides an implementation of the Parrot Actor Framework API
// using the Actix actor framework as the underlying runtime.

pub mod actix;
pub mod system;
pub mod thread;
pub mod logging;

// Re-export commonly used types
pub use actix::*;
pub use system::*;
pub use parrot_api_derive::*;  // Re-export all macros from parrot-api-derive