use std::sync::Arc;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, error, info};
use anyhow;

use parrot_api::actor::Actor;
use parrot_api::errors::ActorError;
use parrot_api::types::{BoxedMessage, ActorResult};

use crate::thread::actor::ThreadActor;
use crate::thread::context::ThreadContext;
use crate::thread::mailbox::Mailbox;
use crate::thread::envelope::ControlMessage;
use crate::thread::error::SystemError;
use crate::thread::config::ThreadActorConfig;

use std::sync::Mutex;
use std::panic;
use std::panic::AssertUnwindSafe;


/// Processor status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessorStatus {
    /// Processor is initializing
    Initializing = 0,
    
    /// Processor is running normally
    Running = 1,
    
    /// Processor is paused
    Paused = 2,
    
    /// Processor is stopping
    Stopping = 3,
    
    /// Processor is stopped
    Stopped = 4,
    
    /// Processor has failed
    Failed = 5,
}

/// Worker stats collection
#[derive(Debug, Clone, Default)]
pub struct WorkerStats {
    /// Number of messages processed
    pub messages_processed: Arc<AtomicUsize>,
    
    /// Number of errors encountered
    pub errors_encountered: Arc<AtomicUsize>,
    
    /// Time spent processing messages (nanoseconds)
    pub processing_time_ns: Arc<AtomicUsize>,
}

impl WorkerStats {
    pub fn new() -> Self {
        Self {
            messages_processed: Arc::new(AtomicUsize::new(0)),
            errors_encountered: Arc::new(AtomicUsize::new(0)),
            processing_time_ns: Arc::new(AtomicUsize::new(0)),
        }
    }
}


/// Actor processor, responsible for managing Actor's resources and message processing
/// 
/// # Key Responsibilities
/// - Managing actor lifecycle (initialize, start, stop)
/// - Processing individual actor messages
/// - Batched message processing
/// - Error handling and recovery
/// - State management
///
/// # Implementation Details
/// ## Core Algorithm
/// 1. Initialize actor and start it
/// 2. Process messages individually or in batches
/// 3. Handle errors with appropriate recovery strategies
/// 4. Maintain processor state for proper lifecycle management
///
/// ## Performance Characteristics
/// - Optimized for throughput with batch processing
/// - Yield control between messages for cooperative multitasking
/// - Resource cleanup on termination
///
/// ## Safety Considerations
/// - Error handling with status tracking
/// - Proper actor lifecycle management
/// - Resource cleanup guarantees
#[derive(Debug)]
pub struct ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    /// Actor instance
    actor: ThreadActor<A>,
    
    /// Actor context
    context: ThreadContext<A>,
    
    /// Actor mailbox
    mailbox: Arc<dyn Mailbox>,
    
    /// Actor path
    path: String,
    
    /// Actor config
    config: ThreadActorConfig,
    
    /// Processor status
    status: Arc<AtomicUsize>,
    
    /// Processor stats
    stats: Arc<WorkerStats>,
}

impl<A> ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    /// Create a new Actor processor
    pub fn new(
        actor: ThreadActor<A>,
        context: ThreadContext<A>,
        mailbox: Arc<dyn Mailbox>,
        path: String,
        config: ThreadActorConfig,
    ) -> Self {
        Self {
            actor,
            context,
            mailbox,
            path,
            config,
            status: Arc::new(AtomicUsize::new(ProcessorStatus::Initializing as usize)),
            stats: Arc::new(WorkerStats::new()),
        }
    }
    
    /// Initialize the actor
    pub async fn initialize_actor(&mut self) -> Result<(), SystemError> {
        debug!("Initializing actor at {}", self.path);
        if let Err(e) = self.actor.initialize(&mut self.context).await {
            error!("Failed to initialize actor at {}: {}", self.path, e);
            self.status.store(ProcessorStatus::Failed as usize, Ordering::SeqCst);
            self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
            return Err(SystemError::ActorCreationError(format!(
                "Failed to initialize actor at {}: {}", self.path, e
            )));
        }
        
        Ok(())
    }
    
    /// Send Start control message to the actor
    pub async fn start_actor(&mut self) -> Result<(), SystemError> {
        debug!("Sending Start message to actor at {}", self.path);
        let start_msg = Box::new(ControlMessage::Start);
        if let Err(e) = self.actor.process_message(start_msg, &mut self.context).await {
            error!("Failed to start actor at {}: {}", self.path, e);
            self.status.store(ProcessorStatus::Failed as usize, Ordering::SeqCst);
            self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
            return Err(SystemError::ActorCreationError(format!(
                "Failed to start actor at {}: {}", self.path, e
            )));
        }
        
        self.status.store(ProcessorStatus::Running as usize, Ordering::SeqCst);
        Ok(())
    }
    
    /// Send Stop control message to the actor
    pub async fn stop_actor(&mut self) -> Result<(), SystemError> {
        debug!("Sending Stop message to actor at {}", self.path);
        self.status.store(ProcessorStatus::Stopping as usize, Ordering::SeqCst);
        
        let stop_msg = Box::new(ControlMessage::Stop);
        if let Err(e) = self.actor.process_message(stop_msg, &mut self.context).await {
            error!("Failed to stop actor at {}: {}", self.path, e);
            self.status.store(ProcessorStatus::Failed as usize, Ordering::SeqCst);
            self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
            return Err(SystemError::ActorCreationError(format!(
                "Failed to stop actor at {}: {}", self.path, e
            )));
        }
        
        self.status.store(ProcessorStatus::Stopped as usize, Ordering::SeqCst);
        Ok(())
    }
    
    /// Initialize and start the actor
    pub async fn initialize_and_start(&mut self) -> Result<(), SystemError> {
        self.initialize_actor().await?;
        self.start_actor().await?;
        Ok(())
    }
    
    /// Process a single message
    pub async fn process_message(&mut self, msg: BoxedMessage) -> ActorResult<BoxedMessage> {
        // Only process messages when in Running state
        if self.get_status() != ProcessorStatus::Running {
            return Err(anyhow::Error::msg(format!("Actor at {} is not running, current status: {:?}", 
                self.path, self.get_status())).into());
        }

        let process_result = panic::catch_unwind(AssertUnwindSafe(|| {
            self.actor.process_message(msg, &mut self.context)
        }));
        
        let result: ActorResult<BoxedMessage> = match process_result {
            Ok(future) => {
                // execute the future to process message
                match future.await {
                    Ok(result) => {
                        Ok(result)
                    },
                    Err(e) => {
                        // message processing error
                        error!("Error processing message for actor {}: {}", self.path, e);
                        Err(e)
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
                
                Err(ActorError::Panic(error_msg))
            }
        };
                
        // Update metrics
        self.stats.messages_processed.fetch_add(1, Ordering::SeqCst);
        
        // Handle errors but don't change processor status
        if let Err(_) = result {
            self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
        }
        
        result
    }
    
    /// Process a batch of messages
    /// 
    /// # Arguments
    /// * `max_messages` - Maximum number of messages to process in this batch
    /// * `yield_after_each_message` - Whether to yield to the scheduler after each message
    /// 
    /// # Returns
    /// A tuple containing:
    /// * Number of messages processed
    /// * Whether any errors were encountered
    pub async fn process_batch(&mut self, max_messages: usize, yield_after_each_message: bool) -> (usize, bool) {
        // Only process messages when in Running state
        if self.get_status() != ProcessorStatus::Running {
            return (0, false);
        }
        
        let mut processed = 0;
        let mut had_error = false;
        
        for _ in 0..max_messages {
            // Check if still in running state
            if self.get_status() != ProcessorStatus::Running {
                break;
            }
            
            // Get message from mailbox
            match self.mailbox.pop().await {
                Some(msg) => {
                    // Process message
                    match self.process_message(msg).await {
                        Ok(_) => {
                            // Message processed successfully
                            processed += 1;
                            
                            // Yield if configured to do so
                            if yield_after_each_message {
                                tokio::task::yield_now().await;
                            }
                        },
                        Err(e) => {
                            // Record error but continue processing
                            processed += 1;
                            had_error = true;
                            // if panic, break the processing loop
                            if let ActorError::Panic(_) = e {
                                debug!("Panic in actor {}: {} break the processing loop", self.path, e);
                                break;
                            }
                        }
                    }
                },
                None => {
                    // No more messages, end batch processing
                    break;
                }
            }
        }
        
        (processed, had_error)
    }
    
    /// Pause the processor
    pub fn pause(&self) {
        debug!("Pausing actor processor for {}", self.path);
        self.status.store(ProcessorStatus::Paused as usize, Ordering::SeqCst);
    }
    
    /// Resume the processor
    pub fn resume(&self) {
        debug!("Resuming actor processor for {}", self.path);
        self.status.store(ProcessorStatus::Running as usize, Ordering::SeqCst);
    }
    
    /// Get current processor status
    pub fn get_status(&self) -> ProcessorStatus {
        match self.status.load(Ordering::SeqCst) {
            0 => ProcessorStatus::Initializing,
            1 => ProcessorStatus::Running,
            2 => ProcessorStatus::Paused,
            3 => ProcessorStatus::Stopping,
            4 => ProcessorStatus::Stopped,
            5 => ProcessorStatus::Failed,
            _ => ProcessorStatus::Failed, // Default to Failed for unknown status
        }
    }
    
    /// Get message statistics
    pub fn get_statistics(&self) -> Arc<WorkerStats> {
        self.stats.clone()
    }
    
    /// Get mailbox reference
    pub fn mailbox(&self) -> Arc<dyn Mailbox> {
        self.mailbox.clone()
    }
    
    /// Get actor path
    pub fn path(&self) -> &str {
        &self.path
    }
    
    /// Check if processor has pending messages
    pub async fn has_pending_messages(&self) -> bool {
        !self.mailbox.is_empty().await
    }
} 