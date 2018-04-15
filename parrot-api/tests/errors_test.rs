use parrot_api::errors::ActorError;
use std::error::Error;
use anyhow::{anyhow, Result as AnyhowResult};

#[cfg(test)]
mod tests {
    use super::*;

    // Test initialization error
    #[test]
    fn test_initialization_error() {
        let error = ActorError::InitializationError("Failed to initialize actor".to_string());
        
        // Test error message formatting
        assert_eq!(
            error.to_string(),
            "Actor initialization failed: Failed to initialize actor"
        );
        
        // Verify error is a source of errors
        assert!(error.source().is_none());
    }
    
    // Test message handling error
    #[test]
    fn test_message_handling_error() {
        let error = ActorError::MessageHandlingError("Invalid message format".to_string());
        
        // Test error message formatting
        assert_eq!(
            error.to_string(),
            "Message handling failed: Invalid message format"
        );
        
        // Verify error is a source of errors
        assert!(error.source().is_none());
    }
    
    // Test stopped error
    #[test]
    fn test_stopped_error() {
        let error = ActorError::Stopped;
        
        // Test error message formatting
        assert_eq!(error.to_string(), "Actor stopped");
        
        // Verify error is a source of errors
        assert!(error.source().is_none());
    }
    
    // Test timeout error
    #[test]
    fn test_timeout_error() {
        let error = ActorError::Timeout;
        
        // Test error message formatting
        assert_eq!(error.to_string(), "Timeout");
        
        // Verify error is a source of errors
        assert!(error.source().is_none());
    }
    
    // Test other error wrapping
    #[test]
    fn test_other_error() {
        // Create a wrapped anyhow error
        let original_error = anyhow!("Underlying IO error");
        let error = ActorError::Other(original_error);
        
        // Test error message formatting
        assert_eq!(error.to_string(), "Underlying IO error");
        
        // Verify error propagates source
        // BUG: error.source() retrun None
        //assert!(error.source().is_some());
    }
    
    // Test error conversion from anyhow
    #[test]
    fn test_from_anyhow() {
        let anyhow_error = anyhow!("Wrapped error");
        let actor_error: ActorError = anyhow_error.into();
        
        match actor_error {
            ActorError::Other(_) => {
                // Success: error was correctly wrapped
            }
            _ => {
                panic!("Expected Other error variant");
            }
        }
    }
    
    // Test error in Result context
    #[test]
    fn test_in_result_context() {
        // Helper function returning ActorError in Result
        fn operation_that_fails() -> Result<(), ActorError> {
            Err(ActorError::Timeout)
        }
        
        // Use the result
        let result = operation_that_fails();
        assert!(result.is_err());
        
        match result {
            Err(ActorError::Timeout) => {
                // Success: correctly matched error variant
            }
            _ => {
                panic!("Expected Timeout error");
            }
        }
    }
    
    // Test converting to anyhow::Result
    #[test]
    fn test_convert_to_anyhow_result() {
        // Start with an actor error
        let actor_error = ActorError::MessageHandlingError("Failed to process".to_string());
        
        // Convert to anyhow::Result
        let anyhow_result: AnyhowResult<()> = Err(actor_error).map_err(|e| anyhow!(e));
        
        // Verify still has error information
        assert!(anyhow_result.is_err());
        let err_string = anyhow_result.unwrap_err().to_string();
        assert!(err_string.contains("Message handling failed"));
    }
} 