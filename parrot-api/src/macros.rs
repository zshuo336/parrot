#[macro_export]
macro_rules! message_response_ok {
    ($type:ty, $value:expr) => {{
        // Compile-time type check - will fail compilation if types don't match
        let _result_with_type: <$type as Message>::Result = $value;
        
        // Wrap the result directly within the macro
        let boxed_result: $crate::types::BoxedMessage = Box::new(_result_with_type) as Box<dyn std::any::Any + Send>;
        Ok(boxed_result)
    }};
    
    ("option", $type:ty, $value:expr) => {{
        let _result_with_type: <$type as Message>::Result = $value;
        let boxed_result: $crate::types::BoxedMessage = Box::new(_result_with_type) as Box<dyn std::any::Any + Send>;
        Some(Ok(boxed_result))
    }};
    
    ("result", $type:ty, $value:expr) => {{
        let _result_with_type: <$type as Message>::Result = $value;
        let boxed_result: $crate::types::BoxedMessage = Box::new(_result_with_type) as Box<dyn std::any::Any + Send>;
        Ok(boxed_result)
    }};
}


/// Matches message types and dispatches to corresponding handler functions
/// 
/// This macro is used in the Actor's handle_message method to call different handler functions based on message types
#[macro_export]
macro_rules! match_message {
    // base version - return ActorResult<BoxedMessage>
    ($self:ident, $msg:ident, $( $type:ty => $handler:expr ),* $(,)?) => {
        match () {
            $(
                _ if $msg.downcast_ref::<$type>().is_some() => {
                    let handler_fn = $handler;
                    let result = handler_fn($self, $msg.downcast_ref::<$type>().unwrap());
                    
                    // Use full path to reference the macro
                    $crate::message_response_ok!($type, result)
                }
            )*
            _ => Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string()))
        }
    };
    
    // option version - return Option<ActorResult<BoxedMessage>>
    ("option", $self:ident, $msg:ident, $( $type:ty => $handler:expr ),* $(,)?) => {
        match () {
            $(
                _ if $msg.downcast_ref::<$type>().is_some() => {
                    let handler_fn = $handler;
                    let result = handler_fn($self, $msg.downcast_ref::<$type>().unwrap());
                    $crate::message_response_ok!("option", $type, result)
                }
            )*
            _ => Some(Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string())))
        }
    };
    
    // result version - return ActorResult<BoxedMessage>
    ("result", $self:ident, $msg:ident, $( $type:ty => $handler:expr ),* $(,)?) => {
        match () {
            $(
                _ if $msg.downcast_ref::<$type>().is_some() => {
                    let handler_fn = $handler;
                    let result = handler_fn($self, $msg.downcast_ref::<$type>().unwrap());
                    $crate::message_response_ok!("result", $type, result)
                }
            )*
            _ => Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string()))
        }
    };

    // async version - return ActorResult<BoxedFuture>  
    ("async", $self:ident, $msg:ident, $( $type:ty => $handler:expr ),* $(,)?) => {
        match () {
            $(
                _ if $msg.downcast_ref::<$type>().is_some() => {
                    let handler_fn = $handler;
                    let result = handler_fn($self, $msg.downcast_ref::<$type>().unwrap()).await;
                    // return BoxedFuture
                    Ok(Box::pin(result))
                }
            )*
            _ => Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string()))
        }
    };
}

/// Matches message types and dispatches to corresponding async handler functions
/// 
/// This macro is similar to match_message but supports async handlers
/// Each handler must be an async function or async block
#[macro_export]
macro_rules! match_async_message {
    // base version - return ActorResult<BoxedMessage>
    ($self:ident, $msg:ident, $( $type:ty => $handler:expr ),* $(,)?) => {
        match () {
            $(
                _ if $msg.downcast_ref::<$type>().is_some() => {
                    let handler_fn = $handler;
                    let result = handler_fn($self, $msg.downcast_ref::<$type>().unwrap()).await;
                    
                    // Use full path to reference the macro
                    $crate::message_response_ok!($type, result)
                }
            )*
            _ => Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string()))
        }
    };
    
    // option version - return Option<ActorResult<BoxedMessage>>
    ("option", $self:ident, $msg:ident, $( $type:ty => $handler:expr ),* $(,)?) => {
        match () {
            $(
                _ if $msg.downcast_ref::<$type>().is_some() => {
                    let handler_fn = $handler;
                    let result = handler_fn($self, $msg.downcast_ref::<$type>().unwrap()).await;
                    $crate::message_response_ok!("option", $type, result)
                }
            )*
            _ => Some(Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string())))
        }
    };
    
    // result version - return ActorResult<BoxedMessage>
    ("result", $self:ident, $msg:ident, $( $type:ty => $handler:expr ),* $(,)?) => {
        match () {
            $(
                _ if $msg.downcast_ref::<$type>().is_some() => {
                    let handler_fn = $handler;
                    let result = handler_fn($self, $msg.downcast_ref::<$type>().unwrap()).await;
                    $crate::message_response_ok!("result", $type, result)
                }
            )*
            _ => Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string()))
        }
    };
}

