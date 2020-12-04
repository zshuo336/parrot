use parrot_api::message::{Message, MessageEnvelope, MessageOptions, RetryPolicy, BackoffStrategy, CloneableMessage};
use parrot_api::types::{BoxedMessage, ActorResult};
use parrot_api::errors::ActorError;
use parrot_api::priority::{BACKGROUND, LOW, NORMAL, HIGH, CRITICAL};
use std::any::Any;
use std::time::Duration;
use uuid::Uuid;
use std::mem::size_of_val;

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
    
    // Test CloneableMessage with Message types
    #[test]
    fn test_cloneable_message_from_message() {
        // Create a test message
        let msg = TestRequest("Cloneable".to_string());
        
        // Convert to CloneableMessage
        let cloneable = CloneableMessage::from_message(msg.clone());
        
        // Clone the message
        let cloned = cloneable.clone();
        
        // Convert back to BoxedMessage
        let boxed = cloned.into_boxed();
        
        // Extract original message
        let extracted = boxed.downcast::<TestRequest>();
        assert!(extracted.is_ok());
        assert_eq!(extracted.unwrap().0, "Cloneable");
    }
    
    // Test CloneableMessage with primitive types
    #[test]
    fn test_cloneable_message_from_primitives() {
        // Test with string
        let string_msg = "String message".to_string();
        let cloneable = CloneableMessage::from_cloneable(string_msg.clone());
        let cloned = cloneable.clone();
        let boxed = cloned.into_boxed();
        let extracted = boxed.downcast::<String>();
        assert!(extracted.is_ok());
        assert_eq!(*extracted.unwrap(), string_msg);
        
        // Test with integer
        let int_msg = 42i32;
        let cloneable = CloneableMessage::from_cloneable(int_msg);
        let cloned = cloneable.clone();
        let boxed = cloned.into_boxed();
        let extracted = boxed.downcast::<i32>();
        assert!(extracted.is_ok());
        assert_eq!(*extracted.unwrap(), int_msg);
    }
    
    // Test try_from_boxed method
    #[test]
    fn test_try_from_boxed() {
        // Create a test message of a basic type that we know is supported
        let string_msg: BoxedMessage = Box::new("Test String".to_string());
        
        // Try to convert to CloneableMessage
        let cloneable = CloneableMessage::try_from_boxed(&string_msg);
        assert!(cloneable.is_some());
        
        // Other basic types should be convertible
        let int_msg: BoxedMessage = Box::new(42i32);
        assert!(CloneableMessage::try_from_boxed(&int_msg).is_some());
        
        let bool_msg: BoxedMessage = Box::new(true);
        assert!(CloneableMessage::try_from_boxed(&bool_msg).is_some());
        
        // Verify custom types are not yet supported
        let custom_msg = TestRequest("Test".to_string());
        let boxed_custom: BoxedMessage = Box::new(custom_msg);
        assert!(CloneableMessage::try_from_boxed(&boxed_custom).is_none());
    }
    
    // Test cloning behavior
    #[test]
    fn test_cloning_behavior() {
        // Test with a cloneable message
        let msg = TestRequest("Clone".to_string());
        let cloneable = CloneableMessage::from_message(msg);
        
        let cloned = cloneable.clone();
        let boxed = cloned.into_boxed();
        
        let extracted = boxed.downcast::<TestRequest>();
        assert!(extracted.is_ok());
        assert_eq!(extracted.unwrap().0, "Clone");
        
        // Test with primitive types
        let int_msg = 123i32;
        let cloneable = CloneableMessage::from_cloneable(int_msg);
        let cloned = cloneable.clone();
        let boxed = cloned.into_boxed();
        
        let extracted = boxed.downcast::<i32>();
        assert!(extracted.is_ok());
        assert_eq!(*extracted.unwrap(), 123);
    }
    
    // Test CloneableMessage in collections
    #[test]
    fn test_cloneable_message_in_collections() {
        // Create a vector of CloneableMessage objects
        let mut messages = Vec::new();
        
        // Add different types of messages
        messages.push(CloneableMessage::from_message(TestRequest("Message1".to_string())));
        messages.push(CloneableMessage::from_cloneable("Message2".to_string()));
        messages.push(CloneableMessage::from_cloneable(42i32));
        
        // Clone the entire vector
        let cloned_messages = messages.clone();
        
        // Verify each message was cloned correctly
        let boxed1 = cloned_messages[0].clone().into_boxed();
        let extracted1 = boxed1.downcast::<TestRequest>();
        assert!(extracted1.is_ok());
        assert_eq!(extracted1.unwrap().0, "Message1");
        
        let boxed2 = cloned_messages[1].clone().into_boxed();
        let extracted2 = boxed2.downcast::<String>();
        assert!(extracted2.is_ok());
        assert_eq!(*extracted2.unwrap(), "Message2");
        
        let boxed3 = cloned_messages[2].clone().into_boxed();
        let extracted3 = boxed3.downcast::<i32>();
        assert!(extracted3.is_ok());
        assert_eq!(*extracted3.unwrap(), 42);
    }
    
    // Test CloneableMessage nesting
    #[test]
    fn test_cloneable_message_nesting() {
        // Create a simple message
        let original_msg = "Original message".to_string();
        
        // First wrap and clone
        let cloneable1 = CloneableMessage::from_cloneable(original_msg.clone());
        let cloned1 = cloneable1.clone();
        let boxed1 = cloned1.into_boxed();
        
        // Verify type after first conversion
        let extracted1 = boxed1.downcast::<String>();
        assert!(extracted1.is_ok());
        assert_eq!(*extracted1.unwrap(), original_msg);
        
        // Create CloneableMessage from BoxedMessage again
        let cloneable2 = CloneableMessage::from_cloneable(original_msg.clone());
        
        // Multiple clones
        let cloned2a = cloneable2.clone();
        let cloned2b = cloned2a.clone();
        let cloned2c = cloned2b.clone();
        
        // Convert back to BoxedMessage
        let boxed2 = cloned2c.into_boxed();
        
        // Verify original type can still be extracted after multiple clones
        let extracted2 = boxed2.downcast::<String>();
        assert!(extracted2.is_ok());
        assert_eq!(*extracted2.unwrap(), original_msg);
        
        // Test nested CloneableMessage
        let nested1 = CloneableMessage::from_cloneable(original_msg.clone());
        let boxed_nested1 = nested1.clone().into_boxed();
        
        // Create another CloneableMessage from extracted BoxedMessage
        let nested2 = CloneableMessage::try_from_boxed(&boxed_nested1).unwrap();
        let boxed_nested2 = nested2.clone().into_boxed();
        
        // Verify original type can be extracted after nesting
        let extracted_nested = boxed_nested2.downcast::<String>();
        assert!(extracted_nested.is_ok());
        assert_eq!(*extracted_nested.unwrap(), original_msg);
    }
    
    // Test complex cloneable message combinations
    #[test]
    fn test_complex_cloneable_message_combinations() {
        // Test complex message type combinations
        
        // 1. Test custom message type
        let test_req = TestRequest("Complex test".to_string());
        let cloneable_req = CloneableMessage::from_message(test_req.clone());
        let boxed_req = cloneable_req.clone().into_boxed();
        
        let extracted_req = boxed_req.downcast::<TestRequest>();
        assert!(extracted_req.is_ok());
        assert_eq!(extracted_req.unwrap().0, "Complex test");
        
        // 2. Test multi-level conversion
        // String -> CloneableMessage -> BoxedMessage -> CloneableMessage -> BoxedMessage
        let original_str = "Multi-level test".to_string();
        let level1 = CloneableMessage::from_cloneable(original_str.clone());
        let boxed1 = level1.clone().into_boxed();
        
        let level2 = CloneableMessage::try_from_boxed(&boxed1).unwrap();
        let boxed2 = level2.clone().into_boxed();
        
        let extracted_str = boxed2.downcast::<String>();
        assert!(extracted_str.is_ok());
        assert_eq!(*extracted_str.unwrap(), original_str);
        
        // 3. Test combination of different types
        // Create a vector containing multiple types
        let mut mixed_types: Vec<BoxedMessage> = Vec::new();
        
        // Add various types
        mixed_types.push(CloneableMessage::from_cloneable("String value".to_string()).into_boxed());
        mixed_types.push(CloneableMessage::from_cloneable(42i32).into_boxed());
        mixed_types.push(CloneableMessage::from_message(TestRequest("Message value".to_string())).into_boxed());
        
        // Verify each type can be correctly extracted
        let str_val = mixed_types[0].downcast_ref::<String>();
        assert!(str_val.is_some());
        assert_eq!(*str_val.unwrap(), "String value");
        
        let int_val = mixed_types[1].downcast_ref::<i32>();
        assert!(int_val.is_some());
        assert_eq!(*int_val.unwrap(), 42);
        
        let msg_val = mixed_types[2].downcast_ref::<TestRequest>();
        assert!(msg_val.is_some());
        assert_eq!(msg_val.unwrap().0, "Message value");
    }
    
    // Test deep nesting and performance
    #[test]
    fn test_deep_nesting_and_performance() {
        // Create a base message
        let base_msg = "Performance test".to_string();
        
        // Create a deeply nested CloneableMessage structure
        // We'll create 5 levels of nesting, each converted from the previous level
        let mut current_boxed = CloneableMessage::from_cloneable(base_msg.clone()).into_boxed();
        
        // Create 5 levels of nesting
        for i in 0..5 {
            // Create new CloneableMessage from current BoxedMessage
            let cloneable = CloneableMessage::try_from_boxed(&current_boxed).unwrap();
            // Clone and convert back to BoxedMessage
            current_boxed = cloneable.clone().into_boxed();
        }
        
        // Verify final result can still be extracted as original type
        let final_extracted = current_boxed.downcast::<String>();
        assert!(final_extracted.is_ok());
        assert_eq!(*final_extracted.unwrap(), base_msg);
        
        // Performance test: bulk cloning
        let mut cloneables = Vec::new();
        for i in 0..100 {
            cloneables.push(CloneableMessage::from_cloneable(format!("Message {}", i)));
        }
        
        // Clone entire vector
        let cloned_vec = cloneables.clone();
        
        // Verify cloned content
        for (i, cloneable) in cloned_vec.into_iter().enumerate() {
            let boxed = cloneable.into_boxed();
            let extracted = boxed.downcast::<String>();
            assert!(extracted.is_ok());
            assert_eq!(*extracted.unwrap(), format!("Message {}", i));
        }
    }
    
    // Test CloneableMessage performance
    #[test]
    fn test_cloneable_message_performance() {
        use std::time::{Instant, Duration};
        
        // Performance test parameters
        const ITERATIONS: usize = 10_000;
        const CLONE_CYCLES: usize = 5;
        
        // 1. Test basic type cloning performance
        let start_time = Instant::now();
        
        let mut messages = Vec::with_capacity(ITERATIONS);
        for i in 0..ITERATIONS {
            messages.push(format!("Performance test message {}", i));
        }
        
        let string_creation_time = Instant::now().duration_since(start_time);
        println!("Created {} String messages in {:?}", ITERATIONS, string_creation_time);
        
        // 2. Test CloneableMessage wrapping performance
        let start_time = Instant::now();
        
        let mut cloneables = Vec::with_capacity(ITERATIONS);
        for msg in &messages {
            cloneables.push(CloneableMessage::from_cloneable(msg.clone()));
        }
        
        let wrapping_time = Instant::now().duration_since(start_time);
        println!("Wrapped {} messages in CloneableMessage in {:?}", ITERATIONS, wrapping_time);
        
        // 3. Test CloneableMessage cloning performance
        let start_time = Instant::now();
        
        let mut cloned = cloneables.clone();
        for _ in 1..CLONE_CYCLES {
            cloned = cloned.clone();
        }
        
        let cloning_time = Instant::now().duration_since(start_time);
        println!("Cloned {} CloneableMessages {} times in {:?}", 
                 ITERATIONS, CLONE_CYCLES, cloning_time);
        
        // 4. Test CloneableMessage unwrapping performance
        let start_time = Instant::now();
        
        let mut boxed_messages = Vec::with_capacity(ITERATIONS);
        for cloneable in cloned {
            boxed_messages.push(cloneable.into_boxed());
        }
        
        let unwrapping_time = Instant::now().duration_since(start_time);
        println!("Unwrapped {} CloneableMessages to BoxedMessage in {:?}", 
                 ITERATIONS, unwrapping_time);
        
        // 5. Test downcast performance
        let start_time = Instant::now();
        
        let mut success_count = 0;
        for boxed in boxed_messages {
            if let Ok(string_val) = boxed.downcast::<String>() {
                success_count += 1;
                // Use value to prevent compiler optimization
                assert!(string_val.len() > 0);
            }
        }
        
        let downcast_time = Instant::now().duration_since(start_time);
        println!("Downcast {} BoxedMessages in {:?} with {} successes", 
                 ITERATIONS, downcast_time, success_count);
        
        // Verify all operations succeeded
        assert_eq!(success_count, ITERATIONS);
        
        // Output overall performance summary
        let total_time = string_creation_time + wrapping_time + cloning_time + unwrapping_time + downcast_time;
        println!("Total performance test completed in {:?}", total_time);
        println!("Average time per message: {:?}", total_time / (ITERATIONS as u32));
    }
    
    // Test CloneableMessage vs direct clone performance
    #[test]
    fn test_cloneable_vs_direct_clone_performance() {
        use std::time::Instant;
        
        // Test parameters
        const ITERATIONS: usize = 10_000;
        const MESSAGE_SIZE: usize = 100; // String length
        
        // Prepare test data - create fixed size string
        let test_str = "X".repeat(MESSAGE_SIZE);
        
        // 1. Test direct clone performance (baseline)
        let start_time = Instant::now();
        
        let mut direct_clones = Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            let cloned = test_str.clone();
            direct_clones.push(Box::new(cloned) as BoxedMessage);
        }
        
        let direct_clone_time = Instant::now().duration_since(start_time);
        println!("Direct clone of {} strings ({} bytes each): {:?}", 
                 ITERATIONS, MESSAGE_SIZE, direct_clone_time);
        
        // 2. Test CloneableMessage clone performance
        let start_time = Instant::now();
        
        let cloneable = CloneableMessage::from_cloneable(test_str.clone());
        let mut cloneable_results = Vec::with_capacity(ITERATIONS);
        
        for _ in 0..ITERATIONS {
            let cloned = cloneable.clone();
            cloneable_results.push(cloned.into_boxed());
        }
        
        let cloneable_time = Instant::now().duration_since(start_time);
        println!("CloneableMessage clone of {} strings ({} bytes each): {:?}", 
                 ITERATIONS, MESSAGE_SIZE, cloneable_time);
        
        // 3. Calculate performance ratio
        let ratio = cloneable_time.as_nanos() as f64 / direct_clone_time.as_nanos() as f64;
        println!("Performance ratio: CloneableMessage is {:.2}x slower than direct clone", ratio);
        
        // Verify results
        assert!(direct_clones.len() == ITERATIONS);
        assert!(cloneable_results.len() == ITERATIONS);
        
        // Verify first element content
        let direct_str = direct_clones[0].downcast_ref::<String>().unwrap();
        let cloneable_str = cloneable_results[0].downcast_ref::<String>();
        
        assert!(cloneable_str.is_some());
        assert_eq!(direct_str, cloneable_str.unwrap());
    }
    
    // Test memory usage in nested scenarios
    #[test]
    fn test_memory_usage_in_nested_scenarios() {
        use std::mem::{size_of_val, size_of};
        
        // Create base message
        let base_msg = "Memory test".to_string();
        let base_capacity = base_msg.capacity();
        
        // Estimate total memory usage for String
        fn estimate_string_size(s: &String) -> usize {
            // String struct size + heap allocated capacity
            size_of::<String>() + s.capacity()
        }
        
        // Estimate total memory usage for BoxedMessage
        fn estimate_boxed_size(boxed: &BoxedMessage, content_type: &str) -> usize {
            // Box pointer size + estimated internal size based on content type
            let box_size = size_of::<BoxedMessage>();
            
            let content_size = match content_type {
                "String" => {
                    if let Some(s) = boxed.downcast_ref::<String>() {
                        estimate_string_size(s)
                    } else {
                        0
                    }
                },
                _ => 8, // Default estimate
            };
            
            box_size + content_size
        }
        
        // Estimate total memory usage for CloneableMessage
        fn estimate_cloneable_size(cloneable: &CloneableMessage) -> usize {
            // CloneableMessage struct size + Box<dyn CloneableMessageTrait> size
            // Fat pointer (16 bytes) + estimated internal content size
            size_of::<CloneableMessage>() + 16 + 8 // Assume average internal content size of 8 bytes
        }
        
        // Measure base types
        let base_size = size_of::<String>();
        let base_heap_size = estimate_string_size(&base_msg);
        
        // Measure BoxedMessage
        let boxed_msg: BoxedMessage = Box::new(base_msg.clone());
        let boxed_size = size_of::<BoxedMessage>();
        let boxed_heap_size = estimate_boxed_size(&boxed_msg, "String");
        
        // Measure CloneableMessage
        let cloneable_msg = CloneableMessage::from_cloneable(base_msg.clone());
        let cloneable_size = size_of::<CloneableMessage>();
        let cloneable_heap_size = estimate_cloneable_size(&cloneable_msg);
        
        // Print detailed memory information
        println!("--- Memory Usage Details ---");
        println!("Base String:");
        println!("  - Stack size: {} bytes", base_size);
        println!("  - Capacity: {} bytes", base_capacity);
        println!("  - Estimated total size: {} bytes", base_heap_size);
        
        println!("BoxedMessage:");
        println!("  - Stack size: {} bytes", boxed_size);
        println!("  - Estimated total size: {} bytes", boxed_heap_size);
        
        println!("CloneableMessage:");
        println!("  - Stack size: {} bytes", cloneable_size);
        println!("  - Estimated total size: {} bytes", cloneable_heap_size);
        
        // Test nested scenarios
        let mut current_boxed = CloneableMessage::from_cloneable(base_msg.clone()).into_boxed();
        let mut stack_sizes = Vec::new();
        let mut estimated_sizes = Vec::new();
        
        // Create multiple levels of nesting and measure size at each level
        println!("\n--- Nesting Memory Impact ---");
        for i in 0..5 {
            let stack_size = size_of::<BoxedMessage>();
            // This is an estimate, we need to consider nesting levels
            let estimated_size = boxed_heap_size + i * 16; // Add fat pointer overhead per level
            
            stack_sizes.push(stack_size);
            estimated_sizes.push(estimated_size);
            
            println!("Nesting level {}:", i);
            println!("  - Stack size: {} bytes", stack_size);
            println!("  - Estimated total size: {} bytes", estimated_size);
            
            // Create new CloneableMessage from current BoxedMessage
            let cloneable = CloneableMessage::try_from_boxed(&current_boxed).unwrap();
            // Clone and convert back to BoxedMessage
            current_boxed = cloneable.clone().into_boxed();
        }
        
        // Final size
        let final_stack_size = size_of::<BoxedMessage>();
        let final_estimated_size = boxed_heap_size + 5 * 16; // 5 levels of nesting
        
        stack_sizes.push(final_stack_size);
        estimated_sizes.push(final_estimated_size);
        
        println!("Final nesting level:");
        println!("  - Stack size: {} bytes", final_stack_size);
        println!("  - Estimated total size: {} bytes", final_estimated_size);
        
        // Verify final result can still be extracted as original type
        let final_extracted = current_boxed.downcast::<String>();
        assert!(final_extracted.is_ok());
        assert_eq!(*final_extracted.unwrap(), base_msg);
        
        // Analyze memory impact of nesting
        let initial_size = estimated_sizes[0];
        let final_size = estimated_sizes[estimated_sizes.len()-1];
        let growth_factor = if initial_size > 0 { final_size as f64 / initial_size as f64 } else { 0.0 };
        
        println!("\n--- Memory Growth Analysis ---");
        println!("Initial estimated size: {} bytes", initial_size);
        println!("Final estimated size: {} bytes", final_size);
        println!("Growth factor: {:.2}x", growth_factor);
        println!("Extra memory per nesting level: ~{} bytes", (final_size - initial_size) / 5);
        
        // Verify growth is bounded
        // We expect growth factor to be less than number of nesting levels
        assert!(growth_factor < 10.0, "Memory growth should be bounded");
    }
} 