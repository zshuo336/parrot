// Common utility functions and types that can be shared across different macros

/// Helper function for error handling in derived macros
pub fn format_error_span<T: quote::ToTokens>(
    item: &T, 
    message: &str
) -> proc_macro2::TokenStream {
    syn::Error::new_spanned(item, message)
        .to_compile_error()
}

/// Convert a syn::Error to a TokenStream that can be returned from a proc_macro function
pub fn to_compile_error(error: syn::Error) -> proc_macro::TokenStream {
    error.to_compile_error().into()
} 