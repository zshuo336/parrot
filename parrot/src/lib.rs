// Parrot Actor Framework Implementation
//
// This crate provides an implementation of the Parrot Actor Framework API
// using the Actix actor framework as the underlying runtime.

pub mod actix;


// Re-export commonly used types
pub use actix::*;