//! The executable and integration tests use the same node assembly path.
use crate::{
    configuration::NodeSettings, FacadeLookup, GatewayAdapter, NodeComponentExecutor, NodeState,
    NODE,
};
use parrot::{
    system::ParrotActorSystem,
    thread::{config::ThreadActorSystemConfig, system::ThreadActorSystem},
};
use parrot_api::system::ActorSystemConfig;
use parrot_remote::roles::directory::network::{
    DirectoryHandle, DirectoryTiming, NetworkDirectory,
};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

pub type NodeError = Box<dyn std::error::Error + Send + Sync>;
pub struct RunningNode {
    pub thread: Arc<ThreadActorSystem>,
    pub remote: Arc<RemoteActorSystem>,
    directory: Option<tokio::sync::Mutex<NetworkDirectory>>,
    directory_handle: Option<DirectoryHandle>,
}
impl RunningNode {
    /// A process hosts exactly one node (the Props inventory uses NODE).
    /// Call shutdown explicitly to await actor hooks and scheduler teardown.
    pub async fn start(settings: &NodeSettings) -> Result<Self, NodeError> {
        Self::start_inner(
            settings,
            #[cfg(feature = "dylib")]
            &crate::NativeTrust::default(),
        )
        .await
    }
    /// Start with explicit native execution authority. Ordinary NodeSettings and
    /// remote Admin permissions cannot create or expand this trust grant.
    #[cfg(feature = "dylib")]
    pub async fn start_with_native_trust(
        settings: &NodeSettings,
        native: &crate::NativeTrust,
    ) -> Result<Self, NodeError> {
        Self::start_inner(settings, native).await
    }
    async fn start_inner(
        settings: &NodeSettings,
        #[cfg(feature = "dylib")] native: &crate::NativeTrust,
    ) -> Result<Self, NodeError> {
        settings.validate()?;
        // Resolve discovery and fixed voting routes independently, under one
        // startup deadline. Multi-address DNS does not add voting members.
        let (mut cfg, peers) = tokio::time::timeout(Duration::from_secs(5), async {
            let ordinary =
                RemoteConfig::from_resolved_async(&settings.resolved, Duration::from_secs(5));
            let directory = async {
                if let Some(directory) = &settings.resolved.remote.directory {
                    let mut resolved = settings.resolved.clone();
                    resolved.remote.node.seeds = directory.peers.clone();
                    let config =
                        RemoteConfig::from_resolved_async(&resolved, Duration::from_secs(5))
                            .await?;
                    Ok::<_, parrot_config::ConfigError>(config.seeds)
                } else {
                    Ok(Vec::new())
                }
            };
            tokio::try_join!(ordinary, directory)
        })
        .await??;
        cfg.extra_caps |= parrot_remote::handshake::caps::ARTIFACTS;
        #[cfg(feature = "dylib")]
        if !native.is_empty() {
            cfg.extra_caps |= parrot_remote::handshake::caps::DYLIB;
        }
        #[cfg(feature = "wasm")]
        {
            cfg.extra_caps |= parrot_remote::handshake::caps::WASM;
        }
        let directory = if let Some(directory) = &settings.resolved.remote.directory {
            Some(
                NetworkDirectory::prepare_with_timing(
                    &directory.storage_dir,
                    &directory.group,
                    cfg.clone(),
                    peers,
                    Arc::new(FacadeLookup),
                    DirectoryTiming {
                        election_ms: directory.election_ms,
                        heartbeat_ms: directory.heartbeat_ms,
                        rpc_timeout_ms: directory.rpc_timeout_ms,
                    },
                )
                .await?,
            )
        } else {
            None
        };
        let remote = match &directory {
            Some(owner) => owner.remote().clone(),
            None => RemoteActorSystem::new(cfg, Arc::new(FacadeLookup))?,
        };
        let facade = Arc::new(ParrotActorSystem::new(ActorSystemConfig::default()).await?);
        let thread =
            ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&settings.resolved));
        let node = Self {
            thread,
            remote,
            directory_handle: directory.as_ref().map(NetworkDirectory::handle),
            directory: directory.map(tokio::sync::Mutex::new),
        };
        let result: Result<(), NodeError> = async {
            facade
                .register_thread_system("eng".into(), node.thread.clone(), true)
                .await?;
            NODE.set(NodeState {
                facade: facade.clone(),
                ts: node.thread.clone(),
            })
            .map_err(|_| "only one node may be started in this process")?;
            let executor = NodeComponentExecutor::for_running_node(
                node.thread.clone(),
                settings.wasm_epoch_ticks,
                #[cfg(feature = "dylib")]
                native,
            )?;
            for name in &settings.actors {
                crate::spawn_builtin(&node.thread, name).await?;
            }
            if let Some(handle) = node.directory() {
                crate::directory::install(&node.thread, handle).await?;
            }
            node.remote.set_component_executor(Some(Arc::new(executor)));
            facade
                .register_remote_gateway(Arc::new(GatewayAdapter(node.remote.gateway())))
                .await?;
            if let Some(directory) = &node.directory {
                directory.lock().await.start_transport().await?;
            } else {
                node.remote.start().await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            if let Err(cleanup) = node.shutdown().await {
                return Err(format!("{error}; startup cleanup: {cleanup}").into());
            }
            return Err(error);
        }
        Ok(node)
    }
    /// The local Directory application API, when this node owns a group.
    pub fn directory(&self) -> Option<DirectoryHandle> {
        self.directory_handle.clone()
    }
    /// Standard processes must stop advertising readiness after a disk owner fails.
    pub async fn wait_directory_failure(&self) -> NodeError {
        if let Some(handle) = &self.directory_handle {
            let state = handle.wait_closed().await;
            format!("Directory owner terminated: {:?}", state.terminal_error).into()
        } else {
            std::future::pending().await
        }
    }
    pub fn endpoint(&self) -> Result<String, NodeError> {
        if self.remote.config.scheme == "mem" {
            return Ok(format!("mem://{}", self.remote.config.node_id));
        }
        self.remote
            .local_addr()
            .map(|addr| addr.to_string())
            .ok_or_else(|| "node has no listening address".into())
    }
    pub async fn shutdown(&self) -> Result<(), NodeError> {
        // Stop network admission before stopping actors; always attempt both.
        let remote: Result<(), NodeError> = if let Some(directory) = &self.directory {
            directory
                .lock()
                .await
                .close()
                .await
                .map_err(|e| Box::new(e) as NodeError)
        } else {
            self.remote
                .shutdown()
                .await
                .map_err(|e| Box::new(e) as NodeError)
        };
        let local = self.thread.shutdown_internal().await;
        match (remote, local) {
            (Ok(()), Ok(())) => Ok(()),
            (remote, local) => {
                Err(format!("node shutdown: remote={remote:?}, thread={local:?}").into())
            }
        }
    }
}

struct EmptyLookup;
#[async_trait::async_trait]
impl LocalLookup for EmptyLookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}

/// Prove a Parrot handshake with the configured identity, not just a TCP backlog.
/// This does not assert application/actor health or cluster convergence.
pub async fn healthcheck(settings: &NodeSettings) -> Result<(), NodeError> {
    let node = &settings.resolved.remote.node;
    if node.scheme == "mem" {
        return Err("mem transport cannot be probed across processes".into());
    }
    let mut address: SocketAddr = node.bind.as_deref().ok_or("missing node bind")?.parse()?;
    if address.port() == 0 {
        let path = settings
            .ready_file
            .as_ref()
            .ok_or("bind port 0 healthcheck requires PARROT_READY_FILE")?;
        let file = tokio::fs::read_to_string(path).await?;
        address = file.trim().parse()?;
        if address.port() == 0 {
            return Err("readiness file contains port 0".into());
        }
    }
    address = loopback_target(address);
    let id = node.node_id.as_deref().ok_or("missing node identity")?;
    let bind = SocketAddr::new(
        if address.is_ipv6() {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        } else {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        },
        0,
    );
    // Periodic process probes must not add a new peer identity on every run.
    // Hashing bounds the identity length even for a maximum-length node name.
    use sha2::{Digest, Sha256};
    let probe_id = format!("health-{}", hex::encode(Sha256::digest(id.as_bytes())));
    let mut probe_config = settings.resolved.clone();
    // A process-local probe can use the node's own private key. Its HELLO must
    // carry that certificate identity; do not invent a name that fails CN binding.
    probe_config.remote.node.node_id = Some(
        if matches!(
            probe_config.remote.security,
            parrot_config::RemoteSecurityResolved::MutualTls { .. }
        ) {
            id.to_owned()
        } else {
            probe_id
        },
    );
    probe_config.remote.node.bind = if node.scheme == "quic" {
        Some(bind.to_string())
    } else {
        None
    };
    probe_config.remote.node.seeds.clear();
    let cfg = RemoteConfig::from_resolved(&probe_config)?;
    let probe = RemoteActorSystem::new(cfg, Arc::new(EmptyLookup))?;
    let result: Result<(), NodeError> = async {
        tokio::time::timeout(
            Duration::from_secs(2),
            probe.connect(&NodeAddr {
                node_id: id.into(),
                identity_is_explicit: true,
                scheme: node.scheme.clone(),
                addr: address,
            }),
        )
        .await??;
        if !probe
            .links_snapshot()
            .await
            .iter()
            .any(|(peer, _, _)| peer == id)
        {
            return Err("healthcheck peer identity does not match configured node".into());
        }
        Ok(())
    }
    .await;
    let cleanup = probe.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}
fn loopback_target(mut address: SocketAddr) -> SocketAddr {
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv6() {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        } else {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        });
    }
    address
}

/// Install signal handlers before readiness is published.
pub struct ShutdownSignal {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(windows)]
    interrupt: tokio::signal::windows::CtrlC,
}
impl ShutdownSignal {
    pub fn install() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            term: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(unix)]
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            #[cfg(windows)]
            interrupt: tokio::signal::windows::ctrl_c()?,
        })
    }
    pub async fn wait(&mut self) {
        #[cfg(unix)]
        tokio::select! { _ = self.term.recv() => {}, _ = self.interrupt.recv() => {} }
        #[cfg(windows)]
        self.interrupt.recv().await;
        #[cfg(not(any(unix, windows)))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unspecified_probe_preserves_address_family() {
        assert_eq!(
            loopback_target("[::]:9801".parse().unwrap()),
            "[::1]:9801".parse().unwrap()
        );
        assert_eq!(
            loopback_target("0.0.0.0:9801".parse().unwrap()),
            "127.0.0.1:9801".parse().unwrap()
        );
    }
}
