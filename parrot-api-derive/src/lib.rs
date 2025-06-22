use proc_macro::TokenStream;

mod message;
mod common;
mod actor;

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
#[proc_macro_derive(Message, attributes(message))]
pub fn derive_message(input: TokenStream) -> TokenStream {
    message::derive_message_impl(input)
}

/// Derives the ParrotActor trait for a type with engine-specific implementations.
/// 
/// This macro automatically implements the Actor trait for the specified engine:
/// - Implements necessary interface for the chosen actor system backend
/// - Handles message routing and conversion
/// - Manages actor lifecycle
/// 
/// # Features
/// 
/// ## 1. Engine Selection
/// 
/// Using a string literal:
/// ```rust
/// #[derive(ParrotActor)]
/// #[ParrotActor(engine = "actix")]  // Use Actix as the actor backend
/// struct MyActor {
///     counter: u32,
/// }
/// ```
/// 
/// Using a constant:
/// ```rust
/// #[derive(ParrotActor)]
/// #[ParrotActor(engine = ACTIX)]  // Use Actix as the actor backend
/// struct MyActor {
///     counter: u32,
/// }
/// ```
/// 
/// Supported engine values:
/// - "actix" (or ACTIX constant)
/// - Future engines will be supported as they are implemented
/// 
/// ## 2. Other Configuration Options
/// 
/// ```rust
/// #[derive(ParrotActor)]
/// #[ParrotActor(
///     engine = "actix",
///     config = "MyActorConfig",
///     supervision = "OneForOne",
///     dispatcher = "default"
/// )]
/// struct MyActor {
///     counter: u32,
/// }
/// ```
/// 
/// ## 3. Complete Example
/// ```rust
/// use parrot_api::actor::Actor;
/// use parrot_api::message::Message;
/// 
/// // Define a message
/// #[derive(Message)]
/// #[message(result = "u32")]
/// struct Increment(u32);
/// 
/// // Define the actor
/// #[derive(ParrotActor)]
/// #[ParrotActor(engine = "actix")]
/// struct CounterActor {
///     value: u32,
/// }
/// 
/// // Implement message handling
/// impl CounterActor {
///     async fn handle_message<M: Message>(&mut self, msg: M, ctx: &mut ActixContext) 
///         -> ActorResult<M::Result> {
///         if let Some(increment) = msg.downcast_ref::<Increment>() {
///             self.value += increment.0;
///             return Ok(self.value);
///         }
///         
///         Err(ActorError::UnknownMessage)
///     }
/// }
/// 
/// // Usage in main()
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let system = ParrotActorSystem::start(ActorSystemConfig::default()).await?;
/// 
/// // Register Actix backend
/// let actix_system = ActixActorSystem::new().await?;
/// system.register_actix_system("actix", actix_system, true).await?;
/// 
/// // Create actor
/// let actor = CounterActor { value: 0 };
/// let actor_ref = system.spawn_root_typed(actor, ()).await?;
/// 
/// // Send message and get response
/// let result = actor_ref.send(Increment(5)).await?;
/// assert_eq!(result, 5);
/// # Ok(())
/// # }
/// ```
#[proc_macro_derive(ParrotActor, attributes(ParrotActor))]
pub fn derive_parrot_actor(input: TokenStream) -> TokenStream {
    actor::derive_actor_impl(input)
}
