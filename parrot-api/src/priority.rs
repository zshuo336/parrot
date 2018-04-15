//! Priority constants for use in message attributes
//!
//! This module provides constants that can be used when specifying 
//! message priorities in the Message derive macro.
//!
//! # Usage
//!
//! ```rust
//! use parrot_api::Message;
//! use parrot_api::priority::{HIGH, LOW};
//!
//! #[derive(Message)]
//! #[message(priority = 70)] // use HIGH
//! struct ImportantMessage {
//!     content: String,
//! }
//!
//! #[derive(Message)]
//! #[message(priority = 30)] // use LOW
//! struct LowPriorityMessage {
//!     content: String,
//! }
//! ```

/// Background priority (10)
pub const BACKGROUND: u8 = 10;

/// Low priority (30)
pub const LOW: u8 = 30;

/// Normal priority (50)
pub const NORMAL: u8 = 50;

/// High priority (70)
pub const HIGH: u8 = 70;

/// Critical priority (90)
pub const CRITICAL: u8 = 90;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MessagePriority;

    #[test]
    fn test_priority_constants() {
        assert_eq!(MessagePriority::BACKGROUND.value(), BACKGROUND);
        assert_eq!(MessagePriority::LOW.value(), LOW);
        assert_eq!(MessagePriority::NORMAL.value(), NORMAL);
        assert_eq!(MessagePriority::HIGH.value(), HIGH);
        assert_eq!(MessagePriority::CRITICAL.value(), CRITICAL);
    }
} 