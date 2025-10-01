//! Example: cloning type-erased messages.
//!
//! `Box<dyn Any + Send>` cannot be cloned generically. The thread engine
//! provides `CloneableMessage` (implemented for all `Clone + Send` types)
//! and `make_cloneable` to build cloneable boxed payloads. On the API side,
//! `parrot_api::message::CloneableMessage::try_from_boxed` performs the same
//! job for arbitrary boxed messages.

use parrot::thread::message::CloneableMessage as ThreadCloneable;
use parrot::thread::message::make_cloneable;
use parrot_api::message::CloneableMessage as ApiCloneable;
use parrot_api::types::BoxedMessage;

#[derive(Clone, Debug)]
struct CustomMessage {
    id: u32,
    payload: String,
    timestamp: u64,
}

fn main() {
    // Create messages of various types.
    let primitive_message: BoxedMessage = Box::new(42i32);
    let string_message: BoxedMessage = Box::new("Hello, Parrot!".to_string());

    let custom_message = CustomMessage {
        id: 1001,
        payload: "Important data".to_string(),
        timestamp: 1622548800,
    };
    let complex_message: BoxedMessage = Box::new(custom_message);

    println!("Clone messages via parrot_api::CloneableMessage::try_from_boxed:");

    // Clone a primitive message
    if let Some(cloned) = ApiCloneable::try_from_boxed(&primitive_message) {
        let cloned = cloned.into_boxed();
        if let Some(value) = cloned.downcast_ref::<i32>() {
            println!("Cloned primitive message: {}", value);
        }
    } else {
        println!("Cannot clone primitive message");
    }

    // Clone a string message
    if let Some(cloned) = ApiCloneable::try_from_boxed(&string_message) {
        let cloned = cloned.into_boxed();
        if let Some(value) = cloned.downcast_ref::<String>() {
            println!("Cloned string message: {}", value);
        }
    } else {
        println!("Cannot clone string message");
    }

    // Clone a custom complex message
    if let Some(cloned) = ApiCloneable::try_from_boxed(&complex_message) {
        let cloned = cloned.into_boxed();
        if let Some(value) = cloned.downcast_ref::<CustomMessage>() {
            println!(
                "Cloned custom message: ID={}, payload={}, timestamp={}",
                value.id, value.payload, value.timestamp
            );
        }
    } else {
        println!("Cannot clone custom message");
    }

    // make_cloneable + clone_box: clone through the concrete CloneableMessage
    // impl of the wrapped type (downcast first, then clone_box).
    println!("\nUsing make_cloneable + CloneableMessage::clone_box:");

    let another_message = CustomMessage {
        id: 2002,
        payload: "Another important data".to_string(),
        timestamp: 1622635200,
    };

    let cloneable_msg = make_cloneable(another_message);
    for i in 1..=3 {
        let inner = cloneable_msg
            .downcast_ref::<CustomMessage>()
            .expect("payload is CustomMessage");
        let cloned = ThreadCloneable::clone_box(inner);
        if let Some(value) = cloned.downcast_ref::<CustomMessage>() {
            println!("Clone #{}: ID={}, payload={}", i, value.id, value.payload);
        }
    }

    println!("\nPeriodic-scheduling usage sketch:");
    println!("let tick = make_cloneable(MyTickMessage {{ count: 0 }});");
    println!("ctx.schedule_periodic(self_ref, tick, initial_delay, interval).await?;");
}
