//! Standard single-node process. See parrot_node for the configuration contract.
use parrot_node::{
    configuration::NodeSettings,
    runtime::{self, NodeError, RunningNode, ShutdownSignal},
};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("[parrot-node] {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
async fn run() -> Result<(), NodeError> {
    let native_service_policy_path = parrot_node::configuration::native_service_policy_request(
        std::env::var_os("PARROT_NATIVE_SERVICE_POLICY"),
    )?;
    let native_policy_path = parrot_node::configuration::native_messaging_policy_request(
        std::env::var_os("PARROT_NATIVE_MESSAGING_POLICY"),
    )?;
    let policy_path = parrot_node::configuration::wasm_messaging_policy_request(std::env::var_os(
        "PARROT_WASM_MESSAGING_POLICY",
    ))?;
    let native_digests = parrot_node::configuration::native_digest_requests(std::env::var_os(
        "PARROT_TRUST_NATIVE_SHA256",
    ))?;
    let service_policy_path = parrot_node::configuration::wasm_service_policy_request(
        std::env::var_os("PARROT_WASM_SERVICE_POLICY"),
    )?;
    let settings = NodeSettings::load()?;
    #[cfg(feature = "dylib")]
    let native_messaging = match native_policy_path {
        Some(path) => parrot_app::host::dylib::NativeMessagingPolicy::load_file(path).await?,
        None => Default::default(),
    };
    #[cfg(feature = "dylib")]
    let native_services = match native_service_policy_path {
        Some(path) => parrot_app::host::dylib::NativeServicePolicy::load_file(path).await?,
        None => Default::default(),
    };
    #[cfg(not(feature = "dylib"))]
    debug_assert!(native_service_policy_path.is_none());
    #[cfg(not(feature = "dylib"))]
    debug_assert!(native_policy_path.is_none());
    #[cfg(feature = "wasm")]
    let wasm_messaging = match policy_path {
        Some(path) => parrot_app::host::wasm::WasmMessagingPolicy::load_file(path).await?,
        None => Default::default(),
    };
    #[cfg(not(feature = "wasm"))]
    debug_assert!(policy_path.is_none());
    #[cfg(feature = "wasm")]
    let wasm_services = match service_policy_path {
        Some(path) => parrot_app::host::wasm::WasmServicePolicy::load_file(path).await?,
        None => Default::default(),
    };
    #[cfg(not(feature = "wasm"))]
    debug_assert!(service_policy_path.is_none());
    if std::env::var("PARROT_HEALTHCHECK").as_deref() == Ok("1") {
        return runtime::healthcheck(&settings).await;
    }
    let mut signal = ShutdownSignal::install()?;
    let options = parrot_node::HostOptions {
        #[cfg(feature = "dylib")]
        native_messaging,
        #[cfg(feature = "dylib")]
        native_services,
        #[cfg(feature = "wasm")]
        wasm_messaging,
        #[cfg(feature = "wasm")]
        wasm_services,
        #[cfg(feature = "dylib")]
        native: {
            // SAFETY: this executable treats the explicit operator environment list
            // as acceptance of the full documented native ABI/dependency contract.
            // Safe library startup and remote commands never create this authority.
            unsafe { parrot_node::NativeTrust::authorize(&native_digests) }?
        },
    };
    #[cfg(not(feature = "dylib"))]
    debug_assert!(native_digests.is_empty());
    let node = RunningNode::start_with_host_options(&settings, &options).await?;
    let result = async {
        let endpoint = node.endpoint()?;
        if let Some(path) = &settings.ready_file {
            publish_readiness(path, &endpoint)?;
        }
        use std::io::Write;
        writeln!(std::io::stdout(), "PARROT_NODE_READY={endpoint}")?;
        std::io::stdout().flush()?;
        tokio::select! {
            biased;
            _ = signal.wait() => {},
            error = node.wait_directory_failure() => return Err(error),
        }
        Ok::<(), NodeError>(())
    }
    .await;
    let cleanup = node.shutdown().await;
    result?;
    cleanup
}

// A polling orchestrator must see either the previous endpoint or the complete
// new endpoint. The file is an address record, not a liveness signal.
fn publish_readiness(path: &std::path::Path, endpoint: &str) -> Result<(), NodeError> {
    use std::io::Write;
    let name = path
        .file_name()
        .ok_or("readiness path requires a filename")?;
    let mut temporary_name = name.to_os_string();
    temporary_name.push(format!(".{}.tmp", std::process::id()));
    let temporary = path.with_file_name(temporary_name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(endpoint.as_bytes())?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(Into::into)
}
