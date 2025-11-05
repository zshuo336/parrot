//! RemoteMessage derives a wire identity and a codec-specific inventory entry.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, LitStr};

fn explicit_key(attrs: &[syn::Attribute]) -> syn::Result<Option<LitStr>> {
    let mut key = None;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("remote")) {
        let mut found = false;
        attr.parse_nested_meta(|nested| {
            if !nested.path.is_ident("key") {
                return Err(nested.error("unknown remote option; expected key = \"bin:...\" or \"pb:...\""));
            }
            if key.is_some() {
                return Err(nested.error("duplicate remote key"));
            }
            let value: LitStr = nested.value()?.parse()?;
            let text = value.value();
            let body = text.strip_prefix("bin:").or_else(|| text.strip_prefix("pb:"));
            if body.is_none_or(str::is_empty)
                || text.chars().any(|c| c.is_whitespace() || c.is_control())
                || text.len() > u16::MAX as usize
            {
                return Err(syn::Error::new(value.span(), "remote key must have a bin: or pb: prefix and a nonempty name, contain no whitespace/control characters, and fit in 65535 UTF-8 bytes"));
            }
            key = Some(value);
            found = true;
            Ok(())
        })?;
        if !found {
            return Err(syn::Error::new_spanned(
                attr,
                "expected remote(key = \"...\")",
            ));
        }
    }
    Ok(key)
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "RemoteMessage inventory requires a concrete type; wrap each generic instantiation in a non-generic message with its own wire key",
        ));
    }
    let explicit = explicit_key(&input.attrs)?;
    let protobuf = explicit
        .as_ref()
        .is_some_and(|key| key.value().starts_with("pb:"));
    let key = match explicit {
        Some(key) => quote!(#key),
        None => quote!(::core::concat!(
            "bin:",
            ::core::module_path!(),
            "::",
            ::core::stringify!(#name),
            "#v1"
        )),
    };
    let (encode, decode) = if protobuf {
        (
            quote!(::parrot_api::message::protobuf_remote_serialize(typed)),
            quote!(::parrot_api::message::protobuf_remote_deserialize(bytes)?),
        )
    } else {
        (
            quote!(::parrot_api::message::serde_remote_serialize(typed)),
            quote!(::parrot_api::message::serde_remote_deserialize(bytes)?),
        )
    };
    let where_clause = &input.generics.where_clause;
    Ok(quote! {
        impl ::parrot_api::message::RemoteMessage for #name #where_clause {
            const TYPE_KEY: &'static str = #key;
        }

        ::parrot_api::message::inventory::submit! {
            ::parrot_api::message::CodecRegistration {
                type_key: <#name as ::parrot_api::message::RemoteMessage>::TYPE_KEY,
                type_id: ::std::any::TypeId::of::<#name>(),
                encode: |msg: &::parrot_api::types::BoxedMessage| -> ::std::result::Result<::std::vec::Vec<u8>, ::std::string::String> {
                    let typed = msg.downcast_ref::<#name>()
                        .ok_or_else(|| ::core::concat!("downcast failed for ", ::core::stringify!(#name)).to_string())?;
                    #encode
                },
                decode: |bytes: &[u8]| -> ::std::result::Result<::parrot_api::types::BoxedMessage, ::std::string::String> {
                    let typed: #name = #decode;
                    ::std::result::Result::Ok(::std::boxed::Box::new(typed) as ::parrot_api::types::BoxedMessage)
                },
            }
        }
    })
}

pub(crate) fn derive_remote_message_impl(input: TokenStream) -> TokenStream {
    expand(parse_macro_input!(input as DeriveInput))
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_unknown_and_duplicate_options() {
        for attr in [
            "#[remote]",
            "#[remote()]",
            "#[remote = 1]",
            "#[remote(key)]",
            "#[remote(key = 7)]",
            "#[remote(kee = \"bin:x\")]",
            "#[remote(key = \"bin:x\", unknown)]",
            "#[remote(key = \"bin:x\", key = \"bin:y\")]",
            "#[remote(key = \"bin:x\")] #[remote(key = \"bin:y\")]",
            "#[remote(key = \"json:x\")]",
            "#[remote(key = \"bin:\")]",
            "#[remote(key = \"pb:\")]",
            "#[remote(key = \"pb:x y\")]",
            "#[remote(key = \"bin:x\\0y\")]",
        ] {
            let input: DeriveInput = syn::parse_str(&format!("{attr} struct Msg;")).unwrap();
            assert!(expand(input).is_err(), "accepted {attr}");
        }
    }

    #[test]
    fn rejects_generic_inventory_with_actionable_diagnostic() {
        for item in [
            "struct Msg<T>(T);",
            "struct Msg<'a>(&'a str);",
            "struct Msg<const N: usize>([u8; N]);",
        ] {
            let err = expand(syn::parse_str(item).unwrap()).unwrap_err();
            assert!(err.to_string().contains("wrap each generic instantiation"));
        }
    }

    #[test]
    fn accepts_explicit_keys_and_missing_attribute() {
        for attr in [
            "",
            "#[remote(key = \"bin:stable::Msg#v2\")]",
            "#[remote(key = \"pb:pkg.Msg\")]",
        ] {
            assert!(expand(syn::parse_str(&format!("{attr} struct Msg;")).unwrap()).is_ok());
        }
    }
}
