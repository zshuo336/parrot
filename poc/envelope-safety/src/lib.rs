//! Interpret the actual production allocation code without starting an engine.
mod reply {
    pub use parrot::thread::ReplyReceiver;
}
#[path = "../../../crates/parrot/src/thread/single_alloc.rs"]
mod single_alloc;
pub use single_alloc::SingleAllocEnvelope;
