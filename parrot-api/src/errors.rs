
/// Actor error types
#[derive(thiserror::Error, Debug)]
pub enum ActorError {
    #[error("Actor initialization failed: {0}")]
    InitializationError(String),
    
    #[error("Message handling failed: {0}")]
    MessageHandlingError(String),
    
    #[error("Actor stopped")]
    Stopped,
    
    #[error("Timeout")]
    Timeout,
    
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

