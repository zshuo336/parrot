//! One owned ticker per engine, retained by both the runtime and its components.
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

/// Epoch ticks are scheduling hints, not hard real-time wall-clock deadlines.
pub const INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub(super) struct Driver {
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
    #[cfg(test)]
    pub(super) ticks: std::sync::Arc<std::sync::atomic::AtomicU64>,
    #[cfg(test)]
    pub(super) exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Driver {
    pub(super) fn start(engine: wasmtime::Engine) -> std::io::Result<Self> {
        let (stop, receiver) = mpsc::channel();
        #[cfg(test)]
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        #[cfg(test)]
        let counter = ticks.clone();
        #[cfg(test)]
        let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(test)]
        let exit_signal = exited.clone();
        let thread = std::thread::Builder::new()
            .name("parrot-wasm-epoch".into())
            .spawn(move || {
                while let Err(RecvTimeoutError::Timeout) = receiver.recv_timeout(INTERVAL) {
                    engine.increment_epoch();
                    #[cfg(test)]
                    counter.fetch_add(1, std::sync::atomic::Ordering::Release);
                }
                #[cfg(test)]
                exit_signal.store(true, std::sync::atomic::Ordering::Release);
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
            #[cfg(test)]
            ticks,
            #[cfg(test)]
            exited,
        })
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // Wake recv_timeout immediately. The worker only increments the engine;
        // it never executes guest/host callbacks or holds application locks.
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
