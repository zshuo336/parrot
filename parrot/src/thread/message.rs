// Thread Actor Message Cloning Mechanism
//
// This module provides functionality for cloning BoxedMessage objects, which by default
// cannot be cloned directly due to the boxed trait object. We implement a zero-cost abstraction
// that allows proper cloning of messages.

use std::any::Any;
use parrot_api::types::BoxedMessage;

/// CloneableMessage is a trait that extends Any and Send with the ability to clone itself.
/// This allows us to implement cloning for BoxedMessage types that contain cloneable data.
pub trait CloneableMessage: Any + Send {
    /// Clone the message and return it as a BoxedMessage
    fn clone_box(&self) -> BoxedMessage;
}

/// Implementation of CloneableMessage for all T that implement Clone
impl<T: 'static + Clone + Send> CloneableMessage for T {
    fn clone_box(&self) -> BoxedMessage {
        Box::new(self.clone())
    }
}

/// Extension trait for BoxedMessage to add cloning capability
pub trait BoxedMessageExt {
    /// Attempts to clone the BoxedMessage
    /// 
    /// # Returns
    /// - `Some(BoxedMessage)` if the message can be cloned
    /// - `None` if the message doesn't support cloning
    fn try_clone(&self) -> Option<BoxedMessage>;
    
    /// Clones the BoxedMessage, or panics if it cannot be cloned
    /// 
    /// # Panics
    /// Panics if the contained message doesn't implement Clone
    fn clone_or_panic(&self) -> BoxedMessage;
    
    /// Clones the BoxedMessage, or returns a default value if it cannot be cloned
    /// 
    /// # Parameters
    /// * `default_factory` - A function that creates a default message to use when cloning fails
    fn clone_or_default<F>(&self, default_factory: F) -> BoxedMessage 
    where
        F: FnOnce() -> BoxedMessage;
}

impl BoxedMessageExt for BoxedMessage {
    fn try_clone(&self) -> Option<BoxedMessage> {
        // Try to downcast to CloneableMessage
        self.as_ref()
            .as_any()
            .downcast_ref::<Box<dyn CloneableMessage>>()
            .map(|cloneable| cloneable.clone_box())
            .or_else(|| {
                // If that doesn't work, try to downcast to a known cloneable type
                // and then box it up with the CloneableMessage trait
                if let Some(s) = self.as_ref().downcast_ref::<String>() {
                    Some(Box::new(s.clone()))
                } else if let Some(i) = self.as_ref().downcast_ref::<i32>() {
                    Some(Box::new(*i))
                } else if let Some(u) = self.as_ref().downcast_ref::<u32>() {
                    Some(Box::new(*u))
                } else if let Some(b) = self.as_ref().downcast_ref::<bool>() {
                    Some(Box::new(*b))
                } else if let Some(f) = self.as_ref().downcast_ref::<f64>() {
                    Some(Box::new(*f))
                } else if let Some(c) = self.as_ref().downcast_ref::<char>() {
                    Some(Box::new(*c))
                } else if let Some(v) = self.as_ref().downcast_ref::<Vec<u8>>() {
                    Some(Box::new(v.clone()))
                } else if let Some(v) = self.as_ref().downcast_ref::<()>() {
                    Some(Box::new(()))
                } else {
                    // Try other common types as needed...
                    None
                }
            })
    }

    fn clone_or_panic(&self) -> BoxedMessage {
        self.try_clone().expect("Failed to clone BoxedMessage: message is not cloneable")
    }

    fn clone_or_default<F>(&self, default_factory: F) -> BoxedMessage 
    where
        F: FnOnce() -> BoxedMessage
    {
        self.try_clone().unwrap_or_else(default_factory)
    }
}

/// Helper function to create a BoxedMessage that can be safely cloned
pub fn make_cloneable<T: 'static + Clone + Send>(value: T) -> BoxedMessage {
    Box::new(value)
}

/// Helper function to convert any BoxedMessage into a CloneableWrapper if possible
pub fn wrap_as_cloneable(message: &BoxedMessage) -> Option<BoxedMessage> {
    message.try_clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[derive(Clone, Debug)]
    struct TestMessage {
        value: String,
        counter: i32,
    }
    
    #[test]
    fn test_cloneable_primitive_types() {
        // Test with primitive types
        let int_msg: BoxedMessage = Box::new(42);
        let string_msg: BoxedMessage = Box::new("hello".to_string());
        let empty_msg: BoxedMessage = Box::new(());
        
        assert!(int_msg.try_clone().is_some());
        assert!(string_msg.try_clone().is_some());
        assert!(empty_msg.try_clone().is_some());
        
        if let Some(cloned) = int_msg.try_clone() {
            assert_eq!(cloned.downcast_ref::<i32>().unwrap(), &42);
        }
        
        if let Some(cloned) = string_msg.try_clone() {
            assert_eq!(cloned.downcast_ref::<String>().unwrap(), "hello");
        }
    }
    
    #[test]
    fn test_cloneable_custom_types() {
        // Test with custom type
        let test_msg = TestMessage {
            value: "test".to_string(),
            counter: 123,
        };
        
        let boxed: BoxedMessage = Box::new(test_msg);
        
        if let Some(cloned) = boxed.try_clone() {
            let unpacked = cloned.downcast_ref::<TestMessage>().unwrap();
            assert_eq!(unpacked.value, "test");
            assert_eq!(unpacked.counter, 123);
        } else {
            panic!("Failed to clone custom message type");
        }
    }
    
    #[test]
    fn test_clone_or_default() {
        // Test with non-cloneable type (assuming we don't have a special case for it)
        struct NonCloneable(i32);
        
        let non_cloneable: BoxedMessage = Box::new(NonCloneable(42));
        let default_msg = || Box::new("default".to_string()) as BoxedMessage;
        
        let result = non_cloneable.clone_or_default(default_msg);
        assert_eq!(result.downcast_ref::<String>().unwrap(), "default");
    }
    
    #[test]
    #[should_panic(expected = "Failed to clone BoxedMessage")]
    fn test_clone_or_panic() {
        // This should panic because we don't have a clone implementation for NonCloneable
        struct NonCloneable(i32);
        
        let non_cloneable: BoxedMessage = Box::new(NonCloneable(42));
        let _ = non_cloneable.clone_or_panic();
    }
    
    #[test]
    fn test_make_cloneable() {
        let msg = make_cloneable(TestMessage {
            value: "cloneable".to_string(),
            counter: 999,
        });
        
        let cloned = msg.try_clone().unwrap();
        let unwrapped = cloned.downcast_ref::<TestMessage>().unwrap();
        assert_eq!(unwrapped.value, "cloneable");
        assert_eq!(unwrapped.counter, 999);
    }
} 