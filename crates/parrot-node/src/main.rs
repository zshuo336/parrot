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
    let native_digests = parrot_node::configuration::native_digest_requests(std::env::var_os(
        "PARROT_TRUST_NATIVE_SHA256",
    ))?;
    let settings = NodeSettings::load()?;
    if std::env::var("PARROT_HEALTHCHECK").as_deref() == Ok("1") {
        return runtime::healthcheck(&settings).await;
    }
    let mut signal = ShutdownSignal::install()?;
    #[cfg(feature = "dylib")]
    let node = {
        // SAFETY: this executable treats the explicit operator environment list
        // as acceptance of the full documented native ABI/dependency contract.
        // Safe library startup and remote commands never create this authority.
        let trust = unsafe { parrot_node::NativeTrust::authorize(&native_digests) }?;
        RunningNode::start_with_native_trust(&settings, &trust).await?
    };
    #[cfg(not(feature = "dylib"))]
    let node = {
        debug_assert!(native_digests.is_empty());
        RunningNode::start(&settings).await?
    };
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
