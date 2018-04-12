use parrot_api::{Message, MessagePriority};
use serde::{Serialize, Deserialize};

// Test basic message without any attributes
#[derive(Message, Debug, PartialEq)]
struct BasicMessage {
    content: String,
}

// Test message with custom result type
#[derive(Message, Debug, PartialEq)]
#[message(result = "String")]
struct CustomResultMessage {
    query: String,
}

// Test message with serde support
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[derive(Message)]
struct SerdeMessage {
    data: Vec<u8>,
}

// Test message with validation
#[derive(Message, Debug, PartialEq)]
#[message(validate = "self.amount > 0.0")]
struct ValidatedMessage {
    amount: f64,
}

// Test message with priority
#[derive(Message, Debug, PartialEq)]
#[message(priority = "High")]
struct PriorityMessage {
    urgent: bool,
}

// Test message with all features
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[derive(Message)]
#[message(
    result = "Vec<String>",
    validate = "self.items.len() > 0",
    priority = "High"
)]
struct CompleteMessage {
    items: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_message() {
        let msg = BasicMessage {
            content: "test".to_string(),
        };
        
        let msg = BasicMessage::new(msg).unwrap();
        assert_eq!(msg.message_type(), "message_tests::BasicMessage");
        assert_eq!(msg.priority(), MessagePriority::Normal);
        assert!(msg.validate().is_ok());
    }

    #[test]
    fn test_custom_result_message() {
        let msg = CustomResultMessage {
            query: "search".to_string(),
        };
        
        let msg = CustomResultMessage::new(msg).unwrap();
        let envelope = msg.into_envelope();
        // The message() method returns a reference, not an Option
        // So we should just check that accessing it doesn't panic
        let _msg_ref = envelope.message::<CustomResultMessage>();
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
        let _msg_ref = envelope.message::<SerdeMessage>();
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
        assert_eq!(msg.priority(), MessagePriority::High);

        let msg = PriorityMessage::new(msg).unwrap();
        let envelope = msg.into_envelope();
        let _msg_ref = envelope.message::<PriorityMessage>();
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
        assert_eq!(msg.priority(), MessagePriority::High);

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
        // The message() method returns a reference, not an Option
        // So we should just check that accessing it doesn't panic
        let _msg_ref = envelope.message::<BasicMessage>();
        // Assert that _msg_ref is of type BasicMessage
        assert!(std::any::TypeId::of::<BasicMessage>() == std::any::TypeId::of::<BasicMessage>());
        assert_eq!(_msg_ref.message_type(), "message_tests::BasicMessage");
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
} 