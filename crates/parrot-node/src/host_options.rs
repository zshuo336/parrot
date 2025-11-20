/// Explicit process authority, separate from business config and Admin ACLs.
/// A running deployment owner retains an immutable copy. Defaults deny all
/// native artifacts, guest messaging and guest services; manifests grant none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostOptions {
    #[cfg(feature = "dylib")]
    pub native: crate::NativeTrust,
    #[cfg(feature = "dylib")]
    pub native_messaging: parrot_app::host::dylib::NativeMessagingPolicy,
    #[cfg(feature = "dylib")]
    pub native_services: parrot_app::host::dylib::NativeServicePolicy,
    #[cfg(feature = "wasm")]
    pub wasm_messaging: parrot_app::host::wasm::WasmMessagingPolicy,
    #[cfg(feature = "wasm")]
    pub wasm_services: parrot_app::host::wasm::WasmServicePolicy,
}
