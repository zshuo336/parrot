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

use std::sync::{Arc, Weak, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::Duration;
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::collections::HashMap;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::thread;

use tokio::runtime::Handle;
use tokio::sync::{oneshot, mpsc};
use tokio::task::JoinHandle;
use tokio::time;
use anyhow::anyhow;
use tracing::{debug, error, info, warn};

use crate::thread::mailbox::Mailbox;
use crate::thread::error::SystemError;
use crate::thread::config::ThreadActorConfig;
use crate::thread::system::ThreadActorSystem;
use parrot_api::types::BoxedMessage;

// Constants for configuration
const DEFAULT_BATCH_SIZE: usize = 10;
const DEFAULT_IDLE_SLEEP_DURATION: Duration = Duration::from_millis(10);
const DEFAULT_THREAD_STACK_SIZE: usize = 3 * 1024 * 1024; // 3MB

/// Commands sent to worker threads
#[derive(Debug)]
pub enum WorkerCommand {
    /// Shutdown the worker
    Shutdown,
    
    /// Pause processing
    Pause,
    
    /// Resume processing
    Resume,
    
    /// Stop actor with callback
    Stop(oneshot::Sender<Result<(), SystemError>>),
}

/// Worker thread state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// Worker is initializing
    Initializing = 0,
    
    /// Worker is idle, waiting for work
    Idle = 1,
    
    /// Worker is processing a message
    Processing = 2,
    
    /// Worker is shutting down
    ShuttingDown = 3,
    
    /// Worker has encountered an error
    Error = 4,
}

/// Dedicated worker implementation for compute-intensive actors
pub struct Worker {
    /// Actor path
    path: String,
    
    /// Actor mailbox
    mailbox: Arc<dyn Mailbox>,
    
    /// Worker configuration
    config: ThreadActorConfig,
    
    /// Shutdown flag
    shutdown_flag: Arc<AtomicBool>,
    
    /// Worker state
    state: Arc<AtomicUsize>,
    
    /// Stats collection
    stats: Mutex<WorkerStats>,
    
    /// Worker thread join handle
    join_handle: Mutex<Option<JoinHandle<()>>>,
    
    /// Number of messages to process in a batch
    batch_size: usize,
    
    /// Command channel for worker control
    command_tx: mpsc::Sender<WorkerCommand>,
    
    /// Command receiver
    command_rx: Mutex<Option<mpsc::Receiver<WorkerCommand>>>,
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("path", &self.path)
            .field("state", &self.get_state())
            .field("batch_size", &self.batch_size)
            .field("stats", &self.get_stats())
            .finish()
    }
}

impl Clone for Worker {
    fn clone(&self) -> Self {
        // Create a new command channel for the clone
        let (command_tx, command_rx) = mpsc::channel(32);
        
        Self {
            path: self.path.clone(),
            mailbox: self.mailbox.clone(),
            config: self.config.clone(),
            shutdown_flag: self.shutdown_flag.clone(),
            state: self.state.clone(),
            stats: Mutex::new(self.get_stats()),
            join_handle: Mutex::new(None),
            batch_size: self.batch_size,
            command_tx,
            command_rx: Mutex::new(Some(command_rx)),
        }
    }
}

impl Worker {
    /// Create a new dedicated worker for an actor
    pub fn new(
        path: String,
        mailbox: Arc<dyn Mailbox>,
        config: ThreadActorConfig,
        batch_size: usize,
    ) -> Self {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let state = Arc::new(AtomicUsize::new(WorkerState::Initializing as usize));
        let stats = Mutex::new(WorkerStats::default());
        
        // Create command channel
        let (command_tx, command_rx) = mpsc::channel(32);
        
        Self {
            path,
            mailbox,
            config,
            shutdown_flag,
            state,
            stats,
            join_handle: Mutex::new(None),
            batch_size,
            command_tx,
            command_rx: Mutex::new(Some(command_rx)),
        }
    }
    
    /// Start this worker
    pub fn start(&self) -> Result<(), SystemError> {
        debug!("Starting dedicated worker for actor {}", self.path);
        
        // Get command receiver
        let command_rx = match self.command_rx.lock().unwrap().take() {
            Some(rx) => rx,
            None => return Err(SystemError::ThreadSetupError(
                format!("Command receiver already taken for worker {}", self.path)
            )),
        };
        
        // Create clones for the new thread
        let shutdown_flag = self.shutdown_flag.clone();
        let state = self.state.clone();
        let path = self.path.clone();
        let worker_self = self.clone();
        
        // Get thread stack size from config or use default
        let thread_stack_size = self.config.thread_stack_size
            .unwrap_or(DEFAULT_THREAD_STACK_SIZE);
        
        // Create OS thread for this worker
        let builder = std::thread::Builder::new()
            .name(format!("actor-dedicated-{}", self.path))
            .stack_size(thread_stack_size);
        
        let thread_handle = builder.spawn(move || {
            // Create a dedicated tokio runtime for this OS thread
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .thread_name(format!("tokio-dedicated-{}", path))
                .build()
                .unwrap_or_else(|e| {
                    panic!("Failed to create tokio runtime for worker {}: {}", path, e);
                });
            
            // Set thread local state
            state.store(WorkerState::Initializing as usize, Ordering::Relaxed);
            
            // Run the worker's run method inside this dedicated runtime
            match rt.block_on(async {
                // Run the worker's main loop
                worker_self.run(command_rx).await;
            }) {
                Ok(_) => debug!("Worker {} completed successfully", path),
                Err(e) => error!("Worker {} failed with error: {:?}", path, e)
            }
            
            debug!("OS thread for worker {} terminated", path);
        }).map_err(|e| {
            SystemError::ThreadSetupError(format!("Failed to spawn thread for worker {}: {}", self.path, e))
        })?;
        
        // Store the OS thread join handle
        let mut join_handle = self.join_handle.lock().unwrap();
        // We need to wrap the std::thread::JoinHandle in a compatible structure
        let tokio_handle = tokio::task::spawn_blocking(move || {
            if let Err(e) = thread_handle.join() {
                error!("Error joining worker thread: {:?}", e);
            }
        });
        *join_handle = Some(tokio_handle);
        
        Ok(())
    }
    
    /// Stop this worker
    pub async fn stop(&self) -> Result<(), SystemError> {
        debug!("Stopping dedicated worker for actor {}", self.path);
        
        // Send shutdown command
        if let Err(e) = self.command_tx.send(WorkerCommand::Shutdown).await {
            return Err(SystemError::ThreadShutdownError(
                format!("Failed to send shutdown command to worker {}: {}", self.path, e)
            ));
        }
        
        // Set shutdown flag directly in case command processing is blocked
        self.shutdown_flag.store(true, Ordering::Relaxed);
        
        // Wait for worker to finish
        let join_handle = self.join_handle.lock().unwrap().take();
        if let Some(handle) = join_handle {
            // Since we're now using tokio's JoinHandle that wraps std::thread::JoinHandle,
            // we can await it directly
            if let Err(e) = handle.await {
                return Err(SystemError::ThreadShutdownError(
                    format!("Error waiting for worker {} to stop: {:?}", self.path, e)
                ));
            }
        }
        
        Ok(())
    }
    
    /// Pause this worker
    pub async fn pause(&self) -> Result<(), SystemError> {
        if let Err(e) = self.command_tx.send(WorkerCommand::Pause).await {
            return Err(SystemError::Other(
                format!("Failed to send pause command to worker {}: {}", self.path, e)
            ));
        }
        
        Ok(())
    }
    
    /// Resume this worker
    pub async fn resume(&self) -> Result<(), SystemError> {
        if let Err(e) = self.command_tx.send(WorkerCommand::Resume).await {
            return Err(SystemError::Other(
                format!("Failed to send resume command to worker {}: {}", self.path, e)
            ));
        }
        
        Ok(())
    }
    
    /// Get current worker state
    pub fn get_state(&self) -> WorkerState {
        match self.state.load(Ordering::Relaxed) {
            0 => WorkerState::Initializing,
            1 => WorkerState::Idle,
            2 => WorkerState::Processing,
            3 => WorkerState::ShuttingDown,
            _ => WorkerState::Error,
        }
    }
    
    /// Get worker stats
    pub fn get_stats(&self) -> WorkerStats {
        self.stats.lock().unwrap().clone()
    }
    
    /// Get worker state atomic reference
    pub fn state_ref(&self) -> Arc<AtomicUsize> {
        self.state.clone()
    }
    
    /// Main worker thread loop
    async fn run(&self, mut command_rx: mpsc::Receiver<WorkerCommand>) {
        debug!("Starting dedicated worker for actor {}", self.path);
        
        // Update worker state to idle
        self.state.store(WorkerState::Idle as usize, Ordering::Relaxed);
        
        // Get idle sleep duration from config or use default
        let idle_sleep_duration = self.config.idle_sleep_duration
            .unwrap_or(DEFAULT_IDLE_SLEEP_DURATION);
            
        // Main processing loop
        while !self.shutdown_flag.load(Ordering::Relaxed) {
            // Check for commands
            match command_rx.try_recv() {
                Ok(WorkerCommand::Shutdown) => {
                    debug!("Received shutdown command for worker {}", self.path);
                    self.shutdown_flag.store(true, Ordering::Relaxed);
                    break;
                }
                Ok(WorkerCommand::Pause) => {
                    debug!("Received pause command for worker {}", self.path);
                    // Pause the processing - we'll implement this by not processing messages 
                    // and just waiting for the next command
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
                Ok(WorkerCommand::Resume) => {
                    debug!("Received resume command for worker {}", self.path);
                    // Resume normal processing
                }
                _ => { /* No command or other command, continue */ }
            }
            
            // Check if mailbox has messages
            if self.mailbox.is_empty().await {
                // No messages, sleep for a bit
                tokio::time::sleep(idle_sleep_duration).await;
                continue;
            }
            
            // Mailbox has messages, start processing
            self.state.store(WorkerState::Processing as usize, Ordering::Relaxed);
            
            // Process messages with panic recovery
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                self.process_messages(self.batch_size)
            }));
            
            match result {
                Ok(future) => {
                    // Execute the future to process messages
                    if let Err(e) = future.await {
                        error!("Error processing messages for actor {}: {}", self.path, e);
                        self.state.store(WorkerState::Error as usize, Ordering::Relaxed);
                    }
                }
                Err(panic_err) => {
                    // Worker thread panicked
                    let error_msg = match panic_err.downcast::<String>() {
                        Ok(string) => format!("Panic in worker for {}: {}", self.path, string),
                        Err(e) => format!("Panic in worker for {}: {:?}", self.path, e),
                    };
                    
                    error!("{}", error_msg);
                    
                    // Mark worker as having encountered an error
                    self.state.store(WorkerState::Error as usize, Ordering::Relaxed);
                }
            }
            
            // Reset state to idle
            self.state.store(WorkerState::Idle as usize, Ordering::Relaxed);
        }
        
        // Worker is shutting down
        self.state.store(WorkerState::ShuttingDown as usize, Ordering::Relaxed);
        debug!("Dedicated worker for actor {} shutting down", self.path);
    }
    
    /// Process messages using processor
    async fn process_messages(&self, max_messages: usize) -> anyhow::Result<()> {
        // Get yield flag from config or use default (false)
        let yield_after_each_message = self.config.yield_after_each_message
            .unwrap_or(false);
        
        // use processor to process messages
        if !self.mailbox.has_processor() {
            // if mailbox has no associated processor, record warning and skip processing
            warn!("Mailbox for actor {} has no associated processor", self.path);
            return Ok(());
        }
        
        // get processor reference
        let processor_ref_any = self.mailbox.get_processor_ref().ok_or_else(|| {
            anyhow!("Failed to get processor for actor {}", self.path)
        })?;
        
        let mut processed = 0;
        
        // process batch_size messages
        for _ in 0..max_messages {
            // get next message
            match self.mailbox.pop().await {
                Some(message) => {
                    // directly use processor to process message, not put back to mailbox
                    let process_result = panic::catch_unwind(AssertUnwindSafe(|| {
                        self.process_message_with_processor_ref(processor_ref_any, message)
                    }));
                    
                    match process_result {
                        Ok(future) => {
                            // execute the future to process message
                            match future.await {
                                Ok(_) => {
                                    // message processed successfully
                                    processed += 1;
                                    
                                    // update stats
                                    {
                                        let mut stats = self.stats.lock().unwrap();
                                        stats.messages_processed += 1;
                                    }
                                    
                                    // if yield after each message is configured, yield now
                                    if yield_after_each_message {
                                        tokio::task::yield_now().await;
                                    }
                                },
                                Err(e) => {
                                    // message processing error
                                    error!("Error processing message for actor {}: {}", self.path, e);
                                    
                                    // update stats
                                    {
                                        let mut stats = self.stats.lock().unwrap();
                                        stats.errors_encountered += 1;
                                    }
                                }
                            }
                        },
                        Err(panic_error) => {
                            // processor processing message panicked
                            let error_msg = match panic_error.downcast::<String>() {
                                Ok(string) => format!("Panic in actor {}: {}", self.path, string),
                                Err(e) => format!("Panic in actor {}: {:?}", self.path, e),
                            };
                            
                            error!("{}", error_msg);
                            
                            // update stats
                            {
                                let mut stats = self.stats.lock().unwrap();
                                stats.errors_encountered += 1;
                            }
                            
                            // break the processing loop
                            break;
                        }
                    }
                },
                None => {
                    // no more messages
                    break;
                }
            }
        }
        
        debug!("Processed {} messages for actor {}", processed, self.path);
        Ok(())
    }
    
    /// use processor to process message
    /// note: this method returns a Future, which is responsible for execution by the caller
    fn process_message_with_processor_ref(
        &self,
        processor_ref_any: Arc<dyn Any + Send + Sync>,
        message: BoxedMessage
    ) -> Pin<Box<dyn Future<Output = Result<(), anyhow::Error>> + Send + '_>> {
        Box::pin(async move {
            // first try to convert processor to the base type of ActorProcessor
            // so that we can directly call process_message method
            
            // try to convert processor to several possible processor types
            // note: in actual system, you should know the exact processor type
            
            // try to use system reference to process message
            if let Some(system) = self.system.upgrade() {
                // system should know how to process message
                // since ThreadContext has all related information, it can correctly process message
                // process message and return result
                return system.handle_message(&self.path, message).await
                    .map_err(|e| anyhow!("Error processing message via system: {}", e));
            }
            
            // we can try some direct conversions, although it's unlikely to succeed
            for &type_name in &["ActorProcessor", "ThreadProcessor", "DefaultProcessor"] {
                info!("Attempting to process message as {}", type_name);
            }
            
            // if we can't process, record warning
            warn!("Could not find suitable processor for actor {}", self.path);
            Err(anyhow!("Could not process message for actor {}", self.path))
        })
    }
} 