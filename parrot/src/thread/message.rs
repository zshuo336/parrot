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



/// Helper function to create a BoxedMessage that can be safely cloned
pub fn make_cloneable<T: 'static + Clone + Send>(value: T) -> BoxedMessage {
    Box::new(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clone_box_clones_cloneable_payload() {
        let msg = make_cloneable(String::from("hello"));
        let cloned = msg.downcast_ref::<String>().unwrap().clone_box();
        let inner = cloned.downcast::<String>().expect("cloned is String");
        assert_eq!(*inner, "hello");
    }

    #[test]
    fn test_clone_box_works_for_custom_types() {
        #[derive(Clone, Debug, PartialEq)]
        struct Payload {
            id: u64,
            items: Vec<u32>,
        }

        let payload = Payload {
            id: 7,
            items: vec![1, 2, 3],
        };
        let msg = make_cloneable(payload);
        let cloned = msg.downcast_ref::<Payload>().unwrap().clone_box();
        let inner = cloned.downcast::<Payload>().expect("cloned is Payload");
        assert_eq!(*inner, Payload {
            id: 7,
            items: vec![1, 2, 3],
        });
    }

    #[test]
    fn test_non_cloneable_dispatch() {
        // A non-Clone payload simply has no CloneableMessage impl; using
        // make_cloneable requires Clone at compile time, so this test checks
        // that plain boxed messages of Clone types still downcast correctly.
        let msg: BoxedMessage = Box::new(42u64);
        assert_eq!(*msg.downcast_ref::<u64>().unwrap(), 42u64);
    }
}

