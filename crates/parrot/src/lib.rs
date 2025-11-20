// Parrot Actor Framework Implementation
//
// This crate provides an implementation of the Parrot Actor Framework API
// using native Thread and Actix runtimes.

// Initialization and supervision compose deeply nested Send future obligations.
#![recursion_limit = "256"]

pub mod actix;
mod broadcast;
mod children;
mod constructor;
mod context_message;
mod context_tasks;
mod error_receipt;
pub mod logging;
pub mod mirror;
#[cfg(test)]
mod operation_timeout_contracts;
mod owned_payload;
mod receive_timeout;
pub mod root_cohort;
mod stop_context;
mod streams;
mod supervision_policy;
pub mod system;
#[cfg(test)]
pub(crate) mod test_support;
pub mod thread;
mod typed_supervision;

// Re-export commonly used types
pub use actix::*;
pub use parrot_api_derive::*;
pub use system::*; // Re-export all macros from parrot-api-derive

pub mod typed_context;

mod escalation_pause;
