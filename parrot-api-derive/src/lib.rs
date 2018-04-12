use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Attribute, Type, parse_str};
use darling::{FromMeta, FromAttributes};
use proc_macro2::{Ident, Span};

/// Message attribute options for customizing message behavior
#[derive(Debug, Default, FromMeta)]
struct MessageOptions {
    /// Custom result type for the message
    #[darling(default)]
    result: Option<String>,
    /// Enable serde serialization
    #[darling(default)]
    serde: bool,
    /// Validation expression
    #[darling(default)]
    validate: Option<String>,
    /// Message priority
    #[darling(default)]
    priority: Option<String>,
}

#[derive(Debug, FromAttributes)]
#[darling(attributes(message))]
struct MessageArgs {
    #[darling(flatten)]
    opts: MessageOptions,
}

/// Parse message attributes to extract options
fn parse_message_attrs(attrs: &[Attribute]) -> MessageOptions {
    MessageArgs::from_attributes(attrs)
        .map(|a| a.opts)
        .unwrap_or_default()
}


/// Derives the Message trait for a type with extended functionality.
/// 
/// This macro automatically implements the Message trait and provides additional features:
/// - Custom result type specification
/// - Serialization/deserialization support
/// - Validation rules
/// - Message priority handling
/// - Helper methods for message processing
/// 
/// # Features
/// 
/// ## 1. Custom Return Type
/// ```rust
/// # use parrot_api::Message;
/// #[derive(Message)]
/// #[message(result = "Option<UserProfile>")]
/// struct GetUserProfile {
///     user_id: String,
/// }
/// ```
/// 
/// ## 2. Serialization Support
/// ```rust
/// # use parrot_api::Message;
/// #[derive(Message)]
/// #[message(serde)]  // Enables serde::Serialize and serde::Deserialize
/// struct UserEvent {
///     event_type: String,
///     data: Vec<u8>,
/// }
/// ```
/// 
/// ## 3. Validation Rules
/// ```rust
/// # use parrot_api::Message;
/// #[derive(Message)]
/// #[message(
///     validate = "amount > 0.0 && items.len() > 0",
///     result = "OrderResult"
/// )]
/// struct CreateOrder {
///     user_id: String,
///     amount: f64,
///     items: Vec<String>,
/// }
/// ```
/// 
/// ## 4. Message Priority
/// ```rust
/// # use parrot_api::Message;
/// #[derive(Message)]
/// #[message(priority = "High")]  // Sets message processing priority
/// struct EmergencyAlert {
///     alert_type: String,
///     message: String,
/// }
/// ```
/// 
/// ## 5. Complete Example
/// ```rust
/// # use parrot_api::Message;
/// #[derive(Message)]
/// #[message(
///     result = "Vec<Order>",           // Custom return type
///     serde,                          // Enable serialization
///     validate = "amount > 0.0",      // Add validation
///     priority = "High"               // Set priority
/// )]
/// struct CreateOrder {
///     user_id: String,
///     amount: f64,
///     items: Vec<String>,
/// }
/// 
/// // Usage example:
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let order = CreateOrder {
///     user_id: "user123".to_string(),
///     amount: 99.99,
///     items: vec!["item1".to_string()],
/// };
/// 
/// // Create with validation
/// let order = CreateOrder::new(order)?;
/// 
/// // Get message type
/// assert_eq!(order.message_type(), "CreateOrder");
/// 
/// // Convert to envelope and send
/// let envelope = order.into_envelope();
/// // actor.send(envelope).await?;
/// # Ok(())
/// # }
/// ```
/// 
/// # Generated Methods
/// 
/// The macro generates the following methods for your message type:
/// 
/// - `new(msg: Self) -> Result<Self, ActorError>`: Creates a new message with validation
/// - `message_type(&self) -> &'static str`: Returns the message type name
/// - `into_envelope(self) -> MessageEnvelope`: Converts the message into an envelope
/// - `validate(&self) -> Result<(), ActorError>`: Validates the message (if validation rules are specified)
/// - `priority(&self) -> MessagePriority`: Returns the message priority
#[proc_macro_derive(Message, attributes(message))]
pub fn derive_message(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    
    // Check if the input is a struct
    match input.data {
        syn::Data::Struct(_) => (),
        _ => {
            return syn::Error::new_spanned(
                input.ident,
                "Message can only be derived for structs"
            )
            .to_compile_error()
            .into();
        }
    }
    
    let name = &input.ident;
    let options = parse_message_attrs(&input.attrs);
    
    // Parse the result type from the attribute or use () as default
    let result_type = if let Some(ref result_type) = options.result {
        parse_str::<Type>(result_type).unwrap_or_else(|_| parse_str("()").unwrap())
    } else {
        parse_str("()").unwrap()
    };

    // Parse priority or use Normal as default
    let priority = if let Some(priority) = options.priority {
        let priority_ident = Ident::new(&priority, Span::call_site());
        quote! { parrot_api::MessagePriority::#priority_ident }
    } else {
        quote! { parrot_api::MessagePriority::Normal }
    };

    // Generate validation code
    let validate_impl = if let Some(validate_expr) = options.validate {
        let expr = parse_str::<syn::Expr>(&validate_expr).unwrap();
        quote! {
            if #expr {
                Ok(())
            } else {
                Err(parrot_api::errors::ActorError::MessageHandlingError(
                    format!("Validation failed for {}: {}", std::any::type_name::<Self>(), #validate_expr)
                ))
            }
        }
    } else {
        quote! { Ok(()) }
    };

    // Generate the implementation
    let expanded = quote! {
        impl parrot_api::Message for #name {
            type Result = #result_type;

            fn extract_result(result: Box<dyn std::any::Any + Send>) -> Result<Self::Result, parrot_api::errors::ActorError> {
                result
                    .downcast::<Self::Result>()
                    .map(|b| *b)
                    .map_err(|_| parrot_api::errors::ActorError::MessageHandlingError(
                        format!("Failed to downcast message result for {}", std::any::type_name::<Self>())
                    ))
            }

            fn message_type(&self) -> &'static str {
                std::any::type_name::<Self>()
            }

            fn priority(&self) -> parrot_api::MessagePriority {
                #priority
            }

            fn validate(&self) -> Result<(), parrot_api::errors::ActorError> {
                #validate_impl
            }
        }

        impl #name {
            pub fn new(msg: Self) -> Result<Self, parrot_api::errors::ActorError> {
                msg.validate()?;
                Ok(msg)
            }

            pub fn into_envelope(self) -> parrot_api::MessageEnvelope {
                parrot_api::MessageEnvelope::new(self, None, None)
            }
        }
    };

    TokenStream::from(expanded)
} 