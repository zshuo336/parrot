//! Construct owned worker threads through the same fallible path used by the pool.
use parrot_api::system::SystemError;

#[derive(Default)]
struct StartingWorkers(Vec<actix::Arbiter>);
impl StartingWorkers {
    fn rollback(&mut self) -> Option<String> {
        // Send every stop before joining any thread: another worker's disposal
        // may depend on a task whose runtime has not stopped yet.
        for owner in &self.0 {
            owner.stop();
        }
        let mut first = None;
        let mut failures = 0;
        for (index, owner) in self.0.drain(..).enumerate() {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.join()))
                .map_err(crate::owned_payload::panic_message)
                .and_then(|result| result.map_err(crate::owned_payload::panic_message));
            if let Err(message) = result {
                failures += 1;
                first.get_or_insert_with(|| format!("worker {index}: {message}"));
            }
        }
        first
            .map(|message| format!("; startup cleanup failed ({failures} worker joins): {message}"))
    }
}
impl Drop for StartingWorkers {
    fn drop(&mut self) {
        self.rollback();
    }
}

pub(super) fn build(
    size: usize,
    mut create: impl FnMut(usize) -> std::io::Result<actix::Arbiter>,
) -> Result<Vec<actix::Arbiter>, SystemError> {
    let mut workers = StartingWorkers::default();
    for index in 0..size {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| create(index)))
            .map_err(crate::owned_payload::panic_message)
            .and_then(|result| result.map_err(|error| error.to_string()));
        match result {
            Ok(owner) => workers.0.push(owner),
            Err(message) => {
                let cleanup = workers.rollback().unwrap_or_default();
                return Err(SystemError::InitializationError(format!(
                    "Actix worker {index} initialization failed: {message}{cleanup}"
                )));
            }
        }
    }
    Ok(std::mem::take(&mut workers.0))
}

#[cfg(test)]
#[path = "pool_startup/contracts.rs"]
mod contracts;
