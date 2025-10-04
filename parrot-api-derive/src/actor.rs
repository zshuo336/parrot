use darling::{FromAttributes, FromMeta};
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{parse_macro_input, parse_str, Attribute, DeriveInput, Ident, Type};

/// Actor attribute options for customizing actor behavior
#[derive(Debug, Default, FromMeta)]
pub struct ActorOptions {
    /// Engine to use for actor implementation.
    ///
    /// The engine name is resolved to the `__parrot_engine` module binding at
    /// the use site (M1 derive-decouple): the macro emits engine-neutral
    /// symbols only; the user binds the concrete engine with
    /// `use parrot::actix as __parrot_engine;` (or the thread equivalent).
    #[darling(default)]
    engine: Option<EngineValue>,

    /// Configuration type for actor
    #[darling(default)]
    config: Option<String>,

    /// Supervision strategy
    ///（向前兼容占位：M6 后无生成物消费，保留解析能力）
    #[darling(default)]
    #[allow(dead_code)]
    supervision: Option<String>,

    /// Dispatcher type (for future use)
    #[darling(default)]
    #[allow(dead_code)]
    dispatcher: Option<String>,

    /// Opt this actor into the async dispatch path (`use_async_handler`).
    ///
    /// When `true`, the generated impl overrides `use_async_handler()` to
    /// return `true`, which makes the Actix engine route every message
    /// through the async `receive_message` (i.e. the user's
    /// `handle_message`) instead of probing the sync
    /// `handle_message_engine` fast path first.
    #[darling(default)]
    async_handler: Option<bool>,
}

/// Represents an engine value that can be a string or an identifier
#[derive(Debug)]
pub enum EngineValue {
    /// Named engine (e.g., "actix", "thread")
    Named(String),
    /// Identifier reference to an engine constant
    Ident(String),
}

impl Default for EngineValue {
    fn default() -> Self {
        EngineValue::Named("actix".to_string())
    }
}

/// Engines the macro knows how to shape code for.
///
/// NOTE(M1): the engine name does NOT select a concrete crate symbol —
/// all generated code goes through the `__parrot_engine` binding at the
/// use site. The name only drives small codegen differences (e.g. the
/// actix engine has an `EngineContextHandle` fast path; the thread engine
/// does not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    Actix,
    Thread,
}

impl EngineKind {
    fn from_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "actix" => Some(EngineKind::Actix),
            "thread" => Some(EngineKind::Thread),
            // Historical alias: early docs used "tokio" for the thread engine.
            "tokio" => Some(EngineKind::Thread),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            EngineKind::Actix => "actix",
            EngineKind::Thread => "thread",
        }
    }
}

impl FromMeta for EngineValue {
    fn from_value(value: &syn::Lit) -> darling::Result<Self> {
        match value {
            syn::Lit::Str(lit_str) => {
                let value = lit_str.value();
                Ok(EngineValue::Named(value))
            }
            _ => Err(darling::Error::unexpected_lit_type(value)),
        }
    }

    fn from_expr(expr: &syn::Expr) -> darling::Result<Self> {
        match expr {
            // Handle path expressions like engine = ACTIX
            syn::Expr::Path(path) => {
                if let Some(ident) = path.path.get_ident() {
                    return Ok(EngineValue::Ident(ident.to_string()));
                }
                Err(darling::Error::custom("Expected a simple identifier"))
            }
            // Pass to from_value for literals
            syn::Expr::Lit(expr_lit) => Self::from_value(&expr_lit.lit),
            _ => Err(darling::Error::unexpected_expr_type(expr)),
        }
    }
}

#[derive(Debug, FromAttributes)]
#[darling(attributes(ParrotActor))]
pub struct ActorArgs {
    #[darling(flatten)]
    opts: ActorOptions,
}

/// Parse actor attributes to extract options
pub fn parse_actor_attrs(attrs: &[Attribute]) -> ActorOptions {
    ActorArgs::from_attributes(attrs)
        .map(|a| a.opts)
        .unwrap_or_default()
}

/// Implementation of the ParrotActor derive macro.
///
/// ## Engine neutrality (M1 derive-decouple)
///
/// The generated code contains **zero concrete engine symbol paths**
/// (no `parrot::actix::*`, no `parrot::thread::*`). Instead, all
/// engine-specific types are referenced through the `__parrot_engine`
/// module alias which the user binds at the use site:
///
/// ```ignore
/// use parrot::actix as __parrot_engine;   // actix engine
/// // or
/// use parrot::thread as __parrot_engine;  // thread engine
/// ```
///
/// Each engine exposes the neutral face consumed by the generated code:
/// - `__parrot_engine::EngineContext<A>` — the engine's context type for A
///
/// This restores "specification does not depend on implementation" at the
/// symbol level (evaluation-report D2 debt): the macro crate generates the
/// same code shape for every engine; engines are plug-compatible via the
/// alias, and `parrot-api-derive` compiles without any engine dependency.
pub(crate) fn derive_actor_impl(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    // Parse the actor attributes into an ActorOptions struct
    let options: ActorOptions = parse_actor_attrs(&input.attrs);

    // Extract actor name
    let actor_name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    // Determine the engine kind (codegen shaping only, not symbol selection)
    let engine = match options.engine {
        Some(EngineValue::Named(ref name)) => name.clone(),
        Some(EngineValue::Ident(ref ident)) => ident.clone(),
        None => "actix".to_string(), // Default to actix engine
    };

    let engine_kind = match EngineKind::from_name(&engine) {
        Some(kind) => kind,
        None => {
            let error_message = format!(
                "Unsupported engine: {}. Known engines: \"actix\", \"thread\" \
                 (bind the engine with `use parrot::{} as __parrot_engine;`)",
                engine, engine
            );
            return syn::Error::new(Span::call_site(), error_message)
                .to_compile_error()
                .into();
        }
    };

    // Parse the configuration type
    let config_type = if let Some(ref config_type) = options.config {
        parse_str::<Type>(config_type)
            .unwrap_or_else(|_| parse_str("parrot_api::actor::EmptyConfig").unwrap())
    } else {
        // If configuration type is not specified, use EmptyConfig instead of ()
        parse_str("parrot_api::actor::EmptyConfig").unwrap()
    };

    // Whether the generated impl opts into the async dispatch path.
    let async_handler = options.async_handler.unwrap_or(false);

    let implementation = generate_engine_neutral_implementation(
        actor_name,
        &impl_generics,
        &ty_generics,
        where_clause,
        &config_type,
        async_handler,
        engine_kind,
    );

    implementation.into()
}

/// Generate the engine-neutral Actor implementation.
///
/// Message dispatch contract (ADR-3 resolution):
///
/// - `receive_message` now *always* forwards to the user's `handle_message`
///   (async). This is the primary path of the thread engine, and of the
///   Actix engine when `async_handler = true`. The historical
///   `#[cfg(not(test))]` error branch made derive-based actors completely
///   unusable on the thread engine in production and blocked the async
///   Actix path; test/prod semantics are now identical.
/// - `receive_message_with_engine` still forwards to the user's
///   `handle_message_engine` (sync, Actix-only fast path). Returning `None`
///   there means "not handled" and the adapter falls back to
///   `handle_message` instead of dropping the message.
///
/// Engine-neutral symbols used by the generated code (bound at use site):
///
/// - `__parrot_engine::EngineContext<A>`: the context type. The user's
///   `handle_message` / `handle_message_engine` signatures must use the
///   same alias (see docs/examples).
fn generate_engine_neutral_implementation(
    actor_name: &Ident,
    impl_generics: &syn::ImplGenerics,
    ty_generics: &syn::TypeGenerics,
    where_clause: Option<&syn::WhereClause>,
    config_type: &Type,
    async_handler: bool,
    engine_kind: EngineKind,
) -> TokenStream2 {
    // Only emit the `use_async_handler` override when opted in; otherwise
    // the trait default (`false`) applies and manual overrides still win.
    let use_async_handler_override = if async_handler {
        quote! {
            fn use_async_handler(&self) -> bool {
                true
            }
        }
    } else {
        quote! {}
    };

    // M6: `receive_message_with_engine` moved from the spec `Actor` trait
    // to the actix-side extension trait `ActixEngineExt` (blanket-impl'd
    // with a `None` default). The derive emits a separate extension impl
    // for the actix engine (forwarding to the user's
    // `handle_message_engine`); the thread engine needs none (blanket
    // default applies).

    let _ = engine_kind.as_str(); // available for diagnostics if needed

    // M6: actix 扩展 trait（ActixEngineExt）单独立 impl 块——
    // `receive_message_with_engine` 已从规范 Actor trait 移出。
    let engine_ext_impl = match engine_kind {
        EngineKind::Actix => quote! {
            impl #impl_generics parrot_api::actor::ActixEngineExt for #actor_name #ty_generics #where_clause {
                // Actix sync fast path: forward to the user's
                // `handle_message_engine`. (The blanket impl defaults to
                // `None`; this override exists so user handlers wire up.)
                fn receive_message_with_engine<'a>(&'a mut self, msg: parrot_api::types::BoxedMessage, ctx: &'a mut __parrot_engine::EngineContext<Self>, engine_ctx: parrot_api::actor::EngineContextHandle)
                    -> Option<parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>> {
                    self.handle_message_engine(msg, ctx, engine_ctx)
                }
            }
        },
        EngineKind::Thread => quote! {
            // Thread engine never probes the sync fast path; the blanket
            // impl default (`None`) applies. No extension impl emitted.
        },
    };

    quote! {
        impl #impl_generics parrot_api::actor::Actor for #actor_name #ty_generics #where_clause {
            type Context = __parrot_engine::EngineContext<Self>;
            type Config = #config_type;

            fn receive_message<'a>(&'a mut self, msg: parrot_api::types::BoxedMessage, ctx: &'a mut Self::Context)
                -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>> {
                Box::pin(async move {
                    self.handle_message(msg, ctx).await
                })
            }

            #use_async_handler_override

            fn state(&self) -> parrot_api::actor::ActorState {
                // Default implementation returns Running
                parrot_api::actor::ActorState::Running
            }
        }

        #engine_ext_impl
    }
}
