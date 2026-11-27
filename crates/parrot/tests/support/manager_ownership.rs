use parrot::thread::{
    ProcessorInterface, SystemError,
    processor::{ProcessorStats, ProcessorStatsTrait},
};
use parrot_api::types::BoxedFuture;
use std::{
    any::Any,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[path = "isolated.rs"]
mod child;

#[derive(Default)]
pub struct Counts {
    pub drops: AtomicUsize,
    pub stops: AtomicUsize,
}

pub struct Probe {
    pub counts: Arc<Counts>,
    pub on_drop: Option<Box<dyn FnOnce() + Send + Sync>>,
}
impl Probe {
    pub fn plain(counts: &Arc<Counts>) -> Arc<Self> {
        Arc::new(Self {
            counts: counts.clone(),
            on_drop: None,
        })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(on_drop) = self.on_drop.take() {
            on_drop();
        }
    }
}
impl ProcessorStatsTrait for Probe {
    fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
        None
    }
}
impl ProcessorInterface for Probe {
    fn is_initialized(&self) -> bool {
        false
    }
    fn initialize_and_start_erased(
        self: Arc<Self>,
    ) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
    fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
        self.counts.stops.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn process_batch_erased(
        self: Arc<Self>,
        _: Arc<dyn parrot::thread::mailbox::Mailbox>,
        _: usize,
        _: bool,
    ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
        Box::pin(async { Ok((0, 0)) })
    }
    fn as_any(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

pub fn isolated(name: &str, test: impl FnOnce()) {
    child::run(
        name,
        "PARROT_MANAGER_CHILD",
        Duration::from_secs(10),
        || {
            test();
            println!("manager ownership complete: {name}");
        },
    );
}

pub fn panics(action: impl FnOnce()) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(action)).is_err()
}
