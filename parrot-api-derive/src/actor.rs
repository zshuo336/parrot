use proc_macro::TokenStream;
use proc_macro2::{TokenStream as TokenStream2, Span};
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Attribute, Ident, parse_str, Type};
use darling::{FromMeta, FromAttributes};

/// Actor attribute options for customizing actor behavior
#[derive(Debug, Default, FromMeta)]
pub struct ActorOptions {
    /// Engine to use for actor implementation
    #[darling(default)]
    engine: Option<EngineValue>,
    
    /// Configuration type for actor
    #[darling(default)]
    config: Option<String>,
    
    /// Supervision strategy
    #[darling(default)]
    supervision: Option<String>,
    
    /// Dispatcher type (for future use)
    #[darling(default)]
    dispatcher: Option<String>,
}

/// Represents an engine value that can be a string or an identifier
#[derive(Debug)]
pub enum EngineValue {
    /// Named engine (e.g., "actix", "tokio")
    Named(String),
    /// Identifier reference to an engine constant
    Ident(String),
}

impl Default for EngineValue {
    fn default() -> Self {
        EngineValue::Named("actix".to_string())
    }
}

impl FromMeta for EngineValue {
    fn from_value(value: &syn::Lit) -> darling::Result<Self> {
        match value {
            syn::Lit::Str(lit_str) => {
                let value = lit_str.value();
                Ok(EngineValue::Named(value))
            },
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
            },
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

/// Implementation of the ParrotActor derive macro
pub(crate) fn derive_actor_impl(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    
    // Parse the actor attributes into an ActorOptions struct
    let options: ActorOptions = parse_actor_attrs(&input.attrs);
    
    // Extract actor name
    let actor_name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    
    // Determine the engine to use
    let engine = match options.engine {
        Some(EngineValue::Named(ref name)) => name.clone(),
        Some(EngineValue::Ident(ref ident)) => {
            // Handle constants - here we convert known constants to their string values
            match ident.to_uppercase().as_str() {
                "ACTIX" => "actix".to_string(),
                "TOKIO" => "tokio".to_string(),
                _ => ident.clone(),
            }
        },
        None => "actix".to_string(), // Default to actix engine
    };
    
    // Parse the configuration type
    let config_type = if let Some(ref config_type) = options.config {
        parse_str::<Type>(config_type).unwrap_or_else(|_| parse_str("parrot_api::actor::EmptyConfig").unwrap())
    } else {
        // If configuration type is not specified, use EmptyConfig instead of ()
        parse_str("parrot_api::actor::EmptyConfig").unwrap()
    };
    
    // Generate implementation based on the engine
    let implementation = match engine.as_str() {
        "actix" => generate_actix_implementation(actor_name, &impl_generics, &ty_generics, where_clause, &config_type),
        _ => {
            let error_message = format!("Unsupported engine: {}", engine);
            return syn::Error::new(Span::call_site(), error_message)
                .to_compile_error()
                .into();
        }
    };
    
    implementation.into()
}

/// Generate implementation for actix engine
fn generate_actix_implementation(
    actor_name: &Ident,
    impl_generics: &syn::ImplGenerics,
    ty_generics: &syn::TypeGenerics,
    where_clause: Option<&syn::WhereClause>,
    config_type: &Type,
) -> TokenStream2 {
    quote! {
        impl #impl_generics parrot_api::actor::Actor for #actor_name #ty_generics #where_clause {
            type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
            type Config = #config_type;
            
            fn receive_message<'a>(&'a mut self, msg: parrot_api::types::BoxedMessage, ctx: &'a mut Self::Context) 
                -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>> {
                Box::pin(async move {
                    // on test environment, call the user's handle_message implementation
                    #[cfg(test)]
                    {
                        self.handle_message(msg, ctx).await
                    }
                    
                    #[cfg(not(test))]
                    {
                        // on production environment, not call handle_message
                        Err(parrot_api::errors::ActorError::MessageHandlingError("Not use on actix engine".to_string()))
                    }
                })
            }
            
            // Process an incoming message and produce a response. *use on actix engine*
            fn receive_message_with_engine<'a>(&'a mut self, msg: parrot_api::types::BoxedMessage, ctx: &'a mut Self::Context, engine_ctx: std::ptr::NonNull<dyn std::any::Any>)
                -> Option<parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>> {
                    // Call the user's message handler implementation
                    // This will be implemented separately by the user
                    self.handle_message_engine(msg, ctx, engine_ctx)
            }

            fn state(&self) -> parrot_api::actor::ActorState {
                // Default implementation returns Running
                parrot_api::actor::ActorState::Running
            }
        }
        
        impl #impl_generics parrot::actix::IntoActorBase for #actor_name #ty_generics #where_clause {
            fn into_actor_base(self) -> parrot::actix::ActorBase<Self> {
                parrot::actix::ActorBase::new(self)
            }
        }
    }
} 