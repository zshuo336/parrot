use std::sync::Arc;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::runtime::Handle;
use tracing::{info, error, debug};
use std::any::Any;

use crate::thread::actor::ThreadActor;
use crate::thread::context::ThreadContext;
use crate::thread::mailbox::Mailbox;
use crate::thread::config::ThreadActorConfig;
use crate::thread::processor::{ActorProcessor, ProcessorStatus};
use crate::thread::error::SystemError;
use parrot_api::actor::Actor;

/// # ActorProcessorManager
///
/// Manager for creating and managing actor processors in the actor system. it is a factory for actor processors.
///
/// ## Key Responsibilities
/// - Creating actor processors with appropriate configuration
/// - Managing processor lifecycle (registration, lookup, removal)
/// - Providing access to processors by actor path
///
/// ## Implementation Details
/// ### Core Algorithm
/// 1. Create processors with appropriate actor and context
/// 2. Register processors in a centralized registry
/// 3. Provide lookup mechanisms for finding processors by path
///
/// ### Performance Characteristics
/// - Fast processor lookup via HashMap
/// - Thread-safe processor management with Mutex protection
/// - Efficient type-erased storage using Box<dyn Any>
#[derive(Debug)]
pub struct ActorProcessorManager {
    /// Registry of created processors
    processors: Arc<Mutex<HashMap<String, Arc<Mutex<dyn std::any::Any + Send + Sync>>>>>,
}

impl ActorProcessorManager {
    /// Create a new actor processor manager
    pub fn new() -> Self {
        Self {
            processors: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    
    /// Create an actor processor instance for the given actor
    ///
    /// # Parameters
    /// * `actor` - The thread actor to create a processor for
    /// * `context` - The actor's context
    /// * `mailbox` - The actor's mailbox
    /// * `path` - The actor's path
    /// * `config` - The actor's configuration
    ///
    /// # Returns
    /// A new actor processor instance
    pub fn create_processor<A>(
        &self,
        actor: ThreadActor<A>,
        context: ThreadContext<A>,
        mailbox: Arc<Mutex<dyn Mailbox>>,
        path: String,
        config: ThreadActorConfig,
    ) -> Arc<Mutex<ActorProcessor<A>>>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        // Create actor processor
        let processor = Arc::new(Mutex::new(ActorProcessor::new(
            actor,
            context,
            mailbox.clone(),
            path.clone(),
            config,
        )));
        
        debug!("Created processor for actor at {}", path);
        
        // Register the processor with the mailbox
        let mut mailbox_guard = mailbox.lock().unwrap();
        mailbox_guard.set_processor(processor.clone());
        
        processor
    }
    
    /// Create and register a processor for the given actor
    ///
    /// # Parameters
    /// * `actor` - The thread actor to create a processor for
    /// * `context` - The actor's context
    /// * `mailbox` - The actor's mailbox
    /// * `path` - The actor's path
    /// * `config` - The actor's configuration
    ///
    /// # Returns
    /// A reference to the created and registered processor
    pub fn create_and_register_processor<A>(
        &self,
        actor: ThreadActor<A>,
        context: ThreadContext<A>,
        mailbox: Arc<Mutex<dyn Mailbox>>,
        path: String,
        config: ThreadActorConfig,
    ) -> Arc<Mutex<ActorProcessor<A>>>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        debug!("Creating and registering processor for actor at {}", path);

        // Create processor
        let processor = self.create_processor(actor, context, mailbox, path.clone(), config);
        // Store in registry
        self.register_processor_arc(path, processor.clone());
        processor
    }
    
    /// Find processor by path
    pub fn get_processor_by_path(&self, path: &str) -> Option<Arc<Mutex<dyn std::any::Any + Send + Sync>>> {
        let processors = self.processors.lock().unwrap();
        processors.get(path).map(|p| p.clone())
    }
    
    /// Find processor by path and downcast to specific type
    pub fn get_processor_by_path_as<A>(&self, path: &str) -> Option<Arc<Mutex<ActorProcessor<A>>>> 
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        let processor_any = self.get_processor_by_path(path)?;
        
        // check type
        let guard = processor_any.lock().unwrap();
        if !guard.is::<ActorProcessor<A>>() {
            debug!("Failed to downcast processor for actor at {}: type mismatch", path);
            return None;
        }
        drop(guard);
        
        // clone the original Arc to keep the reference count
        let cloned = processor_any.clone();
        // then convert to the desired type
        let ptr = Arc::into_raw(cloned);
        let typed_ptr = ptr as *const Mutex<ActorProcessor<A>>;
        Some(unsafe { Arc::from_raw(typed_ptr) })
    }
    
    /// Check if processor exists
    pub fn has_processor(&self, path: &str) -> bool {
        let processors = self.processors.lock().unwrap();
        processors.contains_key(path)
    }
    
    /// Register processor
    pub fn register_processor<A>(&self, path: String, processor: ActorProcessor<A>) 
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        let mut processors = self.processors.lock().unwrap();
        let processor_arc = Arc::new(Mutex::new(processor));
        processors.insert(path.clone(), processor_arc);
        debug!("Registered processor for actor at {}", path);
    }
    
    /// Register processor with Arc
    pub fn register_processor_arc<A>(&self, path: String, processor: Arc<Mutex<ActorProcessor<A>>>) 
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        let mut processors = self.processors.lock().unwrap();
        processors.insert(path.clone(), processor);
        debug!("Registered processor (Arc) for actor at {}", path);
    }
    
    /// Remove processor
    pub fn remove_processor(&self, path: &str) -> bool {
        let mut processors = self.processors.lock().unwrap();
        if processors.remove(path).is_some() {
            debug!("Removed processor for actor at {}", path);
            true
        } else {
            false
        }
    }
    
    /// Get processor count
    pub fn processor_count(&self) -> usize {
        let processors = self.processors.lock().unwrap();
        processors.len()
    }
    
    /// Get all processor paths
    pub fn get_all_processor_paths(&self) -> Vec<String> {
        let processors = self.processors.lock().unwrap();
        processors.keys().cloned().collect()
    }
    
    /// Get processor registry reference
    pub fn processors_ref(&self) -> Arc<Mutex<HashMap<String, Arc<Mutex<dyn std::any::Any + Send + Sync>>>>> {
        self.processors.clone()
    }
    
    /// Find all processors with a specific status
    pub fn find_processors_with_status(&self, status: ProcessorStatus) -> Vec<String> {
        let processors = self.processors.lock().unwrap();
        let mut result = Vec::new();
        
        for (path, processor_any) in processors.iter() {
            // Try different processor types
            // This is a somewhat inefficient approach, in a real implementation
            // you might want to store type information alongside the processor
            // or use a more efficient type-erasure mechanism
            
            // Since we don't know the exact type parameter A, we'll use a heuristic
            // to check the processor status across different possible types
            
            let mut found = false;
            
            // Check if we can extract the status from any known processor type
            // This code is more for demonstration, in practice you would likely
            // have a way to extract this information without needing to know the type
            if let Some(status_atomic) = extract_processor_status(processor_any) {
                if status as usize == status_atomic.load(std::sync::atomic::Ordering::SeqCst) {
                    result.push(path.clone());
                    found = true;
                }
            }
            
            // Log if we couldn't determine the status
            if !found {
                debug!("Could not determine status for processor at {}", path);
            }
        }
        
        result
    }
}

/// Helper function to extract processor status from any processor type
fn extract_processor_status(processor_any: &Arc<Mutex<dyn std::any::Any + Send + Sync>>) -> Option<Arc<std::sync::atomic::AtomicUsize>> {
    // Try different possible processor types
    // In reality, you would use a trait or other mechanism to abstract this
    
    // For now, we'll use a simpler approach
    None // Placeholder - implement based on your actual needs
} 