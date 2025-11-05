//! Read a Directory entry through a fresh quorum barrier over mTLS TCP.
use parrot_remote::{
    roles::directory::{client::DirectoryClient, DirQuery},
    tls::TlsConfig,
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{sync::Arc, time::Duration};
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [group, client, cert, key, ca, target, address, node] = args.as_slice() else {
        return Err("usage: directory_read GROUP CLIENT CERT KEY CA TARGET IP:PORT NODE".into());
    };
    let route = NodeAddr::tcp(target, address.parse()?);
    let remote = RemoteActorSystem::new(
        RemoteConfig::tcp(client, None).with_mtls(TlsConfig::from_paths(cert, key, ca)?, []),
        Arc::new(Empty),
    )?;
    let client = DirectoryClient::new(remote.clone(), group)?;
    remote.start().await?;
    let result = client
        .query_linearizable(
            &route,
            &DirQuery::Resolve {
                prefix_or_node: node.clone(),
            },
            Duration::from_secs(15),
        )
        .await;
    let cleanup = remote.shutdown().await;
    let result = result?;
    cleanup?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
