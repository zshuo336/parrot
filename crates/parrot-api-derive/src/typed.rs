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
//! 生成代码只引用 `parrot_api::*` 规范符号，零引擎依赖（与 M1 的
//! 引擎中立原则一致：引擎绑定发生在 `spawn_typed` 调用点）。
//!
//! ```ignore
//! #[derive(ParrotTypedActor)]
//! #[ParrotTypedActor(msgs(Add, Get))]
//! struct Calc { n: u64 }
//!
//! impl TypedReceive<Add> for Calc { .. }
//! impl TypedReceive<Get> for Calc { .. }
//!
//! // 生成：enum CalcMsg { Add(Add), Get(Get) }
//! //       enum CalcReply { Add(u64), Get(u64) }
//! //       impl ParrotTypedDispatch for Calc { .. }
//! //       impl ParrotMsgVariant<Calc> for Add { .. }
//! //       impl ParrotMsgVariant<Calc> for Get { .. }
//! ```

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Ident};

/// 解析 `#[ParrotTypedActor(msgs(A, B, ..))]` 的消息集。
fn parse_msg_list(attrs: &[syn::Attribute]) -> Result<Vec<Ident>, syn::Error> {
    let mut msgs = Vec::new();
    let mut found = false;

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
            } else {
                Err(meta.error("unsupported attribute; expected msgs(...)"))
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
    Ok(msgs)
}

pub(crate) fn derive_typed_actor_impl(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let actor = &input.ident;

    let msgs = match parse_msg_list(&input.attrs) {
        Ok(m) => m,
        Err(e) => return e.to_compile_error().into(),
    };

    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let msg_enum = Ident::new(&format!("{}Msg", actor), Span::call_site());
    let reply_enum = Ident::new(&format!("{}Reply", actor), Span::call_site());

    // 枚举变体：Msg::Add(Add)、Reply::Add(<Add as Message>::Result)
    let msg_variants = msgs.iter().map(|m| quote! { #m(#m) });
    let reply_variants = msgs.iter().map(|m| {
        quote! { #m(<#m as parrot_api::message::Message>::Result) }
    });

    // dispatch 分派臂：Msg::Add(m) => self.receive_typed(m).map(Reply::Add)
    let dispatch_arms = msgs.iter().map(|m| {
        quote! {
            #msg_enum::#m(inner) => {
                self.receive_typed(inner)
                    .await
                    .map(#reply_enum::#m)
            }
        }
    });

    // ParrotMsgVariant impl（每消息一份）
    let variant_impls = msgs.iter().map(|m| {
        quote! {
            impl #impl_generics parrot_api::typed::ParrotMsgVariant<#actor #ty_generics> for #m
            #where_clause
            {
                fn inject(msg: Self) -> #msg_enum {
                    #msg_enum::#m(msg)
                }

                fn extract(reply: #reply_enum) -> parrot_api::types::ActorResult<Self::Result> {
                    match reply {
                        #reply_enum::#m(r) => Ok(r),
                        _ => Err(parrot_api::typed::variant_mismatch_internal()),
                    }
                }
            }
        }
    });

    quote! {
        /// 枚举信封（derive 生成）。
        #[allow(non_camel_case_types)]
        pub enum #msg_enum {
            #(#msg_variants,)*
        }

        /// 回复枚举（derive 生成）。
        #[allow(non_camel_case_types)]
        pub enum #reply_enum {
            #(#reply_variants,)*
        }

        impl #impl_generics parrot_api::typed::ParrotTypedDispatch for #actor #ty_generics #where_clause {
            type Msg = #msg_enum;
            type Reply = #reply_enum;

            fn dispatch<'a>(
                &'a mut self,
                msg: Self::Msg,
            ) -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<Self::Reply>> {
                Box::pin(async move {
                    match msg {
                        #(#dispatch_arms)*
                    }
                })
            }
        }

        #(#variant_impls)*
    }
    .into()
}
