/// Box a response while checking the selected message's associated result type.
/// No trait imports are required at the invocation site. A mismatched response
/// remains a compilation error:
///
/// ```compile_fail
/// struct Query;
/// impl parrot_api::message::Message for Query { type Result = u64; }
/// let result: parrot_api::types::ActorResult<parrot_api::types::BoxedMessage> =
///     parrot_api::message_response_ok!(Query, String::from("wrong response"));
/// ```
#[macro_export]
macro_rules! message_response_ok {
    ($type:ty, $value:expr) => {{
        // Compile-time type check - will fail compilation if types don't match
        let _result_with_type: <$type as $crate::message::Message>::Result = $value;

        // Wrap the result directly within the macro
        let boxed_result: $crate::types::BoxedMessage = ::std::boxed::Box::new(_result_with_type);
        ::core::result::Result::Ok(boxed_result)
    }};

    ("option", $type:ty, $value:expr) => {{
        let _result_with_type: <$type as $crate::message::Message>::Result = $value;
        let boxed_result: $crate::types::BoxedMessage = ::std::boxed::Box::new(_result_with_type);
        ::core::option::Option::Some(::core::result::Result::Ok(boxed_result))
    }};

    ("result", $type:ty, $value:expr) => {{
        let _result_with_type: <$type as $crate::message::Message>::Result = $value;
        let boxed_result: $crate::types::BoxedMessage = ::std::boxed::Box::new(_result_with_type);
        ::core::result::Result::Ok(boxed_result)
    }};
}

/// Matches message types and dispatches to corresponding handler functions
///
/// This macro is used in the Actor's handle_message method to call different handler functions based on message types
/// The `"async"` form selects a handler synchronously and returns
/// `ActorResult<BoxedFuture<'_, ActorResult<BoxedMessage>>>`. Its handler is
/// invoked when that future is polled; unknown message types fail at selection.
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
            _ => ::core::result::Result::Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string()))
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
            _ => ::core::option::Option::Some(::core::result::Result::Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string())))
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
            _ => ::core::result::Result::Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string()))
        }
    };

    // Deferred selection uses one erased future type across all message branches.
    ("async", $self:ident, $msg:ident, $( $type:ty => $handler:expr ),* $(,)?) => {
        match () {
            $(
                _ if $msg.downcast_ref::<$type>().is_some() => {
                    let handler_fn = $handler;
                    let message = $msg.downcast_ref::<$type>().unwrap();
                    let future: $crate::types::BoxedFuture<'_, $crate::types::ActorResult<$crate::types::BoxedMessage>> =
                        ::std::boxed::Box::pin(async move {
                            let result = handler_fn($self, message).await;
                            $crate::message_response_ok!($type, result)
                        });
                    ::core::result::Result::Ok(future)
                }
            )*
            _ => ::core::result::Result::Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string()))
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
            _ => ::core::result::Result::Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string()))
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
            _ => ::core::option::Option::Some(::core::result::Result::Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string())))
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
            _ => ::core::result::Result::Err($crate::errors::ActorError::MessageHandlingError("Unknown message type".to_string()))
        }
    };
}
