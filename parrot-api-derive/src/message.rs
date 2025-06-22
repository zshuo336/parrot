use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Attribute, Type, parse_str};
use darling::{FromMeta, FromAttributes};
use proc_macro2::{Ident, Span};

/// Message attribute options for customizing message behavior
#[derive(Debug, Default, FromMeta)]
pub struct MessageOptions {
    /// Custom result type for the message
    #[darling(default)]
    result: Option<String>,
    /// Validation expression
    #[darling(default)]
    validate: Option<String>,
    /// Message priority - either a number (0-100) or a named priority
    #[darling(default)]
    priority: Option<PriorityValue>,
    /// Message timeout in seconds
    #[darling(default)]
    timeout: Option<u64>,
    /// Retry policy
    #[darling(default)]
    retry_max_attempts: Option<u32>,
    #[darling(default)]
    retry_interval: Option<u64>,
    #[darling(default)]
    retry_strategy: Option<String>,
}

/// Represents a priority value that can be either a number, a named priority, or an identifier
#[derive(Debug)]
pub enum PriorityValue {
    /// Numeric priority value (0-100)
    Numeric(u8),
    /// Named priority (BACKGROUND, LOW, NORMAL, HIGH, CRITICAL)
    Named(String),
    /// Identifier reference to a constant
    Ident(String),
}

impl Default for PriorityValue {
    fn default() -> Self {
        PriorityValue::Named("NORMAL".to_string())
    }
}

impl FromMeta for PriorityValue {
    fn from_value(value: &syn::Lit) -> darling::Result<Self> {
        match value {
            syn::Lit::Int(lit_int) => {
                let value = lit_int.base10_parse::<u8>()?;
                Ok(PriorityValue::Numeric(value))
            },
            syn::Lit::Str(lit_str) => {
                let value = lit_str.value();
                Ok(PriorityValue::Named(value))
            },
            _ => Err(darling::Error::unexpected_lit_type(value)),
        }
    }

    fn from_expr(expr: &syn::Expr) -> darling::Result<Self> {
        match expr {
            // Handle path expressions like priority = HIGH
            syn::Expr::Path(path) => {
                if let Some(ident) = path.path.get_ident() {
                    return Ok(PriorityValue::Ident(ident.to_string()));
                }
                Err(darling::Error::custom("Expected a simple identifier"))
            },
            // Pass to from_value for literals
            syn::Expr::Lit(expr_lit) => Self::from_value(&expr_lit.lit),
            _ => Err(darling::Error::unexpected_expr_type(expr)),
        }
    }
}

#[derive(Debug, FromAttributes)]
#[darling(attributes(message))]
pub struct MessageArgs {
    #[darling(flatten)]
    opts: MessageOptions,
}

/// Parse message attributes to extract options
pub fn parse_message_attrs(attrs: &[Attribute]) -> MessageOptions {
    MessageArgs::from_attributes(attrs)
        .map(|a| a.opts)
        .unwrap_or_default()
}

/// Derives the Message trait for a type with extended functionality.
/// 
/// This macro automatically implements the Message trait and provides additional features:
/// - Custom result type specification
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
/// ## 2. Validation Rules
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
/// ## 3. Message Priority
/// ```rust
/// # use parrot_api::Message;
/// // Using a named priority
/// #[derive(Message)]
/// #[message(priority = "HIGH")]  // Sets message processing priority to HIGH (70)
/// struct EmergencyAlert {
///     alert_type: String,
///     message: String,
/// }
/// 
/// // Using a numeric priority (0-100)
/// #[derive(Message)]
/// #[message(priority = 75)]  // Sets custom priority level
/// struct CustomPriorityAlert {
///     alert_type: String,
///     message: String,
/// }
/// ```
/// 
/// Supported priority names:
/// - "BACKGROUND" (value 10)
/// - "LOW" (value 30)
/// - "NORMAL" (value 50)
/// - "HIGH" (value 70)
/// - "CRITICAL" (value 90)
/// 
/// Or numeric values from 0 to 100.
/// 
/// ## 4. Complete Example
/// ```rust
/// # use parrot_api::Message;
/// #[derive(Message)]
/// #[message(
///     result = "Vec<Order>",           // Custom return type
///     validate = "amount > 0.0",      // Add validation
///     priority = "HIGH"               // Set priority
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
/// // Convert to envelope
/// let envelope = order.into_envelope();
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
/// - `extract_result(result: Box<dyn Any + Send>) -> Result<Self::Result, ActorError>`: Extracts the result from a boxed Any type
pub fn derive_message_impl(input: TokenStream) -> TokenStream {
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
    // Parse the message attributes into a MessageOptions struct
    let options: MessageOptions = parse_message_attrs(&input.attrs);
    
    // Parse the result type from the attribute or use () as default
    let result_type = if let Some(ref result_type) = options.result {
        parse_str::<Type>(result_type).unwrap_or_else(|_| parse_str("()").unwrap())
    } else {
        parse_str("()").unwrap()
    };

    // Parse priority or use Normal as default
    let priority = if let Some(priority_value) = &options.priority {
        match priority_value {
            PriorityValue::Numeric(value) => {
                quote! { parrot_api::MessagePriority::new_unchecked(#value) }
            },
            PriorityValue::Named(name) => {
                match name.to_uppercase().as_str() {
                    "BACKGROUND" => quote! { parrot_api::MessagePriority::BACKGROUND },
                    "LOW" => quote! { parrot_api::MessagePriority::LOW },
                    "NORMAL" => quote! { parrot_api::MessagePriority::NORMAL },
                    "HIGH" => quote! { parrot_api::MessagePriority::HIGH },
                    "CRITICAL" => quote! { parrot_api::MessagePriority::CRITICAL },
                    _ => {
                        // Default to NORMAL if an invalid name is provided
                        quote! { parrot_api::MessagePriority::NORMAL }
                    }
                }
            },
            PriorityValue::Ident(ident) => {
                match ident.to_uppercase().as_str() {
                    "BACKGROUND" => quote! { parrot_api::MessagePriority::new_unchecked(parrot_api::BACKGROUND) },
                    "LOW" => quote! { parrot_api::MessagePriority::new_unchecked(parrot_api::LOW) },
                    "NORMAL" => quote! { parrot_api::MessagePriority::new_unchecked(parrot_api::NORMAL) },
                    "HIGH" => quote! { parrot_api::MessagePriority::new_unchecked(parrot_api::HIGH) },
                    "CRITICAL" => quote! { parrot_api::MessagePriority::new_unchecked(parrot_api::CRITICAL) },
                    _ => {
                        // For other identifiers, assume they are constants
                        let ident_token = syn::Ident::new(ident, proc_macro2::Span::call_site());
                        quote! { parrot_api::MessagePriority::new_unchecked(#ident_token) }
                    }
                }
            }
        }
    } else {
        quote! { parrot_api::MessagePriority::NORMAL }
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

    let timeout = options.timeout;
    let retry_max_attempts = options.retry_max_attempts;
    let retry_interval = options.retry_interval;
    let retry_strategy = options.retry_strategy.as_deref();

    // Modify Option<T> handling
    let timeout_opt = if let Some(timeout) = timeout {
        quote! { Some(std::time::Duration::from_secs(#timeout)) }
    } else {
        quote! { None }
    };
    
    let retry_policy_opt = if let Some(retry_max) = retry_max_attempts {
        let max_attempts = retry_max;
        let interval = retry_interval.unwrap_or(1);
        let strategy = match retry_strategy {
            Some("Fixed") => quote! { parrot_api::message::BackoffStrategy::Fixed },
            Some("Linear") => quote! { parrot_api::message::BackoffStrategy::Linear },
            Some("Exponential") => quote! { 
                parrot_api::message::BackoffStrategy::Exponential {
                    base: 2.0,
                    max_interval: std::time::Duration::from_secs(60),
                }
            },
            _ => quote! { parrot_api::message::BackoffStrategy::Fixed },
        };
        
        quote! {
            Some(parrot_api::message::RetryPolicy {
                max_attempts: #max_attempts,
                retry_interval: std::time::Duration::from_secs(#interval),
                backoff_strategy: #strategy,
            })
        }
    } else {
        quote! { None }
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

            fn message_options(&self) -> Option<parrot_api::MessageOptions> {
                Some(parrot_api::MessageOptions {
                    timeout: #timeout_opt,
                    retry_policy: #retry_policy_opt,
                    priority: self.priority(),
                })
            }
        }

        impl #name {
            pub fn new(msg: Self) -> Result<Self, parrot_api::errors::ActorError> {
                msg.validate()?;
                Ok(msg)
            }

            pub fn into_envelope(self) -> parrot_api::MessageEnvelope {
                let options = self.message_options();
                parrot_api::MessageEnvelope::new(self, None, options)
            }
        }
    };

    TokenStream::from(expanded)
} 