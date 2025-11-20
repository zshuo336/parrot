use crate::thread::{
    ProcessorInterface, SystemError,
    mailbox::{Mailbox, mpsc::MpscMailbox},
    processor::{ProcessorStats, ProcessorStatsTrait},
};
use parrot_api::{address::ActorPath, types::BoxedFuture};
use std::{
    any::Any,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
pub(super) struct Probe {
    pub(super) calls: Mutex<Vec<(usize, bool)>>,
    pub(super) values: Mutex<Vec<u64>>,
    pub(super) changed: tokio::sync::Notify,
    initialized: AtomicBool,
}
impl ProcessorStatsTrait for Probe {
    fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
        None
    }
}
impl ProcessorInterface for Probe {
    fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::SeqCst)
    }
    fn initialize_and_start_erased(
        self: Arc<Self>,
    ) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async move {
            self.initialized.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
    fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
    fn retire_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
    fn process_batch_erased(
        self: Arc<Self>,
        mailbox: Arc<dyn Mailbox>,
        maximum: usize,
        yield_each: bool,
    ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push((maximum, yield_each));
            let mut processed = 0;
            for _ in 0..maximum {
                let Some(message) = mailbox.pop().await else {
                    break;
                };
                self.values
                    .lock()
                    .unwrap()
                    .push(*message.downcast::<u64>().unwrap());
                processed += 1;
            }
            self.changed.notify_one();
            Ok((processed, 0))
        })
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
pub(super) fn stack(path: &str) -> (Arc<MpscMailbox>, Arc<Probe>) {
    let mailbox = Arc::new(MpscMailbox::new(8, ActorPath::placeholder(path)));
    let processor = Arc::new(Probe::default());
    mailbox.set_processor(processor.clone());
    (mailbox, processor)
}
