use parrot::thread::{
    ProcessorInterface, SystemError,
    processor::{ProcessorStats, ProcessorStatsTrait},
};
use parrot_api::types::BoxedFuture;
use std::{
    any::Any,
    panic::AssertUnwindSafe,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

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
    if std::env::var("PARROT_MANAGER_CHILD").as_deref() == Ok(name) {
        test();
        println!("manager ownership complete: {name}");
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env("PARROT_MANAGER_CHILD", name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break true;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !timed_out && output.status.success(),
        "manager child failed: timed_out={timed_out}, status={:?}\n{stdout}\n{stderr}",
        output.status
    );
    assert!(stdout.contains(&format!("manager ownership complete: {name}")));
}

pub fn panics(action: impl FnOnce()) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(action)).is_err()
}
