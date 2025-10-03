use proc_macro::TokenStream;

mod actor;
mod message;
mod typed;

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
/// # use parrot_api::{Message, MessageDerive};
/// #[derive(MessageDerive)]
/// #[message(result = "Option<UserProfile>")]
/// struct GetUserProfile {
///     user_id: String,
/// }
/// # struct UserProfile;
/// ```
///
/// ## 2. Validation Rules
/// ```rust
/// # use parrot_api::{Message, MessageDerive};
/// #[derive(MessageDerive)]
/// #[message(
///     validate = "self.amount > 0.0 && self.items.len() > 0",
///     result = "OrderResult"
/// )]
/// struct CreateOrder {
///     user_id: String,
///     amount: f64,
///     items: Vec<String>,
/// }
/// # struct OrderResult;
/// ```
///
/// ## 3. Message Priority
/// ```rust
/// # use parrot_api::{Message, MessageDerive};
/// // Using a named priority
/// #[derive(MessageDerive)]
/// #[message(priority = "HIGH")]  // Sets message processing priority to HIGH (70)
/// struct EmergencyAlert {
///     alert_type: String,
///     message: String,
/// }
///
/// // Using a numeric priority (0-100)
/// #[derive(MessageDerive)]
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
/// # use parrot_api::{Message, MessageDerive};
/// #[derive(MessageDerive)]
/// #[message(
///     result = "Vec<Order>",           // Custom return type
///     validate = "self.amount > 0.0",      // Add validation
///     priority = "HIGH"               // Set priority
/// )]
/// struct CreateOrder {
///     user_id: String,
///     amount: f64,
///     items: Vec<String>,
/// }
/// # #[derive(Clone)] struct Order;
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
/// assert!(order.message_type().ends_with("CreateOrder"));
///
/// // Convert to envelope
/// let _envelope = order.into_envelope();
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
/// ```ignore
/// #[derive(ParrotActor)]
/// #[ParrotActor(engine = "actix")]  // Use Actix as the actor backend
/// struct MyActor {
///     counter: u32,
/// }
/// ```
///
/// Using a constant:
/// ```ignore
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
/// ```ignore
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
/// ```ignore
/// use parrot_api::actor::Actor;
/// use parrot_api::message::Message;
///
/// // M1 derive-decouple: bind the engine once at the use site.
/// // The macro-generated code references `__parrot_engine::*` only.
/// use parrot::actix as __parrot_engine;
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
/// // Implement message handling (use the same engine alias in signatures)
/// impl CounterActor {
///     async fn handle_message(&mut self, msg: BoxedMessage, ctx: &mut __parrot_engine::EngineContext<Self>)
///         -> ActorResult<BoxedMessage> {
///         if let Some(increment) = msg.downcast_ref::<Increment>() {
///             self.value += increment.0;
///             return Ok(Box::new(self.value));
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
///
/// ## 4. Engine Switching (M1)
///
/// Switching engines is a one-line change at the binding site:
///
/// ```ignore
/// use parrot::actix as __parrot_engine;   // actix engine
/// // ... or ...
/// use parrot::thread as __parrot_engine;  // thread engine
/// ```
///
/// The derive attribute (`engine = "actix"` / `engine = "thread"`) only
/// shapes small codegen details (e.g. the actix sync fast path); all type
/// references flow through the alias, so the macro crate never depends on
/// any engine crate.
#[proc_macro_derive(ParrotActor, attributes(ParrotActor))]
pub fn derive_parrot_actor(input: TokenStream) -> TokenStream {
    actor::derive_actor_impl(input)
}

/// M4: 静态类型轨 derive——枚举信封 + match 分派（零 Any）。
///
/// 按属性声明的消息集生成 `ParrotTypedDispatch`（`{Actor}Msg` 枚举 +
/// `{Actor}Reply` 回复枚举 + match 分派）与每消息的
/// `ParrotMsgVariant`（inject/extract 类型级映射）。配合 thread 引擎的
/// `ThreadActorSystem::spawn_typed` 使用；动态轨完全不受影响。
///
/// ```ignore
/// #[derive(ParrotTypedActor)]
/// #[ParrotTypedActor(msgs(Add, Get))]
/// struct Calc { n: u64 }
///
/// impl TypedReceive<Add> for Calc {
///     async fn receive_typed(&mut self, msg: Add) -> ActorResult<u64> {
///         self.n += msg.0; Ok(self.n)
///     }
/// }
///
/// // spawn_typed(Calc::default(), "/calc") -> TypedActorRef<Calc, Add>
/// // r.ask(Add(41)).await -> u64（零 Any）
/// // r.ref_for::<Get>().ask(Get).await -> u64（同一 actor 多协议）
/// ```
#[proc_macro_derive(ParrotTypedActor, attributes(ParrotTypedActor))]
pub fn derive_parrot_typed_actor(input: TokenStream) -> TokenStream {
    typed::derive_typed_actor_impl(input)
}
