//! Interpret the actual production allocation code without starting an engine.
#[path = "../../../crates/parrot/src/thread/single_alloc.rs"]
mod single_alloc;
pub use single_alloc::SingleAllocEnvelope;
