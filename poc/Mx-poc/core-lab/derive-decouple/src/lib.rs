//! Derive runtime-neutral dispatch, preserving generics and optional lifecycle.
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, parse_quote, DeriveInput, LitStr, Path};

/// Generate ErasedActor and spawn_on for concrete and dyn EngineRuntime.
/// `#[neutral(crate = "renamed_api", lifecycle, binding = "counter")]` selects
/// a contract crate path, opts into Lifecycle, and adds an explicit owned
/// register_binding(registry) constructor for Default actors.
#[proc_macro_derive(ParrotActorNeutral, attributes(neutral))]
pub fn derive_parrot_actor_neutral(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}
fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let mut api: Path = parse_quote!(::parrot_api);
    let mut lifecycle = false;
    let mut binding = None;
    let mut crate_seen = false;
    for attr in &input.attrs {
        if !attr.path().is_ident("neutral") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("crate") {
                if crate_seen {
                    return Err(meta.error("duplicate contract crate path"));
                }
                crate_seen = true;
                let value: LitStr = meta.value()?.parse()?;
                api = value.parse()?;
                Ok(())
            } else if meta.path.is_ident("lifecycle") {
                if lifecycle {
                    return Err(meta.error("duplicate lifecycle option"));
                }
                lifecycle = true;
                Ok(())
            } else if meta.path.is_ident("binding") {
                if binding.is_some() {
                    return Err(meta.error("duplicate binding name"));
                }
                let value: LitStr = meta.value()?.parse()?;
                if value.value().is_empty() {
                    return Err(meta.error("binding name must not be empty"));
                }
                binding = Some(value);
                Ok(())
            } else {
                Err(meta.error("expected crate, lifecycle, or binding"))
            }
        })?;
    }
    let name = &input.ident;
    let (_, type_args, _) = input.generics.split_for_impl();
    let mut generics = input.generics.clone();
    generics
        .make_where_clause()
        .predicates
        .push(parse_quote!(#name #type_args: #api::ActorBehaviour));
    if lifecycle {
        generics
            .make_where_clause()
            .predicates
            .push(parse_quote!(#name #type_args: #api::Lifecycle));
    }
    let (impl_args, _, where_clause) = generics.split_for_impl();
    let mut engine_name = "__ParrotEngine".to_string();
    while input.generics.params.iter().any(|p| match p {
        syn::GenericParam::Type(t) => t.ident == engine_name,
        syn::GenericParam::Const(c) => c.ident == engine_name,
        _ => false,
    }) {
        engine_name.push('_');
    }
    let engine = format_ident!("{}", engine_name);
    let hooks = if lifecycle {
        quote! {
            fn pre_start(&mut self)->::std::result::Result<(),::std::string::String>{#api::Lifecycle::pre_start(self)}
            fn post_stop(&mut self){#api::Lifecycle::post_stop(self);}
        }
    } else {
        quote! {}
    };
    let registration = if let Some(binding) = binding {
        quote! {
            pub fn register_binding(registry:&#api::BindingRegistry)->::std::result::Result<#api::Registration,::std::string::String>
            where Self: ::std::default::Default {
                registry.register(#binding,||Ok(::std::boxed::Box::new(<Self as ::std::default::Default>::default()) as ::std::boxed::Box<dyn #api::ErasedActor>))
            }
        }
    } else {
        quote! {}
    };
    Ok(quote! {
        impl #impl_args #api::ErasedActor for #name #type_args #where_clause {
            fn receive(&mut self,msg:#api::BoxedMessage)->#api::BoxedResult{#api::ActorBehaviour::receive(self,msg)}
            #hooks
        }
        impl #impl_args #name #type_args #where_clause {
            pub fn spawn_on<#engine:#api::EngineRuntime+?Sized>(self,engine:&#engine,path:&str)->::std::result::Result<#api::BoxedActorRef,::std::string::String>{
                #api::EngineRuntime::spawn_erased(engine,::std::boxed::Box::new(self),path)
            }
            #registration
        }
    })
}
