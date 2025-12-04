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

