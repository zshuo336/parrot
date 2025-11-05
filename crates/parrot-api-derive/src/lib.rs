use proc_macro::TokenStream;

mod actor;
mod message;
mod remote;
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
/// Invalid option types, unknown/duplicate fields, invalid result/validation
/// syntax, unknown named priorities/retry strategies and literal priorities
/// outside 0..=100 are compile errors. Retry interval/strategy options require
/// `retry_max_attempts`. Custom priority constant identifiers are range-checked
/// in a const expression when their generated priority method is instantiated.
/// Generic parameters and where clauses are preserved; callers must supply
/// the Send and static bounds required by Message and its Result.
///
/// `new` invokes validation. Generated options are metadata; deriving them
/// alone does not install runtime retry/deadline enforcement.
#[proc_macro_derive(Message, attributes(message))]
pub fn derive_message(input: TokenStream) -> TokenStream {
    message::derive_message_impl(input)
}

/// Derives a concrete message's remote identity and inventory codec.
///
/// Default key: `bin:{module_path}::{Type}#v1`. This includes the crate and modules
/// at the expansion site. Use an explicit stable key across module/crate renames;
/// function-local types with the same name in one module also need explicit keys.
/// Generic messages must be wrapped in concrete types for static registration.
///
/// `bin:` uses serde + bincode. `pb:` uses prost::Message + Default and requires
/// parrot-api's `remote-protobuf` feature (enabled by parrot-remote).
/// Unknown, malformed, and duplicate remote options are compile errors.
///
/// ```
/// use parrot_api_derive::RemoteMessage;
/// #[derive(serde::Serialize, serde::Deserialize, RemoteMessage)]
/// #[remote(key = "bin:myapp::Echo#v2")]
/// struct Echo(u64);
/// impl parrot_api::Message for Echo { type Result = (); }
/// assert_eq!(<Echo as parrot_api::message::RemoteMessage>::TYPE_KEY, "bin:myapp::Echo#v2");
/// ```
///
/// Protobuf fields carry explicit schema tags; serde alone cannot encode protobuf:
///
/// ```
/// use parrot_api_derive::RemoteMessage;
/// #[derive(Clone, PartialEq, prost::Message, RemoteMessage)]
/// #[remote(key = "pb:myapp.Echo")]
/// struct Echo { #[prost(uint64, tag = "1")] value: u64 }
/// impl parrot_api::Message for Echo { type Result = (); }
/// ```
///
/// ```compile_fail
/// use parrot_api_derive::RemoteMessage;
/// #[derive(serde::Serialize, serde::Deserialize, RemoteMessage)]
/// #[remote(kee = "bin:myapp::Echo#v1")]
/// struct Echo(u64);
/// impl parrot_api::Message for Echo { type Result = (); }
/// ```
///
/// ```compile_fail
/// use parrot_api_derive::RemoteMessage;
/// #[derive(serde::Serialize, serde::Deserialize, RemoteMessage)]
/// #[remote(key = "pb:myapp.Echo")]
/// struct Echo(u64); // Does not implement prost::Message.
/// impl parrot_api::Message for Echo { type Result = (); }
/// ```
#[proc_macro_derive(RemoteMessage, attributes(remote))]
pub fn derive_remote_message(input: TokenStream) -> TokenStream {
    remote::derive_remote_message_impl(input)
}

/// Implements `parrot_api::Actor` and, for Actix, `ActixEngineExt`.
///
/// Bind the chosen runtime as `__parrot_engine` at the use site. Set
/// `#[ParrotActor(engine = "actix")]` or `#[ParrotActor(engine = "thread")]`
/// to select the generated context/dispatch shape; `"tokio"` is a historical
/// alias for `"thread"`. The default is Actix. Identifier labels such as
/// `ACTIX` and `THREAD` are recognized by their spelling; the macro does not
/// evaluate the value of a Rust constant.
///
/// `config = "MyConfig"` selects an `ActorConfig` type, defaulting to
/// `parrot_api::actor::EmptyConfig`. `async_handler = true` opts Actix into
/// the async `handle_message` path. Actix derives also require
/// `handle_message_engine`; synchronous mode uses this method and reports
/// unhandled messages when it returns `None`. Thread derives need only the
/// async handler. Context signatures use `__parrot_engine::EngineContext<Self>`.
///
/// Malformed, unknown and duplicate options, unknown engines and invalid
/// configuration type syntax are compilation errors. Legacy `supervision`
/// and `dispatcher` attributes are rejected because they have no runtime
/// wiring; configure the engine/system explicitly. Their future support is
/// separate from the dispatch implementation, not silently enabled here.
///
/// Lifecycle management belongs to the selected runtime. Switching runtimes
/// also requires the corresponding constructor and context/handler shape;
/// changing the module alias alone is insufficient. For a complete executable
/// example on both engines, run from the repository root:
///
/// ```text
/// cargo run --locked -p parrot --example quickstart
/// ```
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
/// ```rust
/// use parrot_api::{Message, ParrotTypedActor};
/// use parrot_api::typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive};
/// use parrot_api::types::{ActorResult, BoxedFuture};
///
/// pub struct Add(u64);
/// impl Message for Add { type Result = u64; }
/// pub struct Get;
/// impl Message for Get { type Result = u64; }
///
/// #[derive(ParrotTypedActor)]
/// #[ParrotTypedActor(msgs(Add, Get))]
/// struct Calc { n: u64 }
///
/// impl TypedReceive<Add> for Calc {
///     fn receive_typed<'a>(&'a mut self, msg: Add) -> BoxedFuture<'a, ActorResult<u64>> {
///         Box::pin(async move { self.n += msg.0; Ok(self.n) })
///     }
/// }
/// impl TypedReceive<Get> for Calc {
///     fn receive_typed<'a>(&'a mut self, _: Get) -> BoxedFuture<'a, ActorResult<u64>> {
///         Box::pin(async move { Ok(self.n) })
///     }
/// }
///
/// // The protocol compiles independently of any engine.
/// fn check_protocol<A: ParrotTypedDispatch, M: ParrotMsgVariant<A>>() {}
/// check_protocol::<Calc, Add>();
/// check_protocol::<Calc, Get>();
/// ```
#[proc_macro_derive(ParrotTypedActor, attributes(ParrotTypedActor))]
pub fn derive_parrot_typed_actor(input: TokenStream) -> TokenStream {
    typed::derive_typed_actor_impl(input)
}
