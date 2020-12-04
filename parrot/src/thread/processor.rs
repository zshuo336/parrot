use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};
use tokio::sync::mpsc::{self, Sender, Receiver};
use tokio::time::{sleep, timeout};

use parrot_api::actor::{Actor, ActorState};
use parrot_api::types::{BoxedMessage, ActorResult};
use parrot_api::errors::ActorError;
use anyhow::anyhow;

use crate::thread::actor::ThreadActor;
use crate::thread::context::ThreadContext;
use crate::thread::mailbox::Mailbox;
use crate::thread::envelope::ControlMessage;
use crate::thread::error::SystemError;
use crate::thread::config::ThreadActorConfig;
use crate::thread::config::DEFAULT_DEDICATED_THREAD_MAILBOX_CAPACITY;
/// message processor command
#[derive(Debug)]
enum ProcessorCommand {
    /// process a message batch
    ProcessBatch {
        /// max messages
        max_messages: usize,
    },
    
    /// stop processing
    Stop,
    
    /// pause processing
    Pause,
    
    /// resume processing
    Resume,
}

/// Actor processor, responsible for managing Actor's message loop and lifecycle
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
    
    /// command sender
    cmd_tx: Option<Sender<ProcessorCommand>>,
    
    /// command receiver
    cmd_rx: Option<Receiver<ProcessorCommand>>,
    
    /// whether running
    running: bool,
}

impl<A> ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    /// create a new Actor processor
    pub fn new(
        actor: ThreadActor<A>,
        context: ThreadContext<A>,
        mailbox: Arc<dyn Mailbox>,
        path: String,
        config: ThreadActorConfig,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        
        Self {
            actor,
            context,
            mailbox,
            path,
            config,
            cmd_tx: Some(cmd_tx),
            cmd_rx: Some(cmd_rx),
            running: false,
        }
    }
    
    /// get the command sender of the processor
    pub fn command_sender(&self) -> Option<Sender<ProcessorCommand>> {
        self.cmd_tx.clone()
    }
    
    /// start the processor
    pub async fn start(&mut self) -> Result<(), SystemError> {
        if self.running {
            return Ok(());
        }
        
        self.running = true;
        
        // initialize actor
        debug!("Initializing actor at {}", self.path);
        if let Err(e) = self.actor.initialize(&mut self.context).await {
            error!("Failed to initialize actor at {}: {}", self.path, e);
            return Err(SystemError::ActorCreationError(format!(
                "Failed to initialize actor at {}: {}", self.path, e
            )));
        }
        
        // send Start message
        debug!("Sending Start message to actor at {}", self.path);
        let start_msg = Box::new(ControlMessage::Start);
        if let Err(e) = self.actor.process_message(start_msg, &mut self.context).await {
            error!("Failed to start actor at {}: {}", self.path, e);
            return Err(SystemError::ActorCreationError(format!(
                "Failed to start actor at {}: {}", self.path, e
            )));
        }
        
        // get batch size
        let batch_size = match self.config.scheduling_mode.as_ref() {
            // shared pool
            Some(crate::thread::config::SchedulingMode::SharedPool { max_messages_per_run }) => {
                *max_messages_per_run
            },
            // dedicated thread
            Some(crate::thread::config::SchedulingMode::DedicatedThread) => {
                self.config.mailbox_capacity.unwrap_or(DEFAULT_DEDICATED_THREAD_MAILBOX_CAPACITY)
            },
            _ => 1024, // default batch size
        };
        
        // get processing period
        let mailbox = self.mailbox.clone();
        let path = self.path.clone();
        
        // create a separate command processing channel
        let (worker_tx, mut worker_rx) = mpsc::channel::<ProcessorCommand>(32);
        
        // take the command receiver from the current processor
        let mut cmd_rx = self.cmd_rx.take().expect("Command receiver already taken");
        
        // forward all commands
        let cmd_forward_path = path.clone();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if worker_tx.send(cmd).await.is_err() {
                    error!("Worker channel closed for {}, stopping command forwarding", cmd_forward_path);
                    break;
                }
            }
        });
        
        // create a new command sender, for external callers
        let cmd_tx_clone = worker_tx.clone();
        self.cmd_tx = Some(cmd_tx_clone);
        
        debug!("Starting processor command loop for actor at {}", path);
        
        // start the worker thread
        tokio::spawn(async move {
            // create the worker state
            let mut processor_state = ProcessorState {
                mailbox,
                path: path.clone(),
            };
            
            // first process a batch of messages
            if let Err(e) = processor_state.process_mailbox_batch(batch_size).await {
                error!("Initial batch processing failed for actor at {}: {}", path, e);
                return;
            }
            
            // then enter the command loop
            while let Some(cmd) = worker_rx.recv().await {
                match cmd {
                    ProcessorCommand::ProcessBatch { max_messages } => {
                        if let Err(e) = processor_state.process_mailbox_batch(max_messages).await {
                            error!("Error processing batch for actor at {}: {}", 
                                   path, e);
                            break;
                        }
                    },
                    ProcessorCommand::Stop => {
                        info!("Stopping actor processor for {}", path);
                        // only exit the loop, ActorProcessor.shutdown will handle the actual stop logic
                        break;
                    },
                    ProcessorCommand::Pause => {
                        debug!("Pausing actor processor for {}", path);
                        // pause processing, but keep the command loop running
                    },
                    ProcessorCommand::Resume => {
                        debug!("Resuming actor processor for {}", path);
                        if let Err(e) = processor_state.process_mailbox_batch(batch_size).await {
                            error!("Error processing batch during resume for actor at {}: {}", 
                                   path, e);
                            break;
                        }
                    }
                }
            }
            
            info!("Actor processor for {} has stopped", path);
        });
        
        Ok(())
    }
    
    /// shutdown the Actor processor
    pub async fn shutdown(&mut self) -> ActorResult<()> {
        debug!("Shutting down actor processor for {}", self.path);
        
        // send Stop message
        let stop_msg = Box::new(ControlMessage::Stop);
        self.actor.process_message(stop_msg, &mut self.context).await?;
        
        // close the command sender
        self.cmd_tx = None;
        
        // mark as not running
        self.running = false;
        
        Ok(())
    }
}

/// the state of the processor worker thread
struct ProcessorState {
    /// Actor mailbox
    mailbox: Arc<dyn Mailbox>,
    
    /// Actor path
    path: String,
}

impl ProcessorState {
    /// process a batch of mailbox messages
    async fn process_mailbox_batch(&mut self, max_messages: usize) -> ActorResult<()> {
        debug!("Processing batch of up to {} messages for actor at {}", 
               max_messages, self.path);
        
        // process the message batch
        let mut processed = 0;
        for _ in 0..max_messages {
            // try to get a message
            if let Some(msg) = self.mailbox.pop().await {
                // push the message back to the mailbox, let the main processor handle it
                // this is not the most efficient way, but avoids synchronization issues with ActorProcessor
                if let Err(e) = self.mailbox.push(msg, crate::thread::config::BackpressureStrategy::Block).await {
                    error!("Failed to push message back to mailbox for actor at {}: {}", 
                           self.path, e);
                    return Err(ActorError::MessageSendError(format!(
                        "Failed to push message back to mailbox: {}", e
                    )));
                }
                
                processed += 1;
            } else {
                // mailbox is empty, batch processing done
                break;
            }
        }
        
        debug!("Processed {} messages for actor at {}", processed, self.path);
        
        // if the mailbox is not empty, schedule the next batch
        if !self.mailbox.is_empty() {
            // the mailbox is not empty, will be processed in the next loop
        }
        
        Ok(())
    }
} 