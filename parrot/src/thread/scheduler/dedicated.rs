use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, Weak, atomic::{AtomicBool, Ordering}};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::anyhow;
use tokio::runtime::{Builder, Handle, Runtime};

use crate::thread::mailbox::Mailbox;
use crate::thread::actor_path::ActorPath;
use crate::thread::config::ThreadActorConfig;
use crate::thread::error::SystemError;

/// # DedicatedThreadPool
/// 
/// Manages actors that run on their own dedicated OS threads.
/// 
/// This scheduler is designed for actors that:
/// - Need consistent, predictable performance
/// - Perform CPU-intensive work
/// - Have strict latency requirements
/// - Need complete isolation from other actors
/// 
/// ## Key Features
/// 
/// - Each actor runs on its exclusive OS thread
/// - Every dedicated thread has its own single-threaded Tokio runtime
/// - Optional core affinity for maximum performance
/// - Efficient thread lifecycle management
/// 
/// ## Design Principles
/// 
/// - Minimize interference between actors
/// - Prioritize low latency over throughput
/// - Support CPU-bound workloads efficiently
/// - Provide isolation for critical actors
/// 
/// ## Thread Safety
/// 
/// - Thread-safe through Arc/Mutex
/// - Avoids lock contention by design
#[derive(Debug)]
pub struct DedicatedThreadPool {
    /// Active dedicated threads, keyed by actor path
    threads: Arc<Mutex<HashMap<ActorPath, ThreadEntry>>>,
    
    /// System runtime handle for task spawning
    runtime_handle: Handle,
    
    /// System reference for actor lookups and error handling
    system: Option<Weak<dyn SystemRef + Send + Sync>>,
    
    /// Shutdown flag
    is_shutting_down: Arc<AtomicBool>,
    
    /// Maximum number of dedicated threads allowed
    max_threads: usize,
}

/// Interface for system-level operations needed by dedicated threads
pub trait SystemRef {
    /// Find an actor by its path
    fn find_actor(&self, path: &str) -> Option<Arc<dyn std::any::Any + Send + Sync>>;
    
    /// Handle worker panic, providing error details and affected mailbox path
    fn handle_worker_panic(&self, error: String, mailbox_path: String);
}

/// Entry for an active dedicated thread
struct ThreadEntry {
    /// Thread handle for management and joining
    handle: JoinHandle<()>,
    
    /// Channel to send shutdown signal
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    
    /// Actor path (for identification)
    path: ActorPath,
    
    /// Actor configuration (for restart/recovery)
    config: ThreadActorConfig,
    
    /// Optional thread CPU affinity
    core_affinity: Option<usize>,
}

impl DedicatedThreadPool {
    /// Creates a new DedicatedThreadPool
    /// 
    /// # Arguments
    /// * `runtime_handle` - Tokio runtime handle for the system
    /// * `system` - Optional reference to the actor system
    /// * `max_threads` - Maximum number of dedicated threads allowed
    pub fn new(
        runtime_handle: Handle,
        system: Option<Weak<dyn SystemRef + Send + Sync>>,
        max_threads: usize,
    ) -> Self {
        let threads = Arc::new(Mutex::new(HashMap::new()));
        let is_shutting_down = Arc::new(AtomicBool::new(false));
        
        Self {
            threads,
            runtime_handle,
            system,
            is_shutting_down,
            max_threads,
        }
    }
    
    /// Schedules an actor to run on a dedicated thread
    /// 
    /// Creates a new OS thread with its own Tokio runtime, then runs
    /// the actor's processing loop on it.
    /// 
    /// # Arguments
    /// * `mailbox` - The mailbox for the actor
    /// * `config` - Actor configuration, used for core affinity and other settings
    /// 
    /// # Returns
    /// `Ok(())` on success, or `Err` if scheduling fails
    pub fn schedule(
        &self,
        mailbox: Arc<dyn Mailbox + Send + Sync>,
        config: Option<&ThreadActorConfig>,
    ) -> Result<(), SystemError> {
        // Check if system is shutting down
        if self.is_shutting_down.load(Ordering::Relaxed) {
            return Err(SystemError::ShuttingDown);
        }
        
        let actor_path = mailbox.path().clone();
        let actor_path_str = actor_path.path.clone();
        
        let mut threads = self.threads.lock().map_err(|_| {
            SystemError::Other(anyhow!("Failed to acquire threads lock"))
        })?;
        
        // Check if the actor already has a dedicated thread
        if threads.contains_key(&actor_path) {
            return Err(SystemError::ConfigError(format!(
                "Actor '{}' already has a dedicated thread",
                actor_path.path
            )));
        }
        
        // Check if we've reached the maximum number of threads
        if threads.len() >= self.max_threads {
            return Err(SystemError::ConfigError(format!(
                "Maximum number of dedicated threads ({}) reached",
                self.max_threads
            )));
        }
        
        // Use provided config or create default
        let thread_config = match config {
            Some(cfg) => cfg.clone(),
            None => ThreadActorConfig::default(),
        };
        
        // Extract core affinity if specified
        let core_affinity = match &thread_config.scheduling_mode {
            Some(crate::thread::config::SchedulingMode::DedicatedThread) => {
                // We could have a specialized field for core affinity
                // For now, just illustrate how it would work
                None // In future could be Some(core_id)
            },
            _ => None,
        };
        
        // Create shutdown channel
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        
        // Clone necessary references for the dedicated thread
        let system_ref = self.system.clone();
        let is_shutting_down = self.is_shutting_down.clone();
        
        // Prepare thread name for better debugging
        let thread_name = format!("dedicated-actor-{}", actor_path.path.replace('/', "-"));
        
        // Spawn dedicated OS thread
        let thread_handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                // Set thread-local core affinity if specified
                if let Some(core_id) = core_affinity {
                    #[cfg(target_os = "linux")]
                    {
                        // This is placeholder code - would need proper affinity lib
                        // Would use `core_affinity` crate or similar
                        println!("Setting thread affinity to core {}", core_id);
                    }
                }
                
                // Create a single-threaded runtime for this actor
                let runtime = Builder::new_current_thread()
                    .enable_all()
                    .thread_name(format!("tokio-rt-{}", actor_path.path))
                    .build()
                    .expect("Failed to create actor runtime");
                
                // Run the actor loop on this thread's runtime
                runtime.block_on(Self::actor_loop(
                    mailbox,
                    shutdown_rx,
                    is_shutting_down,
                    system_ref,
                    actor_path_str,
                ));
                
                // Runtime and thread will terminate when actor_loop returns
            })
            .map_err(|e| {
                SystemError::Other(anyhow!("Failed to spawn dedicated thread: {}", e))
            })?;
        
        // Store thread entry for management
        threads.insert(
            actor_path,
            ThreadEntry {
                handle: thread_handle,
                shutdown_tx,
                path: actor_path.clone(),
                config: thread_config,
                core_affinity,
            },
        );
        
        Ok(())
    }
    
    /// Removes an actor from its dedicated thread
    /// 
    /// Sends a shutdown signal to the specified actor's thread
    /// and optionally waits for it to complete
    /// 
    /// # Arguments
    /// * `path` - Path of the actor to deschedule
    /// * `wait` - Whether to wait for the thread to terminate
    /// * `timeout_ms` - Maximum time to wait (if wait is true)
    /// 
    /// # Returns
    /// `Ok(())` on success, or `Err` if descheduling fails
    pub fn deschedule(
        &self,
        path: &ActorPath,
        wait: bool,
        timeout_ms: Option<u64>,
    ) -> Result<(), SystemError> {
        let mut threads = self.threads.lock().map_err(|_| {
            SystemError::Other(anyhow!("Failed to acquire threads lock"))
        })?;
        
        let entry = threads.remove(path).ok_or_else(|| {
            SystemError::ActorNotFound(path.path.clone())
        })?;
        
        // Send shutdown signal to the actor thread
        let _ = entry.shutdown_tx.send(true);
        
        // Optionally wait for thread to terminate
        if wait {
            let timeout = timeout_ms.unwrap_or(5000);
            
            // Use system runtime to spawn a task that waits for thread completion
            let handle = entry.handle;
            let path_str = path.path.clone();
            
            // This is one approach - assumes self.runtime_handle is system runtime
            // In full implementation, we might handle this differently
            self.runtime_handle.spawn(async move {
                // Create a timeout future
                let timeout_future = tokio::time::sleep(Duration::from_millis(timeout));
                
                // Create a task to join the thread
                let join_task = tokio::task::spawn_blocking(move || {
                    handle.join()
                });
                
                // Wait for either join completion or timeout
                tokio::select! {
                    _ = timeout_future => {
                        println!("Warning: Timeout waiting for actor thread '{}' to terminate", path_str);
                    }
                    result = join_task => {
                        match result {
                            Ok(thread_result) => {
                                if let Err(e) = thread_result {
                                    println!("Warning: Actor thread '{}' terminated with error: {:?}", path_str, e);
                                }
                            }
                            Err(e) => {
                                println!("Warning: Failed to join actor thread '{}': {:?}", path_str, e);
                            }
                        }
                    }
                }
            });
        }
        
        Ok(())
    }
    
    /// Gracefully shuts down all dedicated threads
    /// 
    /// Signals all managed threads to stop and optionally waits for them
    /// 
    /// # Arguments
    /// * `timeout_ms` - Maximum time to wait for threads to terminate
    /// 
    /// # Returns
    /// `Ok(())` on success, or timeout error
    pub async fn shutdown(&self, timeout_ms: u64) -> Result<(), SystemError> {
        // Set shutdown flag
        self.is_shutting_down.store(true, Ordering::SeqCst);
        
        // Get all thread entries while releasing the lock
        let entries = {
            let mut threads = self.threads.lock().map_err(|_| {
                SystemError::Other(anyhow!("Failed to acquire threads lock"))
            })?;
            
            // Take all entries, leaving an empty map
            std::mem::take(&mut *threads)
                .into_iter()
                .collect::<Vec<_>>()
        };
        
        // Signal all threads to shut down
        for (_, entry) in &entries {
            let _ = entry.shutdown_tx.send(true);
        }
        
        // Create a timeout for joining threads
        let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
        
        // Spawn tasks to join all threads
        let join_tasks = entries
            .into_iter()
            .map(|(path, entry)| {
                let handle = entry.handle;
                let path_str = path.path.clone();
                
                tokio::task::spawn_blocking(move || {
                    match handle.join() {
                        Ok(_) => println!("Actor thread '{}' terminated gracefully", path_str),
                        Err(e) => println!("Actor thread '{}' terminated with error: {:?}", path_str, e),
                    }
                })
            })
            .collect::<Vec<_>>();
        
        // Wait for all threads to join or timeout
        tokio::select! {
            _ = futures::future::join_all(join_tasks) => {
                println!("All dedicated threads terminated gracefully");
                Ok(())
            }
            _ = timeout => {
                println!("Warning: Timeout waiting for dedicated threads to terminate");
                Err(SystemError::ShutdownError(format!("Timeout after {}ms", timeout_ms)))
            }
        }
    }
    
    /// The main actor processing loop for a dedicated thread
    /// 
    /// Continuously processes messages from the actor's mailbox
    /// until signaled to stop or the mailbox closes
    /// 
    /// # Arguments
    /// * `mailbox` - The actor's mailbox
    /// * `shutdown_rx` - Channel to receive shutdown signal
    /// * `is_system_shutting_down` - System-wide shutdown flag
    /// * `system` - Optional reference to the actor system
    /// * `actor_path_str` - String representation of the actor path (for logging)
    async fn actor_loop(
        mailbox: Arc<dyn Mailbox + Send + Sync>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
        is_system_shutting_down: Arc<AtomicBool>,
        system: Option<Weak<dyn SystemRef + Send + Sync>>,
        actor_path_str: String,
    ) {
        println!("Starting dedicated thread for actor '{}'", actor_path_str);
        
        // Processing loop
        loop {
            // Check shutdown conditions
            if *shutdown_rx.borrow() || is_system_shutting_down.load(Ordering::Relaxed) {
                break;
            }
            
            // Try to get and process messages
            tokio::select! {
                // Check for shutdown signal
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                
                // Try to get a message
                msg_opt = mailbox.pop() => {
                    match msg_opt {
                        Some(msg) => {
                            // TODO: Process message with the actual actor
                            // This would involve:
                            // 1. Finding the actor instance (via system.find_actor)
                            // 2. Getting a lock on the actor state
                            // 3. Delivering the message to the actor
                            // 4. Handling any errors
                            
                            // For now, just log
                            println!("Actor '{}' processed a message", actor_path_str);
                            
                            // Optionally yield to ensure fairness (though less important for dedicated threads)
                            tokio::task::yield_now().await;
                        }
                        None => {
                            // No more messages, wait a bit before checking again
                            // This is more efficient than continuously polling
                            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                        }
                    }
                }
                
                // Optional timeout just to periodically check shutdown status
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                    // No action needed, this just ensures we check shutdown flag regularly
                }
            }
        }
        
        println!("Shutting down dedicated thread for actor '{}'", actor_path_str);
        
        // Perform any necessary cleanup
        // e.g., close the mailbox
        mailbox.close().await;
    }
    
    /// Returns the number of active dedicated threads
    pub fn thread_count(&self) -> Result<usize, SystemError> {
        let threads = self.threads.lock().map_err(|_| {
            SystemError::Other(anyhow!("Failed to acquire threads lock"))
        })?;
        
        Ok(threads.len())
    }
    
    /// Returns the maximum number of allowed dedicated threads
    pub fn max_threads(&self) -> usize {
        self.max_threads
    }
    
    /// Checks if an actor has a dedicated thread
    pub fn has_dedicated_thread(&self, path: &ActorPath) -> Result<bool, SystemError> {
        let threads = self.threads.lock().map_err(|_| {
            SystemError::Other(anyhow!("Failed to acquire threads lock"))
        })?;
        
        Ok(threads.contains_key(path))
    }
}

#[cfg(test)]
mod tests {
    // TODO: Add tests for DedicatedThreadPool
} 