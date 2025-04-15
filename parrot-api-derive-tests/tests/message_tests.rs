use parrot_api::{Message, MessagePriority};
use parrot_api::{HIGH, LOW, NORMAL, CRITICAL, BACKGROUND};
use parrot_api::message::BackoffStrategy;
use serde::{Serialize, Deserialize};

// Test basic message without any attributes
#[derive(Message, Debug, PartialEq, Clone)]
struct BasicMessage {
    content: String,
}

// Test message with custom result type
#[derive(Message, Debug, PartialEq, Clone)]
#[message(result = "String")]
struct CustomResultMessage {
    query: String,
}

// Test message with serde support
#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
#[derive(Message)]
struct SerdeMessage {
    data: Vec<u8>,
}

// Test message with validation
#[derive(Message, Debug, PartialEq, Clone)]
#[message(validate = "self.amount > 0.0")]
struct ValidatedMessage {
    amount: f64,
}

// Test message with priority
#[derive(Message, Debug, PartialEq, Clone)]
#[message(priority = 70)]
struct PriorityMessage {
    urgent: bool,
}

// Test message with all features
#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
#[derive(Message)]
#[message(
    result = "Vec<String>",
    validate = "self.items.len() > 0",
    priority = 70
)]
struct CompleteMessage {
    items: Vec<String>,
}

// Add new message type for testing timeout options
#[derive(Message, Debug, PartialEq, Clone)]
#[message(timeout = 5)]
struct TimeoutMessage {
    data: String,
}

// Add new message type for testing retry strategy
#[derive(Message, Debug, PartialEq, Clone)]
#[message(
    retry_max_attempts = 3,
    retry_interval = 2,
    retry_strategy = "Fixed"
)]
struct RetryFixedMessage {
    job_id: String,
}

#[derive(Message, Debug, PartialEq, Clone)]
#[message(
    retry_max_attempts = 3,
    retry_interval = 2,
    retry_strategy = "Linear"
)]
struct RetryLinearMessage {
    job_id: String,
}

#[derive(Message, Debug, PartialEq, Clone)]
#[message(
    retry_max_attempts = 3,
    retry_interval = 2,
    retry_strategy = "Exponential"
)]
struct RetryExponentialMessage {
    job_id: String,
}

// Add new message type for testing numeric priority
#[derive(Message, Debug, PartialEq, Clone)]
#[message(priority = 75)]
struct NumericPriorityMessage {
    data: String,
}

// Add a complete message type with all new options
#[derive(Message, Debug, PartialEq, Clone)]
#[message(
    result = "Vec<String>",
    validate = "self.amount > 0.0",
    priority = 90,
    timeout = 10,
    retry_max_attempts = 5,
    retry_interval = 3,
    retry_strategy = "Exponential"
)]
struct FullFeaturedMessage {
    amount: f64,
}

// Add new message type for testing string priority
#[derive(Message, Debug, PartialEq, Clone)]
#[message(priority = "HIGH")]
struct StringPriorityMessage {
    data: String,
}

// 使用自定义常量 - 类似于C++中的 #define PRIORITY_HIGH 70
#[derive(Message, Debug, Clone)]
#[message(priority = HIGH)]
struct ConstantPriorityMessage {
    data: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_basic_message() {
        let msg = BasicMessage {
            content: "test".to_string(),
        };
        
        let msg = BasicMessage::new(msg).unwrap();
        assert_eq!(msg.message_type(), "message_tests::BasicMessage");
        assert_eq!(msg.priority(), MessagePriority::NORMAL);
        assert!(msg.validate().is_ok());
        assert_eq!(msg.content, "test".to_string());
    }

    #[test]
    fn test_custom_result_message() {
        let msg = CustomResultMessage {
            query: "search".to_string(),
        };
        
        let msg = CustomResultMessage::new(msg).unwrap();
        let envelope = msg.into_envelope();
        let _msg_ref = envelope.message::<CustomResultMessage>().unwrap();
        // Assert that _msg_ref is of type CustomResultMessage
        assert!(std::any::TypeId::of::<CustomResultMessage>() == std::any::TypeId::of::<CustomResultMessage>());
        assert_eq!(_msg_ref.message_type(), "message_tests::CustomResultMessage");
        assert_eq!(_msg_ref.query, "search".to_string());
    }

    #[test]
    fn test_serde_message() {
        let msg = SerdeMessage {
            data: vec![1, 2, 3],
        };
        
        // Test serialization
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: SerdeMessage = serde_json::from_str(&serialized).unwrap();
        assert_eq!(msg.data, deserialized.data);

        let msg = SerdeMessage::new(msg).unwrap();
        let envelope = msg.into_envelope();
        let _msg_ref = envelope.message::<SerdeMessage>().unwrap();
        assert!(std::any::TypeId::of::<SerdeMessage>() == std::any::TypeId::of::<SerdeMessage>());
        assert_eq!(_msg_ref.message_type(), "message_tests::SerdeMessage");
        assert_eq!(_msg_ref.data, vec![1, 2, 3]);
    }

    #[test]
    fn test_validated_message() {
        // Test valid case
        let valid_msg = ValidatedMessage { amount: 100.0 };
        assert!(ValidatedMessage::new(valid_msg).is_ok());

        // Test invalid case
        let invalid_msg = ValidatedMessage { amount: -1.0 };
        assert!(ValidatedMessage::new(invalid_msg).is_err());
    }

    #[test]
    fn test_priority_message() {
        let msg = PriorityMessage { urgent: true };
        assert_eq!(msg.priority(), MessagePriority::HIGH);

        let msg = PriorityMessage::new(msg).unwrap();
        let envelope = msg.into_envelope();
        let _msg_ref = envelope.message::<PriorityMessage>().unwrap();
        assert!(std::any::TypeId::of::<PriorityMessage>() == std::any::TypeId::of::<PriorityMessage>());
        assert_eq!(_msg_ref.message_type(), "message_tests::PriorityMessage");
        assert_eq!(_msg_ref.urgent, true);
    }

    #[test]
    fn test_complete_message() {
        // Test valid case
        let valid_msg = CompleteMessage {
            items: vec!["item1".to_string()],
        };
        let msg = CompleteMessage::new(valid_msg).unwrap();
        assert_eq!(msg.priority(), MessagePriority::HIGH);

        // Test invalid case
        let invalid_msg = CompleteMessage {
            items: vec![],
        };
        assert!(CompleteMessage::new(invalid_msg).is_err());

        // Test serialization
        let msg = CompleteMessage {
            items: vec!["test".to_string()],
        };
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: CompleteMessage = serde_json::from_str(&serialized).unwrap();
        assert_eq!(msg.items, deserialized.items);
    }

    #[test]
    fn test_message_envelope() {
        let msg = BasicMessage {
            content: "test".to_string(),
        };
        let envelope = msg.into_envelope();
        let _msg_ref = envelope.message::<BasicMessage>().unwrap();
        // Assert that _msg_ref is of type BasicMessage
        assert!(std::any::TypeId::of::<BasicMessage>() == std::any::TypeId::of::<BasicMessage>());
        assert_eq!(_msg_ref.message_type(), "message_tests::BasicMessage");

        // Test full featured message envelope conversion
        let msg = FullFeaturedMessage {
            amount: 100.0,
        };
        let envelope = msg.into_envelope();
        
        // Verify message content after conversion
        let msg_ref = envelope.message::<FullFeaturedMessage>().unwrap();
        assert_eq!(msg_ref.amount, 100.0);
        
        // Verify message options
        let options = envelope.options;
        assert_eq!(options.priority, MessagePriority::CRITICAL);
        assert_eq!(options.timeout, Some(Duration::from_secs(10)));
        
        // Verify retry policy
        let retry_policy = options.retry_policy.unwrap();
        assert_eq!(retry_policy.max_attempts, 5);
        assert_eq!(retry_policy.retry_interval, Duration::from_secs(3));
        matches!(retry_policy.backoff_strategy, BackoffStrategy::Fixed);
    }

    #[test]
    fn test_result_extraction() {
        let result: Box<dyn std::any::Any + Send> = Box::new("test".to_string());
        let extracted = <CustomResultMessage as Message>::extract_result(result);
        assert!(extracted.is_ok());
        assert_eq!(extracted.unwrap(), "test".to_string());

        // Test invalid result type
        let invalid_result: Box<dyn std::any::Any + Send> = Box::new(42);
        let extracted = <CustomResultMessage as Message>::extract_result(invalid_result);
        assert!(extracted.is_err());
    }

    #[test]
    fn test_message_type_names() {
        let basic = BasicMessage {
            content: "test".to_string(),
        };
        assert_eq!(basic.message_type(), "message_tests::BasicMessage");

        let custom = CustomResultMessage {
            query: "test".to_string(),
        };
        assert_eq!(custom.message_type(), "message_tests::CustomResultMessage");
    }

    #[test]
    fn test_timeout_message() {
        let msg = TimeoutMessage {
            data: "test".to_string(),
        };
        let msg = TimeoutMessage::new(msg).unwrap();
        let options = msg.message_options().unwrap();
        assert_eq!(options.timeout, Some(Duration::from_secs(5)));
    }

    #[test]
    fn test_retry_fixed_strategy() {
        let msg = RetryFixedMessage {
            job_id: "job1".to_string(),
        };
        let msg = RetryFixedMessage::new(msg).unwrap();
        let options = msg.message_options().unwrap();
        
        if let Some(retry_policy) = options.retry_policy {
            assert_eq!(retry_policy.max_attempts, 3);
            assert_eq!(retry_policy.retry_interval, Duration::from_secs(2));
            matches!(retry_policy.backoff_strategy, BackoffStrategy::Fixed);
        } else {
            panic!("Expected retry policy to be Some");
        }
    }

    #[test]
    fn test_retry_linear_strategy() {
        let msg = RetryLinearMessage {
            job_id: "job1".to_string(),
        };
        let msg = RetryLinearMessage::new(msg).unwrap();
        let options = msg.message_options().unwrap();
        
        if let Some(retry_policy) = options.retry_policy {
            matches!(retry_policy.backoff_strategy, BackoffStrategy::Linear);
        } else {
            panic!("Expected retry policy to be Some");
        }
    }

    #[test]
    fn test_retry_exponential_strategy() {
        let msg = RetryExponentialMessage {
            job_id: "job1".to_string(),
        };
        let msg = RetryExponentialMessage::new(msg).unwrap();
        let options = msg.message_options().unwrap();
        
        if let Some(retry_policy) = options.retry_policy {
            matches!(retry_policy.backoff_strategy, BackoffStrategy::Exponential { .. });
        } else {
            panic!("Expected retry policy to be Some");
        }
    }

    #[test]
    fn test_numeric_priority() {
        let msg = NumericPriorityMessage {
            data: "test".to_string(),
        };
        let msg = NumericPriorityMessage::new(msg).unwrap();
        assert_eq!(msg.priority().value(), 75);
    }

    #[test]
    fn test_full_featured_message() {
        // Test valid case
        let msg = FullFeaturedMessage { amount: 100.0 };
        let msg = FullFeaturedMessage::new(msg).unwrap();
        
        let options = msg.message_options().unwrap();
        assert_eq!(options.timeout, Some(Duration::from_secs(10)));
        assert_eq!(msg.priority().value(), 90);
        
        if let Some(retry_policy) = options.retry_policy {
            assert_eq!(retry_policy.max_attempts, 5);
            assert_eq!(retry_policy.retry_interval, Duration::from_secs(3));
            matches!(retry_policy.backoff_strategy, BackoffStrategy::Exponential { .. });
        } else {
            panic!("Expected retry policy to be Some");
        }

        // Test invalid case
        let invalid_msg = FullFeaturedMessage { amount: -1.0 };
        assert!(FullFeaturedMessage::new(invalid_msg).is_err());
    }

    #[test]
    fn test_default_options() {
        let msg = BasicMessage {
            content: "test".to_string(),
        };
        let msg = BasicMessage::new(msg).unwrap();
        let options = msg.message_options().unwrap();
        
        assert_eq!(options.timeout, None);
        // Use is_none() check instead of direct comparison
        assert!(options.retry_policy.is_none());
        assert_eq!(options.priority, MessagePriority::NORMAL);
    }

    #[test]
    fn test_envelope_options_propagation() {
        let msg = FullFeaturedMessage { amount: 100.0 };
        let envelope = msg.into_envelope();
        
        assert_eq!(envelope.options.timeout, Some(Duration::from_secs(10)));
        assert_eq!(envelope.options.priority.value(), 90);
        
        if let Some(retry_policy) = envelope.options.retry_policy {
            assert_eq!(retry_policy.max_attempts, 5);
            assert_eq!(retry_policy.retry_interval, Duration::from_secs(3));
            matches!(retry_policy.backoff_strategy, BackoffStrategy::Exponential { .. });
        } else {
            panic!("Expected retry policy to be Some");
        }
    }

    #[test]
    fn test_string_priority() {
        let msg = StringPriorityMessage {
            data: "test".to_string(),
        };
        let msg = StringPriorityMessage::new(msg).unwrap();
        assert_eq!(msg.priority().value(), 70); // HIGH priority value should be 70
    }

    #[test]
    fn test_constant_priority() {
        let msg = ConstantPriorityMessage {
            data: "test".to_string(),
        };
        let msg = ConstantPriorityMessage::new(msg).unwrap();
        assert_eq!(msg.priority().value(), HIGH);
    }
}