//! `#[derive(ParrotTypedActor)]` —— M4 静态类型轨的枚举信封生成器。
//!
//! 按属性声明的消息集生成：
//! 1. `enum {Actor}Msg { .. }` —— 枚举信封（每消息一个变体）
//! 2. `enum {Actor}Reply { .. }` —— 回复枚举（每 `<M as Message>::Result` 一个变体）
//! 3. `impl ParrotTypedDispatch for {Actor}` —— match 分派到各
//!    `TypedReceive<M>` impl（零 Any）
//! 4. `impl ParrotMsgVariant<{Actor}> for {M}` —— 每消息的
//!    inject/extract（类型级 `M ↔ Msg`、`Reply ↔ M::Result` 映射）
//!
//! 生成代码只引用 `::parrot_api::*` 规范符号，零引擎依赖（与 M1 的
//! 引擎中立原则一致：引擎绑定发生在 `spawn_typed` 调用点）。
//!
//! ```rust
//! use parrot_api::{Message, ParrotTypedActor};
//! use parrot_api::typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive};
//! use parrot_api::types::{ActorResult, BoxedFuture};
//!
//! pub struct Add(u64);
//! impl Message for Add { type Result = u64; }
//! pub struct Get;
//! impl Message for Get { type Result = u64; }
//!
//! #[derive(ParrotTypedActor)]
//! #[ParrotTypedActor(msgs(Add, Get))]
//! struct Calc { n: u64 }
//!
//! impl TypedReceive<Add> for Calc {
//!     fn receive_typed<'a>(&'a mut self, msg: Add) -> BoxedFuture<'a, ActorResult<u64>> {
//!         Box::pin(async move { self.n += msg.0; Ok(self.n) })
//!     }
//! }
//! impl TypedReceive<Get> for Calc {
//!     fn receive_typed<'a>(&'a mut self, _: Get) -> BoxedFuture<'a, ActorResult<u64>> {
//!         Box::pin(async move { Ok(self.n) })
//!     }
//! }
//!
//! // The protocol compiles independently of any engine.
//! fn check_protocol<A: ParrotTypedDispatch, M: ParrotMsgVariant<A>>() {}
//! check_protocol::<Calc, Add>();
//! check_protocol::<Calc, Get>();
//! ```

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::ext::IdentExt;
use syn::{parse_macro_input, DeriveInput, Ident};

/// 解析 `#[ParrotTypedActor(msgs(A, B, ..))]` 的消息集。
fn parse_options(attrs: &[syn::Attribute]) -> Result<(Vec<Ident>, bool, bool), syn::Error> {
    let mut msgs = Vec::new();
    let mut found = false;
    let mut lifecycle = false;
    let mut local = false;

    for attr in attrs {
        if !attr.path().is_ident("ParrotTypedActor") {
            continue;
        }
        found = true;
        // 形态：msgs(A, B, C)
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("msgs") {
                let content = meta.input.parse::<proc_macro2::Group>()?;
                // content: (A, B, C)
                let inner = content.stream();
                let parser = syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated;
                let list = syn::parse::Parser::parse2(parser, inner)?;
                for m in list {
                    msgs.push(m);
                }
                Ok(())
            } else if meta.path.is_ident("lifecycle") {
                if lifecycle {
                    return Err(meta.error("duplicate lifecycle option"));
                }
                lifecycle = true;
                Ok(())
            } else if meta.path.is_ident("local") {
                if local {
                    return Err(meta.error("duplicate local option"));
                }
                local = true;
                Ok(())
            } else {
                Err(meta.error("unsupported attribute; expected msgs(...), lifecycle or local"))
            }
        })?;
    }

    if !found {
        return Err(syn::Error::new(
            Span::call_site(),
            "#[derive(ParrotTypedActor)] requires #[ParrotTypedActor(msgs(..))] listing the protocol message types",
        ));
    }
    if msgs.is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "#[ParrotTypedActor(msgs(..))] must list at least one message type",
        ));
    }
    Ok((msgs, lifecycle, local))
}

pub(crate) fn derive_typed_actor_impl(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let actor = &input.ident;

    let (msgs, lifecycle, local) = match parse_options(&input.attrs) {
        Ok(m) => m,
        Err(e) => return e.to_compile_error().into(),
    };

    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let msg_enum = Ident::new(&format!("{}Msg", actor.unraw()), actor.span());
    let reply_enum = Ident::new(&format!("{}Reply", actor.unraw()), actor.span());
    let receiver = if local {
        quote! { ::parrot_api::local_typed::LocalTypedReceive }
    } else {
        quote! { ::parrot_api::typed::TypedReceive }
    };

    // 枚举变体：Msg::Add(Add)、Reply::Add(<Add as Message>::Result)
    let msg_variants = msgs.iter().map(|m| quote! { #m(#m) });
    let reply_variants = msgs.iter().map(|m| {
        quote! { #m(<#m as ::parrot_api::message::Message>::Result) }
    });

    // dispatch 分派臂：Msg::Add(m) => self.receive_typed(m).map(Reply::Add)
    let dispatch_arms = msgs.iter().map(|m| {
        quote! {
            #msg_enum::#m(inner) => {
                <Self as #receiver<#m>>::receive_typed(self, inner)
                    .await
                    .map(#reply_enum::#m)
            }
        }
    });

    let context_arms = msgs.iter().map(|m| quote! {
        #msg_enum::#m(inner) => <Self as #receiver<#m>>::receive_typed_with_context(self, inner, context).await.map(#reply_enum::#m)
    });

    let termination_protocols = msgs.iter().map(|m| {
        quote! {
            if ::std::any::TypeId::of::<#m>()
                == ::std::any::TypeId::of::<::parrot_api::context::ActorTerminated>()
            {
                return ::std::option::Option::Some(|event| {
                    ::parrot_api::typed::cast_termination::<#m>(event).map(#msg_enum::#m)
                });
            }
        }
    });

    // Explicit opt-in avoids breaking existing receivers or guessing inherent
    // methods. UFCS preserves trait hygiene even when callers shadow names.
    let lifecycle_hooks = lifecycle.then(|| quote! {
        fn init(&mut self) -> ::parrot_api::types::BoxedFuture<'_, ::parrot_api::types::ActorResult<()>> {
            <Self as ::parrot_api::typed::TypedLifecycle>::init(self)
        }
        fn before_stop(&mut self) -> ::parrot_api::types::BoxedFuture<'_, ::parrot_api::types::ActorResult<()>> {
            <Self as ::parrot_api::typed::TypedLifecycle>::before_stop(self)
        }
        fn init_with_context<'a>(&'a mut self, context: &'a ::parrot_api::typed_context::TypedContext) -> ::parrot_api::types::BoxedFuture<'a, ::parrot_api::types::ActorResult<()>> {
            <Self as ::parrot_api::typed::TypedLifecycle>::init_with_context(self, context)
        }
        fn before_stop_with_context<'a>(&'a mut self, context: &'a ::parrot_api::typed_context::TypedContext) -> ::parrot_api::types::BoxedFuture<'a, ::parrot_api::types::ActorResult<()>> {
            <Self as ::parrot_api::typed::TypedLifecycle>::before_stop_with_context(self, context)
        }
    });

    // ParrotMsgVariant impl（每消息一份）
    let variant_impls = msgs.iter().map(|m| {
        let mapping = if local {
            quote! { ::parrot_api::local_typed::LocalMsgVariant<#m> for #actor #ty_generics }
        } else {
            quote! { ::parrot_api::typed::ParrotMsgVariant<#actor #ty_generics> for #m }
        };
        quote! {
            impl #impl_generics #mapping
            #where_clause
            {
                fn inject(msg: #m) -> #msg_enum {
                    #msg_enum::#m(msg)
                }

                fn extract(reply: #reply_enum) -> ::parrot_api::types::ActorResult<<#m as ::parrot_api::message::Message>::Result> {
                    match reply {
                        #reply_enum::#m(r) => ::std::result::Result::Ok(r),
                        _ => ::std::result::Result::Err(::parrot_api::typed::variant_mismatch_internal()),
                    }
                }
            }
        }
    });

    let dispatch = if local {
        let hooks = lifecycle.then(|| quote! {
            fn init<'a>(&'a mut self, context: &'a ::parrot_api::typed_context::TypedContext) -> ::parrot_api::local_typed::LocalBoxedFuture<'a, ::parrot_api::types::ActorResult<()>> {
                <Self as ::parrot_api::local_typed::LocalTypedLifecycle>::init_with_context(self, context)
            }
            fn before_stop<'a>(&'a mut self, context: &'a ::parrot_api::typed_context::TypedContext) -> ::parrot_api::local_typed::LocalBoxedFuture<'a, ::parrot_api::types::ActorResult<()>> {
                <Self as ::parrot_api::local_typed::LocalTypedLifecycle>::before_stop_with_context(self, context)
            }
        });
        quote! {
            impl #impl_generics ::parrot_api::local_typed::LocalTypedDispatch for #actor #ty_generics #where_clause {
                type Msg = #msg_enum;
                type Reply = #reply_enum;
                #hooks
                fn termination_protocol() -> ::std::option::Option<::parrot_api::typed::TerminationProtocol<Self::Msg>> {
                    #(#termination_protocols)*
                    ::std::option::Option::None
                }
                fn dispatch<'a>(&'a mut self, msg: Self::Msg, context: &'a ::parrot_api::typed_context::TypedContext) -> ::parrot_api::local_typed::LocalBoxedFuture<'a, ::parrot_api::types::ActorResult<Self::Reply>> {
                    ::std::boxed::Box::pin(async move { match msg { #(#context_arms,)* } })
                }
            }
        }
    } else {
        quote! {
        impl #impl_generics ::parrot_api::typed::ParrotTypedDispatch for #actor #ty_generics #where_clause {
            type Msg = #msg_enum;
            type Reply = #reply_enum;

            #lifecycle_hooks

            fn termination_protocol() -> ::std::option::Option<
                ::parrot_api::typed::TerminationProtocol<Self::Msg>
            > {
                #(#termination_protocols)*
                ::std::option::Option::None
            }

            fn dispatch_with_context<'a>(&'a mut self, msg: Self::Msg, context: &'a ::parrot_api::typed_context::TypedContext) -> ::parrot_api::types::BoxedFuture<'a, ::parrot_api::types::ActorResult<Self::Reply>> {
                ::std::boxed::Box::pin(async move { match msg { #(#context_arms,)* } })
            }

            fn dispatch<'a>(
                &'a mut self,
                msg: Self::Msg,
            ) -> ::parrot_api::types::BoxedFuture<'a, ::parrot_api::types::ActorResult<Self::Reply>> {
                ::std::boxed::Box::pin(async move {
                    match msg {
                        #(#dispatch_arms)*
                    }
                })
            }
        }
        }
    };
    quote! {
        /// Generated mailbox protocol; it contains messages, never actor state.
        #[allow(non_camel_case_types)]
        pub enum #msg_enum { #(#msg_variants,)* }
        /// Generated reply protocol.
        #[allow(non_camel_case_types)]
        pub enum #reply_enum { #(#reply_variants,)* }
        #dispatch
        #(#variant_impls)*
    }
    .into()
}
