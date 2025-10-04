//! Processor manager: tracks processors by actor path.
//!
//! Thin registry kept for API compatibility; the shared pool primarily relies on
//! mailbox-attached processors (`Mailbox::set_processor` / `get_processor`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::thread::error::SystemError;
use crate::thread::processor::ProcessorInterface;

/// Manages actor processors by path.
#[derive(Default)]
pub struct ActorProcessorManager {
    processors: Mutex<HashMap<String, Arc<dyn ProcessorInterface>>>,
}

impl std::fmt::Debug for ActorProcessorManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorProcessorManager")
            .field("processors", &self.len())
            .finish()
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

    /// Remove and stop all processors.
    pub async fn shutdown_all(&self) {
        let all: Vec<Arc<dyn ProcessorInterface>> = {
            let mut map = self.processors.lock().unwrap();
            map.drain().map(|(_, v)| v).collect()
        };
        for processor in all {
            let _ = processor.stop_erased().await;
        }
    }

    /// Clear the registry without stopping processors.
    pub fn clear(&self) {
        self.processors.lock().unwrap().clear();
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
        manager.register("a", make_processor()).unwrap();
        manager.register("b", make_processor()).unwrap();
        assert_eq!(manager.len(), 2);

        manager.shutdown_all().await;
        assert!(manager.is_empty(), "shutdown_all must drain the registry");
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
