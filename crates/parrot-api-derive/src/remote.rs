//! 职责：`#[derive(RemoteMessage)]`——生成 RemoteMessage trait 实现与
//! inventory 自注册（DEV_01 §3.4 / 05 §3.1）。
//!
//! 生成物：
//!   - `impl RemoteMessage for T { const TYPE_KEY: ... }`
//!   - `inventory::submit! { CodecRegistration { ... } }`（bincode 包装）
//!
//! 属性：`#[remote(key = "pb:pkg.Msg")]` 显式覆盖键（跨语言 pb 栈或
//! 布局升版 `#v2` 用，06 I1）。

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, DeriveInput, Meta};

/// 从 `#[remote(key = "...")]` 提取显式键；缺省 `bin:{crate}::{Type}#v1`。
fn explicit_key(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("remote") {
            continue;
        }
        let Meta::List(ml) = &attr.meta else {
            continue;
        };
        let mut key = None;
        let parsed = ml.parse_nested_meta(|nested| {
            if nested.path.is_ident("key") {
                let v = nested.value()?;
                let s: syn::LitStr = v.parse()?;
                key = Some(s.value());
            }
            Ok(())
        });
        if parsed.is_err() {
            continue;
        }
        if let Some(k) = key {
            return Some(k);
        }
    }
    None
}

pub(crate) fn derive_remote_message_impl(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    // 宏展开处的 crate 名（运行时经 env!("CARGO_PKG_NAME") 不可得——用模块路径近似：
    // 正式采用 std::module_path! 不可行（宏卫生），此处用 Cargo 包名注入）。
    let crate_name = std::env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "crate".into());
    let key =
        explicit_key(&input.attrs).unwrap_or_else(|| format!("bin:{}::{}#v1", crate_name, name));

    // bincode 包装函数（类型擦除边界）：encode downcast 后 serde 序列化
    let key_lit = syn::LitStr::new(&key, proc_macro2::Span::call_site());
    let _ = format_ident!("__parrot_remote_{}", name);

    let expanded = quote! {
        impl ::parrot_api::message::RemoteMessage for #name {
            const TYPE_KEY: &'static str = #key_lit;
        }

        ::parrot_api::message::inventory::submit! {
            ::parrot_api::message::CodecRegistration {
                type_key: <#name as ::parrot_api::message::RemoteMessage>::TYPE_KEY,
                type_id: ::std::any::TypeId::of::<#name>(),
                encode: |msg: &::parrot_api::types::BoxedMessage| -> Result<Vec<u8>, String> {
                    let typed = msg.downcast_ref::<#name>()
                        .ok_or_else(|| concat!("downcast failed for ", stringify!(#name)).to_string())?;
                    ::parrot_api::message::serde_remote_serialize(typed)
                },
                decode: |bytes: &[u8]| -> Result<::parrot_api::types::BoxedMessage, String> {
                    let typed: #name = ::parrot_api::message::serde_remote_deserialize(bytes)?;
                    Ok(Box::new(typed) as ::parrot_api::types::BoxedMessage)
                },
            }
        }
    };
    TokenStream::from(expanded)
}
