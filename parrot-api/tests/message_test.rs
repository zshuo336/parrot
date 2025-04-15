use parrot_api::message::{Message, MessageEnvelope, MessageOptions, RetryPolicy, BackoffStrategy};
use parrot_api::types::{BoxedMessage, ActorResult};
use parrot_api::errors::ActorError;
use parrot_api::priority::{BACKGROUND, LOW, NORMAL, HIGH, CRITICAL};
use std::any::Any;
use std::time::Duration;
use uuid::Uuid;

// Test messages
#[derive(Debug, Clone)]
struct TestRequest(String);

#[derive(Debug, Clone, PartialEq, Eq)]
struct TestResponse(String);

// Implement Message trait for TestRequest
impl Message for TestRequest {
    type Result = TestResponse;
    
    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        if let Ok(response) = result.downcast::<TestResponse>() {
            Ok(*response)
        } else {
            Err(ActorError::MessageHandlingError("Failed to extract TestResponse".to_string()))
        }
    }
}

// Error message test
#[derive(Debug, Clone)]
struct ErrorMessage;

impl Message for ErrorMessage {
    type Result = ();
    
    fn extract_result(_: BoxedMessage) -> ActorResult<Self::Result> {
        Err(ActorError::MessageHandlingError("This always fails".to_string()))
    }
}

// Unit message test (no result)
#[derive(Debug, Clone)]
struct UnitMessage;

impl Message for UnitMessage {
    type Result = ();
    
    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        if result.downcast::<()>().is_ok() {
            Ok(())
        } else {
            Err(ActorError::MessageHandlingError("Failed to extract unit result".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::message::MessagePriority;
    
    // Test Message trait implementation
    #[test]
    fn test_message_trait() {
        // Create a test message and box it
        let msg = TestRequest("Hello".to_string());
        let boxed: BoxedMessage = Box::new(msg);
        
        // Test downcasting
        let unboxed = boxed.downcast::<TestRequest>();
        assert!(unboxed.is_ok());
        assert_eq!(unboxed.unwrap().0, "Hello");
    }
    
    // Test result extraction
    #[test]
    fn test_result_extraction() {
        let response = TestResponse("World".to_string());
        let boxed: BoxedMessage = Box::new(response);
        
        // Extract using the Message trait
        let result = TestRequest::extract_result(boxed);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), TestResponse("World".to_string()));
    }
    
    // Test error message
    #[test]
    fn test_error_message() {
        let boxed: BoxedMessage = Box::new(());
        
        // This should always fail
        let result = ErrorMessage::extract_result(boxed);
        assert!(result.is_err());
        
        if let Err(ActorError::MessageHandlingError(msg)) = result {
            assert_eq!(msg, "This always fails");
        } else {
            panic!("Expected MessageHandlingError");
        }
    }
    
    // Test unit message
    #[test]
    fn test_unit_message() {
        let boxed: BoxedMessage = Box::new(());
        
        // Extract unit result
        let result = UnitMessage::extract_result(boxed);
        assert!(result.is_ok());
    }
    
    // Test message envelope
    #[test]
    fn test_message_envelope() {
        let msg = TestRequest("Test".to_string());
        
        // Create envelope with default options
        let envelope = MessageEnvelope::new(
            msg, 
            None, 
            None
        );
        
        // Default options should have normal priority and no timeout
        assert_eq!(envelope.options.priority, MessagePriority::NORMAL);
        assert!(envelope.options.timeout.is_none());
        
        // Check message can be accessed
        let extracted = envelope.payload::<TestRequest>();
        assert!(extracted.is_some());
        assert_eq!(extracted.unwrap().0, "Test");
    }
    
    // Test message options
    #[test]
    fn test_message_options() {
        // Test default options
        let default_options = MessageOptions::default();
        assert_eq!(default_options.priority, MessagePriority::NORMAL);
        assert!(default_options.timeout.is_none());
        assert!(default_options.retry_policy.is_none());
        
        // Test custom options
        let custom_options = MessageOptions {
            priority: MessagePriority::HIGH,
            timeout: Some(Duration::from_secs(10)),
            retry_policy: None,
        };
        
        assert_eq!(custom_options.priority, MessagePriority::HIGH);
        assert_eq!(custom_options.timeout, Some(Duration::from_secs(10)));
    }
    
    // Test message priority constants
    #[test]
    fn test_message_priorities() {
        // Check priority ordering
        assert!(MessagePriority::new(BACKGROUND).unwrap().value() < MessagePriority::new(LOW).unwrap().value());
        assert!(MessagePriority::new(LOW).unwrap().value() < MessagePriority::new(NORMAL).unwrap().value());
        assert!(MessagePriority::new(NORMAL).unwrap().value() < MessagePriority::new(HIGH).unwrap().value());
        assert!(MessagePriority::new(HIGH).unwrap().value() < MessagePriority::new(CRITICAL).unwrap().value());
        
        // Check numeric values
        assert_eq!(BACKGROUND, 10);
        assert_eq!(LOW, 30);
        assert_eq!(NORMAL, 50);
        assert_eq!(HIGH, 70);
        assert_eq!(CRITICAL, 90);
    }
    
    // Test envelope with custom options
    #[test]
    fn test_envelope_with_custom_options() {
        let msg = TestRequest("Priority".to_string());
        
        // Create options
        let options = MessageOptions {
            priority: MessagePriority::HIGH,
            timeout: Some(Duration::from_secs(5)),
            retry_policy: Some(RetryPolicy {
                max_attempts: 3,
                retry_interval: Duration::from_secs(2),
                backoff_strategy: BackoffStrategy::Fixed,
            }),
        };
        
        // Create envelope with custom options
        let envelope = MessageEnvelope::new(
            msg,
            None,
            Some(options)
        );
        
        // Verify options were set
        assert_eq!(envelope.options.priority, MessagePriority::HIGH);
        assert_eq!(envelope.options.timeout, Some(Duration::from_secs(5)));
        assert!(envelope.options.retry_policy.is_some());
        
        let retry_policy = envelope.options.retry_policy.as_ref().unwrap();
        assert_eq!(retry_policy.max_attempts, 3);
        assert_eq!(retry_policy.retry_interval, Duration::from_secs(2));
        match retry_policy.backoff_strategy {
            BackoffStrategy::Fixed => {}, // Expected
            _ => panic!("Expected Fixed backoff strategy"),
        }
    }
    
    // Test message type mismatches
    #[test]
    fn test_message_type_mismatch() {
        // Create a message of wrong type
        let wrong_type: BoxedMessage = Box::new(42u32);
        
        // Try to extract as TestResponse
        let result = TestRequest::extract_result(wrong_type);
        assert!(result.is_err());
        
        if let Err(ActorError::MessageHandlingError(_)) = result {
            // Expected error
        } else {
            panic!("Expected MessageHandlingError");
        }
    }
    
    // Test envelope properties
    #[test]
    fn test_envelope_properties() {
        let msg = TestRequest("Envelope".to_string());
        let envelope = MessageEnvelope::new(
            msg,
            None,
            None
        );
        
        // Verify envelope properties
        assert!(envelope.id != Uuid::nil());
        assert!(envelope.message_type.contains("TestRequest"));
        assert!(envelope.sender.is_none());
    }
} 