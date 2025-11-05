//! Two real implementations of the same neutral face: one dedicated worker per
//! actor, and a shared reduction scheduler. No engine symbols enter api-neutral.
mod common;
mod dedicated;
mod shared;
pub use dedicated::DedicatedEngine;
pub use shared::SharedEngine;
