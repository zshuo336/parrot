//! Processor manager: tracks processors by actor path.
//!
//! Thin registry kept for API compatibility; the shared pool primarily relies on
//! mailbox-attached processors (`Mailbox::set_processor` / `get_processor`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::thread::error::SystemError;
use crate::thread::processor::ProcessorInterface;

mod shutdown;

/// Manages actor processors by path.
#[derive(Default)]
pub struct ActorProcessorManager {
    processors: Mutex<HashMap<String, Arc<dyn ProcessorInterface>>>,
    // No user state is protected by this gate; unwinding releases its permit.
    // Preserve the registry's existing UnwindSafe/RefUnwindSafe API traits.
    shutdown: std::panic::AssertUnwindSafe<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for ActorProcessorManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorProcessorManager")
            .field("processors", &self.len())
            .finish()
    }
}

impl Drop for ActorProcessorManager {
    fn drop(&mut self) {
        if let Err(error) = self.try_clear() {
            crate::owned_payload::cleanup_diagnostic(|| {
                tracing::warn!(%error, "processor registry disposal failed");
            });
        }
    }
}

impl ActorProcessorManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a processor under `path`.
    ///
    /// Fails (without modifying the registry) if a processor is already
    /// registered for the path.
    pub fn register(
        &self,
        path: &str,
        processor: Arc<dyn ProcessorInterface>,
    ) -> Result<(), SystemError> {
        let mut map = self.processors.lock().unwrap();
        if map.contains_key(path) {
            return Err(SystemError::RegistrationError(format!(
                "Processor already registered for path {}",
                path
            )));
        }
        map.insert(path.to_string(), processor);
        Ok(())
    }

    /// Look up a processor by path.
    pub fn get(&self, path: &str) -> Option<Arc<dyn ProcessorInterface>> {
        self.processors.lock().unwrap().get(path).cloned()
    }

    /// Remove a processor by path.
    pub fn remove(&self, path: &str) -> Option<Arc<dyn ProcessorInterface>> {
        self.processors.lock().unwrap().remove(path)
    }

    /// Number of registered processors.
    pub fn len(&self) -> usize {
        self.processors.lock().unwrap().len()
    }

    /// Whether no processors are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stop the current snapshot, logging failures. Failed or cancelled stops
    /// remain registered for explicit retry. Use `try_shutdown_all` for receipts.
    /// This does not retire actor storage on its execution scheduler.
    pub async fn shutdown_all(&self) {
        if let Err(error) = self.try_shutdown_all().await {
            crate::owned_payload::cleanup_diagnostic(|| {
                tracing::warn!(%error, "processor snapshot shutdown failed");
            });
        }
    }

    /// Poll all snapshot stops cooperatively, without spawning detached work.
    /// Only successful stops with successful future disposal are removed, and
    /// only if the path still identifies the original processor. Registrations
    /// after snapshot capture are excluded. Concurrent shutdowns are serialized.
    /// Cancellation retains unfinished entries; no implicit deadline is added.
    /// Blocking callbacks/destructors cannot be preempted by this registry.
    pub async fn try_shutdown_all(&self) -> Result<(), SystemError> {
        self.shutdown_snapshot().await
    }

    /// Release this registry snapshot without stopping processors. Panics only
    /// after all handles have been released if a processor destructor panics.
    /// Use `try_clear` to receive the disposal failures instead.
    pub fn clear(&self) {
        self.try_clear().expect("processor registry clear failed");
    }

    /// Detach the current snapshot, then release each handle outside the lock.
    /// Registrations made during disposal belong to the next snapshot. External
    /// handles can keep processors alive; this is not a storage-retirement API.
    /// Disposal is synchronous on the caller and isolates unwindable panics.
    pub fn try_clear(&self) -> Result<(), SystemError> {
        let all = std::mem::take(&mut *self.processors.lock().unwrap());
        let mut failures = Vec::new();
        for (path, processor) in all {
            let result = crate::owned_payload::PublicationInput::new(processor).retire();
            if let Err(error) = result {
                failures.push(format!(
                    "{path}: {}",
                    crate::error_receipt::actor_summary(&error)
                ));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(SystemError::Panic(format!(
                "processor registry disposal failed: {}",
                failures.join("; ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::actor::ThreadActor;
    use crate::thread::config::ThreadActorConfig;
    use crate::thread::context::ThreadContext;
    use crate::thread::processor::ActorProcessor;
    use crate::thread::tests_support::DummyActor;

    fn make_processor() -> Arc<dyn ProcessorInterface> {
        let context = ThreadContext::<DummyActor>::new_for_test("test/manager");
        Arc::new(ActorProcessor::new(
            ThreadActor::new_for_test(DummyActor),
            context,
            "test/manager".to_string(),
            ThreadActorConfig::default(),
        ))
    }

    #[tokio::test]
    async fn test_register_and_get() {
        let manager = ActorProcessorManager::new();
        assert!(manager.is_empty());

        let processor = make_processor();
        manager.register("a", processor.clone()).unwrap();
        assert_eq!(manager.len(), 1);
        assert!(manager.get("a").is_some());
        assert!(manager.get("missing").is_none());
    }

    #[tokio::test]
    async fn test_duplicate_register_is_rejected_without_overwrite() {
        let manager = ActorProcessorManager::new();
        manager.register("a", make_processor()).unwrap();

        let second = manager.register("a", make_processor());
        assert!(second.is_err(), "duplicate registration must be rejected");

        // The original processor must remain registered (no silent overwrite).
        assert_eq!(manager.len(), 1);
        assert!(manager.get("a").is_some());
    }

    #[tokio::test]
    async fn test_remove() {
        let manager = ActorProcessorManager::new();
        manager.register("a", make_processor()).unwrap();

        assert!(manager.remove("a").is_some());
        assert!(manager.is_empty());
        assert!(manager.remove("a").is_none());
    }

    #[tokio::test]
    async fn test_shutdown_all_stops_processors_and_drains() {
        let manager = ActorProcessorManager::new();
        let first = make_processor();
        let second = make_processor();
        first.clone().initialize_and_start_erased().await.unwrap();
        second.clone().initialize_and_start_erased().await.unwrap();
        manager.register("a", first.clone()).unwrap();
        manager.register("b", second.clone()).unwrap();
        assert_eq!(manager.len(), 2);

        manager.shutdown_all().await;
        assert!(manager.is_empty(), "shutdown_all must drain the registry");
        for processor in [first, second] {
            let concrete = processor
                .as_any_ref()
                .downcast_ref::<ActorProcessor<DummyActor>>()
                .unwrap();
            assert_eq!(
                concrete.get_status(),
                crate::thread::processor::ProcessorStatus::Stopped
            );
        }
    }

    #[tokio::test]
    async fn test_clear_drops_without_stopping() {
        let manager = ActorProcessorManager::new();
        manager.register("a", make_processor()).unwrap();
        manager.clear();
        assert!(manager.is_empty());
    }

    #[tokio::test]
    async fn test_debug_formatting() {
        let manager = ActorProcessorManager::new();
        manager.register("a", make_processor()).unwrap();
        let repr = format!("{:?}", manager);
        assert!(repr.contains("ActorProcessorManager"));
    }
}
