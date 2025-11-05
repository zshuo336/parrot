use std::{
    any::Any,
    thread::{self, Thread},
    time::Duration,
};

pub trait Signal: Send + Sync + 'static {
    /// Record notification responsibility while the channel is still locked.
    /// Cancellation can then transfer it before callback delivery.
    fn prepare(&self) {}
    /// Whether waking this receiver leaves the message available to others.
    fn is_stream(&self) -> bool {
        false
    }
    /// Deliver a prepared notification outside channel and registration locks.
    fn fire(&self) -> bool;
    fn as_any(&self) -> &(dyn Any + 'static);
    fn as_ptr(&self) -> *const ();
}

pub struct SyncSignal(Thread);

impl Default for SyncSignal {
    fn default() -> Self {
        Self(thread::current())
    }
}

impl Signal for SyncSignal {
    fn fire(&self) -> bool {
        self.0.unpark();
        false
    }
    fn as_any(&self) -> &(dyn Any + 'static) {
        self
    }
    fn as_ptr(&self) -> *const () {
        self as *const _ as *const ()
    }
}

impl SyncSignal {
    pub fn wait(&self) {
        thread::park();
    }
    pub fn wait_timeout(&self, dur: Duration) {
        thread::park_timeout(dur);
    }
}
