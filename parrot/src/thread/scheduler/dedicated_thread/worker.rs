//! # Worker Thread Module
//!
//! This module provides a dedicated worker implementation that runs an actor
//! on a separate thread. Each worker manages a single compute-intensive actor.
//!
//! ## Key Concepts
//! - Worker lifecycle: Thread creation, execution, and cleanup
//! - Message processing: Handling actor messages in a dedicated thread
//! - Panic recovery: Safely handling thread panics
//!
//! ## Design Principles
//! - Isolation: Run actors in dedicated threads for predictable performance
//! - Error handling: Robust panic handling and reporting
//! - Controlled shutdown: Clean termination of worker threads

use std::sync::{Arc, Weak, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use std::fmt;
use std::error::Error;

use tokio::runtime::Handle;
use tokio::sync::{oneshot, mpsc};
use tokio::time::timeout;

use crate::thread::mailbox::Mailbox;
use crate::thread::config::ThreadActorConfig;

use super::SystemRef;

/// States a worker can be in
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// Worker has been created but not started yet
    Created,
    /// Worker is running
    Running,
    /// Worker is in the process of stopping
    Stopping,
    /// Worker has been stopped
    Stopped,
}

/// Worker that runs an actor on a dedicated thread
pub struct Worker {
    /// Mailbox for the actor
    mailbox: Arc<dyn Mailbox>,
    
    /// Configuration for the actor
    config: Option<ThreadActorConfig>,
    
    /// Current state of the worker
    state: Mutex<WorkerState>,
    
    /// Handle to the worker thread
    thread_handle: Mutex<Option<JoinHandle<()>>>,
    
    /// Sender for stopping the worker thread
    stop_tx: Mutex<Option<oneshot::Sender<()>>>,
    
    /// Tokio runtime handle
    runtime_handle: Handle,
    
    /// Reference to the actor system
    system_ref: Option<Weak<dyn SystemRef + Send + Sync>>,
    
    /// Called when the thread stops
    on_stop_command: Command,
}

/// Debug implementation for Worker
impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("path", &self.mailbox.path())
            .field("state", &*self.state.lock().unwrap())
            .field("has_thread", &self.thread_handle.lock().unwrap().is_some())
            .field("has_stop_channel", &self.stop_tx.lock().unwrap().is_some())
            .finish()
    }
}

/// Reference-counted command for worker operations
type Command = mpsc::Sender<String>;

impl Worker {
    /// Create a new worker for the given mailbox
    pub fn new(
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
        runtime_handle: Handle,
        system_ref: Option<Weak<dyn SystemRef + Send + Sync>>,
        on_stop_command: Command,
    ) -> Self {
        Self {
            mailbox,
            config,
            state: Mutex::new(WorkerState::Created),
            thread_handle: Mutex::new(None),
            stop_tx: Mutex::new(None),
            runtime_handle,
            system_ref,
            on_stop_command,
        }
    }
    
    /// Start the worker thread
    pub fn start(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Check and update state
        {
            let mut state = self.state.lock().unwrap();
            match *state {
                WorkerState::Created => *state = WorkerState::Running,
                _ => return Err(format!(
                    "Cannot start worker for {} - worker is in state {:?}",
                    self.mailbox.path(), *state
                ).into()),
            }
        }
        
        // Create stop channel
        let (stop_tx, stop_rx) = oneshot::channel();
        *self.stop_tx.lock().unwrap() = Some(stop_tx);
        
        // Clone required data for the thread
        let mailbox = Arc::clone(&self.mailbox);
        let config = self.config.clone();
        let system_ref = self.system_ref.clone();
        let runtime_handle = self.runtime_handle.clone();
        let on_stop_command = self.on_stop_command.clone();
        let path = mailbox.path().to_string();
        
        // Spawn worker thread
        let thread_handle = std::thread::Builder::new()
            .name(format!("dedicated-worker-{}", path))
            .spawn(move || {
                Self::worker_thread_main(
                    mailbox,
                    config,
                    stop_rx,
                    system_ref,
                    runtime_handle,
                    on_stop_command,
                    path,
                );
            })
            .map_err(|e| format!(
                "Failed to spawn worker thread for {}: {}", 
                self.mailbox.path(), e
            ))?;
        
        // Store thread handle
        *self.thread_handle.lock().unwrap() = Some(thread_handle);
        
        Ok(())
    }
    
    /// Stop the worker thread
    pub async fn stop(
        &self,
        wait: bool,
        wait_timeout: Option<u64>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Check and update state
        {
            let mut state = self.state.lock().unwrap();
            match *state {
                WorkerState::Running => *state = WorkerState::Stopping,
                WorkerState::Created | WorkerState::Stopped => {
                    // Already in a stopped state, nothing to do
                    return Ok(());
                },
                WorkerState::Stopping => {
                    // Already stopping, continue with join if requested
                },
            }
        }
        
        // Send stop signal
        if let Some(tx) = self.stop_tx.lock().unwrap().take() {
            // Send stop signal to thread
            let _ = tx.send(());
        }
        
        // If wait is requested, join the thread with optional timeout
        if wait {
            if let Some(thread_handle) = self.thread_handle.lock().unwrap().take() {
                if let Some(timeout_ms) = wait_timeout {
                    // Join with timeout
                    let timeout_duration = Duration::from_millis(timeout_ms);
                    let join_handle = tokio::task::spawn_blocking(move || {
                        thread_handle.join()
                    });
                    
                    match timeout(timeout_duration, join_handle).await {
                        Ok(Ok(thread_result)) => {
                            // Thread joined within timeout
                            match thread_result {
                                Ok(()) => {
                                    // Thread terminated successfully
                                    *self.state.lock().unwrap() = WorkerState::Stopped;
                                },
                                Err(e) => {
                                    // Thread panicked
                                    *self.state.lock().unwrap() = WorkerState::Stopped;
                                    return Err(format!(
                                        "Worker thread for {} panicked: {:?}",
                                        self.mailbox.path(), e
                                    ).into());
                                }
                            }
                        },
                        Ok(Err(e)) => {
                            // Join task failed
                            *self.state.lock().unwrap() = WorkerState::Stopped;
                            return Err(format!(
                                "Failed to join worker thread for {}: {}",
                                self.mailbox.path(), e
                            ).into());
                        },
                        Err(_) => {
                            // Timeout waiting for thread to join
                            return Err(format!(
                                "Timeout waiting for worker thread for {} to terminate",
                                self.mailbox.path()
                            ).into());
                        }
                    }
                } else {
                    // Join without timeout
                    let join_handle = tokio::task::spawn_blocking(move || {
                        thread_handle.join()
                    });
                    
                    match join_handle.await {
                        Ok(Ok(())) => {
                            // Thread terminated successfully
                            *self.state.lock().unwrap() = WorkerState::Stopped;
                        },
                        Ok(Err(e)) => {
                            // Thread panicked
                            *self.state.lock().unwrap() = WorkerState::Stopped;
                            return Err(format!(
                                "Worker thread for {} panicked: {:?}",
                                self.mailbox.path(), e
                            ).into());
                        },
                        Err(e) => {
                            // Join task failed
                            *self.state.lock().unwrap() = WorkerState::Stopped;
                            return Err(format!(
                                "Failed to join worker thread for {}: {}",
                                self.mailbox.path(), e
                            ).into());
                        }
                    }
                }
            }
        } else {
            // Not waiting, but mark as stopping
            *self.state.lock().unwrap() = WorkerState::Stopping;
        }
        
        Ok(())
    }
    
    /// Get the current state of the worker
    pub fn state(&self) -> WorkerState {
        *self.state.lock().unwrap()
    }
    
    /// Get the path of the mailbox
    pub fn path(&self) -> &str {
        self.mailbox.path()
    }
    
    /// Main function for the worker thread
    fn worker_thread_main(
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
        mut stop_rx: oneshot::Receiver<()>,
        system_ref: Option<Weak<dyn SystemRef + Send + Sync>>,
        runtime_handle: Handle,
        on_stop_command: Command,
        path: String,
    ) {
        let result = std::panic::catch_unwind(|| {
            // Enter runtime context
            let _guard = runtime_handle.enter();
            
            // Get configuration
            let config = config.unwrap_or_default();
            
            // Process messages until stopped
            let mut should_stop = false;
            
            while !should_stop {
                // Check for stop signal
                match stop_rx.try_recv() {
                    Ok(()) | Err(oneshot::error::TryRecvError::Closed) => {
                        // Stop signal received or channel closed
                        should_stop = true;
                        break;
                    },
                    Err(oneshot::error::TryRecvError::Empty) => {
                        // No stop signal, continue processing
                    }
                }
                
                // Process messages until mailbox is empty or stop requested
                while !should_stop {
                    // Process a batch of messages
                    let batch_processed = mailbox.process_batch(config.batch_size);
                    
                    // Check for stop signal after batch
                    match stop_rx.try_recv() {
                        Ok(()) | Err(oneshot::error::TryRecvError::Closed) => {
                            // Stop signal received or channel closed
                            should_stop = true;
                            break;
                        },
                        Err(oneshot::error::TryRecvError::Empty) => {
                            // No stop signal, continue processing
                        }
                    }
                    
                    // If no messages were processed in this batch, break from inner loop
                    if batch_processed == 0 {
                        break;
                    }
                }
                
                // If stop requested, exit
                if should_stop {
                    break;
                }
                
                // Sleep for a bit if no messages were processed
                std::thread::sleep(Duration::from_millis(config.idle_sleep_ms));
            }
            
            // Final cleanup - process any remaining messages if configured
            if config.drain_on_shutdown {
                mailbox.process_all();
            }
        });
        
        // Handle panic or normal termination
        match result {
            Ok(()) => {
                // Normal termination
                eprintln!("Worker thread for {} terminated normally", path);
            },
            Err(e) => {
                // Panic occurred
                let panic_msg = if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = e.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "Unknown panic".to_string()
                };
                
                eprintln!("Worker thread for {} panicked: {}", path, panic_msg);
                
                // Report panic to system
                if let Some(system_ref) = system_ref {
                    if let Some(system) = system_ref.upgrade() {
                        system.handle_panic(&path, panic_msg);
                    }
                }
            }
        }
        
        // Notify pool that worker has stopped
        let _ = on_stop_command.try_send(path);
    }
} 