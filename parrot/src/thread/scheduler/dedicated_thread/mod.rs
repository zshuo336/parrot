//! # Dedicated Thread Pool Module
//!
//! This module provides a dedicated thread pool implementation for compute-intensive actors.
//! Unlike shared thread pools, dedicated pools assign a dedicated thread per actor.
//!
//! ## Key Concepts
//! - Dedicated threads: One thread per compute-intensive actor
//! - Thread limits: Controls on maximum concurrent dedicated threads
//! - Auto-scaling: Thread management based on system resources
//!
//! ## Design Principles
//! - Isolation: Each actor runs on its own thread for predictable performance
//! - Resource control: Limits on total dedicated threads to prevent system overload
//! - Simplicity: Straightforward API for scheduling and descheduling actors

mod worker;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::fmt;
use std::error::Error;

use tokio::runtime::Handle;
use tokio::sync::{oneshot, mpsc};

use crate::thread::mailbox::Mailbox;
use crate::thread::config::ThreadActorConfig;

use self::worker::Worker;

/// Interface for actor system operations
pub trait SystemRef: fmt::Debug {
    /// Look up an actor by path
    fn lookup(&self, path: &str) -> Option<Arc<dyn Mailbox>>;
    
    /// Handle worker panic
    fn handle_panic(&self, path: &str, error: String);
}

/// Commands for the pool manager
enum Command {
    /// Schedule an actor on a dedicated thread
    Schedule(String, Arc<dyn Mailbox>, Option<ThreadActorConfig>),
    
    /// Deschedule an actor
    Deschedule(String),
    
    /// Shut down the thread pool
    Shutdown(oneshot::Sender<()>),
}

/// Internal pool state
struct PoolState {
    /// Map of actor paths to worker handles
    workers: HashMap<String, Worker>,
    
    /// Current number of active threads
    active_threads: usize,
    
    /// Maximum number of threads allowed
    max_threads: usize,
    
    /// Tokio runtime handle
    runtime_handle: Handle,
    
    /// Reference to the actor system
    system_ref: Option<Weak<dyn SystemRef + Send + Sync>>,
}

/// Configuration for the dedicated thread pool
#[derive(Debug, Clone)]
pub struct DedicatedThreadPoolConfig {
    /// Maximum number of dedicated threads allowed
    pub max_threads: usize,
    
    /// Command channel capacity
    pub command_channel_capacity: usize,
}

impl Default for DedicatedThreadPoolConfig {
    fn default() -> Self {
        Self {
            max_threads: 32,
            command_channel_capacity: 1000,
        }
    }
}

/// Dedicated thread pool for compute-intensive actors
#[derive(Clone)]
pub struct DedicatedThreadPool {
    /// Shared pool state
    state: Arc<Mutex<PoolState>>,
    
    /// Command channel sender
    command_tx: mpsc::Sender<Command>,
}

impl DedicatedThreadPool {
    /// Create a new dedicated thread pool
    pub fn new(
        config: Option<DedicatedThreadPoolConfig>,
        runtime_handle: Handle, 
        system_ref: Option<Weak<dyn SystemRef + Send + Sync>>,
    ) -> Self {
        let config = config.unwrap_or_default();
        
        // Create initial state
        let state = Arc::new(Mutex::new(PoolState {
            workers: HashMap::new(),
            active_threads: 0,
            max_threads: config.max_threads,
            runtime_handle: runtime_handle.clone(),
            system_ref: system_ref.clone(),
        }));
        
        // Create command channel
        let (command_tx, command_rx) = mpsc::channel(config.command_channel_capacity);
        
        // Start manager task
        let state_clone = Arc::clone(&state);
        let manager_tx = command_tx.clone();
        
        runtime_handle.spawn(async move {
            Self::manager_task(state_clone, command_rx, manager_tx).await;
        });
        
        Self { state, command_tx }
    }
    
    /// Schedule an actor on a dedicated thread
    pub async fn schedule(
        &self,
        actor_path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Check if already scheduled
        {
            let state = self.state.lock().unwrap();
            if state.workers.contains_key(actor_path) {
                return Err(format!("Actor {} is already scheduled on a dedicated thread", actor_path).into());
            }
            
            // Check thread limit
            if state.active_threads >= state.max_threads {
                return Err(format!(
                    "Cannot schedule actor {} - maximum number of dedicated threads ({}) reached",
                    actor_path, state.max_threads
                ).into());
            }
        }
        
        // Send schedule command
        let cmd = Command::Schedule(actor_path.to_string(), mailbox, config);
        self.command_tx.send(cmd).await
            .map_err(|_| "Failed to send schedule command - pool may be shutting down".into())
    }
    
    /// Deschedule an actor from its dedicated thread
    pub async fn deschedule(
        &self,
        actor_path: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Check if scheduled
        {
            let state = self.state.lock().unwrap();
            if !state.workers.contains_key(actor_path) {
                return Err(format!("Actor {} is not scheduled on a dedicated thread", actor_path).into());
            }
        }
        
        // Send deschedule command
        let cmd = Command::Deschedule(actor_path.to_string());
        self.command_tx.send(cmd).await
            .map_err(|_| "Failed to send deschedule command - pool may be shutting down".into())
    }
    
    /// Shutdown the thread pool, stopping all workers
    pub async fn shutdown(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Create response channel
        let (tx, rx) = oneshot::channel();
        
        // Send shutdown command
        let cmd = Command::Shutdown(tx);
        self.command_tx.send(cmd).await
            .map_err(|_| "Failed to send shutdown command - pool may already be shutting down".into())?;
        
        // Wait for completion
        rx.await
            .map_err(|_| "Failed to receive shutdown completion - manager task may have crashed".into())
    }
    
    /// Get the current number of active dedicated threads
    pub fn active_threads(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.active_threads
    }
    
    /// Get the maximum allowed number of dedicated threads
    pub fn max_threads(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.max_threads
    }
    
    /// Check if an actor is scheduled on a dedicated thread
    pub fn is_scheduled(&self, actor_path: &str) -> bool {
        let state = self.state.lock().unwrap();
        state.workers.contains_key(actor_path)
    }
    
    /// Task that processes commands for the thread pool
    async fn manager_task(
        state: Arc<Mutex<PoolState>>,
        mut command_rx: mpsc::Receiver<Command>,
        command_tx: mpsc::Sender<Command>,
    ) {
        while let Some(cmd) = command_rx.recv().await {
            match cmd {
                Command::Schedule(path, mailbox, config) => {
                    if let Err(e) = Self::handle_schedule(&state, path, mailbox, config).await {
                        eprintln!("Error scheduling actor: {}", e);
                    }
                },
                Command::Deschedule(path) => {
                    if let Err(e) = Self::handle_deschedule(&state, &path).await {
                        eprintln!("Error descheduling actor: {}", e);
                    }
                },
                Command::Shutdown(response_tx) => {
                    match Self::handle_shutdown(&state).await {
                        Ok(()) => {
                            let _ = response_tx.send(());
                        },
                        Err(e) => {
                            eprintln!("Error during shutdown: {}", e);
                            let _ = response_tx.send(());
                        }
                    }
                    
                    // Exit task after shutdown
                    break;
                }
            }
        }
    }
    
    /// Handle a schedule command
    async fn handle_schedule(
        state: &Arc<Mutex<PoolState>>,
        path: String,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Get runtime handle and system reference
        let (runtime_handle, system_ref, command_tx) = {
            let state = state.lock().unwrap();
            
            // Check if already scheduled
            if state.workers.contains_key(&path) {
                return Err(format!("Actor {} is already scheduled", path).into());
            }
            
            // Check thread limit
            if state.active_threads >= state.max_threads {
                return Err(format!(
                    "Maximum number of dedicated threads ({}) reached", 
                    state.max_threads
                ).into());
            }
            
            (state.runtime_handle.clone(), state.system_ref.clone(), Command::Deschedule(path.clone()))
        };
        
        // Create and start worker
        let worker = Worker::new(
            mailbox,
            config,
            runtime_handle,
            system_ref,
            command_tx,
        );
        
        // Start the worker
        worker.start()?;
        
        // Add to active workers
        let mut state = state.lock().unwrap();
        state.workers.insert(path, worker);
        state.active_threads += 1;
        
        Ok(())
    }
    
    /// Handle a deschedule command
    async fn handle_deschedule(
        state: &Arc<Mutex<PoolState>>,
        path: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Get worker
        let worker = {
            let mut state = state.lock().unwrap();
            
            // Check if scheduled
            state.workers.remove(path).ok_or_else(|| {
                format!("Actor {} is not scheduled", path).into()
            })
        }?;
        
        // Stop the worker
        worker.stop(true, None).await?;
        
        // Update active thread count
        let mut state = state.lock().unwrap();
        state.active_threads = state.active_threads.saturating_sub(1);
        
        Ok(())
    }
    
    /// Handle a shutdown command
    async fn handle_shutdown(
        state: &Arc<Mutex<PoolState>>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Get all workers
        let workers = {
            let mut state = state.lock().unwrap();
            std::mem::take(&mut state.workers)
        };
        
        // Stop all workers in parallel
        let mut tasks = Vec::new();
        
        for (path, worker) in workers {
            let stop_task = tokio::spawn(async move {
                let result = worker.stop(true, None).await;
                (path, result)
            });
            
            tasks.push(stop_task);
        }
        
        // Collect results
        let mut errors = Vec::new();
        
        for task in tasks {
            match task.await {
                Ok((path, Ok(()))) => {
                    // Worker stopped successfully
                },
                Ok((path, Err(e))) => {
                    errors.push(format!("Error stopping worker for {}: {}", path, e));
                },
                Err(e) => {
                    errors.push(format!("Task for stopping worker failed: {}", e));
                }
            }
        }
        
        // Update state
        let mut state = state.lock().unwrap();
        state.active_threads = 0;
        
        // Return errors if any
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n").into())
        }
    }
}

// Implement Debug for DedicatedThreadPool
impl fmt::Debug for DedicatedThreadPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock().unwrap();
        
        f.debug_struct("DedicatedThreadPool")
            .field("active_threads", &state.active_threads)
            .field("max_threads", &state.max_threads)
            .field("worker_count", &state.workers.len())
            .finish()
    }
} 