use darling::{FromAttributes, FromMeta};
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, Attribute, DeriveInput, Ident, Type};

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
    config: Option<syn::LitStr>,

    /// Supervision strategy
    /// Recognized legacy option, rejected until runtime wiring is implemented.
    #[darling(default)]
    supervision: Option<String>,

    /// Recognized legacy option, rejected until runtime wiring is implemented.
    #[darling(default)]
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
    /// Engine label such as ACTIX or THREAD, not evaluation of a Rust constant.
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
pub fn parse_actor_attrs(attrs: &[Attribute]) -> darling::Result<ActorOptions> {
    ActorArgs::from_attributes(attrs).map(|a| a.opts)
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
    expand(parse_macro_input!(input as DeriveInput))
        .unwrap_or_else(darling::Error::write_errors)
        .into()
}

fn expand(input: DeriveInput) -> darling::Result<TokenStream2> {
    // Parse the actor attributes into an ActorOptions struct
    let options = parse_actor_attrs(&input.attrs)?;
    for (name, present) in [
        ("supervision", options.supervision.is_some()),
        ("dispatcher", options.dispatcher.is_some()),
    ] {
        if present {
            return Err(darling::Error::custom(format!(
                "ParrotActor {name} is not wired to runtime configuration; configure the engine/system explicitly"
            ))
            .with_span(&input.ident));
        }
    }

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
            return Err(darling::Error::custom(error_message).with_span(&input.ident));
        }
    };

    // Parse the configuration type
    let config_type = if let Some(ref config_type) = options.config {
        config_type.parse::<Type>().map_err(|error| {
            darling::Error::custom(format!("invalid actor config type: {error}"))
                .with_span(config_type)
        })?
    } else {
        // If configuration type is not specified, use EmptyConfig instead of ()
        syn::parse_quote!(::parrot_api::actor::EmptyConfig)
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

    Ok(implementation)
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
///   there means "not handled" and the synchronous adapter returns an error.
///   `async_handler = true` selects the asynchronous handler directly.
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
    // to the actix-side extension trait `ActixEngineExt`. The derive emits an impl
    // for the actix engine (forwarding to the user's
    // `handle_message_engine`); the thread engine needs no extension impl.

    // M6: actix 扩展 trait（ActixEngineExt）单独立 impl 块——
    // `receive_message_with_engine` 已从规范 Actor trait 移出。
    let engine_ext_impl = match engine_kind {
        EngineKind::Actix => quote! {
            impl #impl_generics ::parrot_api::actor::ActixEngineExt for #actor_name #ty_generics #where_clause {
                // Actix sync fast path: forward to the user's
                // `handle_message_engine`.
                fn receive_message_with_engine<'a>(&'a mut self, msg: ::parrot_api::types::BoxedMessage, ctx: &'a mut __parrot_engine::EngineContext<Self>, engine_ctx: ::parrot_api::actor::EngineContextHandle)
                    -> ::std::option::Option<::parrot_api::types::ActorResult<::parrot_api::types::BoxedMessage>> {
                    self.handle_message_engine(msg, ctx, engine_ctx)
                }
            }
        },
        EngineKind::Thread => quote! {
            // Thread engine never probes the sync fast path. No extension impl.
        },
    };

    quote! {
        impl #impl_generics ::parrot_api::actor::Actor for #actor_name #ty_generics #where_clause {
            type Context = __parrot_engine::EngineContext<Self>;
            type Config = #config_type;

            fn receive_message<'a>(&'a mut self, msg: ::parrot_api::types::BoxedMessage, ctx: &'a mut Self::Context)
                -> ::parrot_api::types::BoxedFuture<'a, ::parrot_api::types::ActorResult<::parrot_api::types::BoxedMessage>> {
                ::std::boxed::Box::pin(async move {
                    self.handle_message(msg, ctx).await
                })
            }

            #use_async_handler_override

            fn state(&self) -> ::parrot_api::actor::ActorState {
                // Default implementation returns Running
                ::parrot_api::actor::ActorState::Running
            }
        }

        #engine_ext_impl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_actor_options_do_not_reset_engine_or_config() {
        for attr in [
            "#[ParrotActor(engine = 1)]",
            "#[ParrotActor(engine = \"thread\", asyn_handler = true)]",
            "#[ParrotActor(engine = \"thread\")] #[ParrotActor(engine = \"actix\")]",
            "#[ParrotActor(config = \"Vec<\")]",
            "#[ParrotActor(async_handler = \"yes\")]",
            "#[ParrotActor(engine = \"unknown\")]",
            "#[ParrotActor(supervision = \"OneForOne\")]",
            "#[ParrotActor(dispatcher = \"default\")]",
        ] {
            let input = syn::parse_str(&format!("{attr} struct Worker;")).unwrap();
            assert!(expand(input).is_err(), "accepted {attr}");
        }
    }
}
