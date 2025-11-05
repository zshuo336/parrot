//! Submit one stable, idempotent Directory request over mTLS TCP.
use parrot_remote::{
    roles::directory::{client::DirectoryClient, DirCmd, DirectoryRequestStatus},
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
    let [group, client, cert, key, ca, target, address, id, node, endpoint] = args.as_slice()
    else {
        return Err("usage: directory_client GROUP CLIENT CERT KEY CA TARGET IP:PORT REQUEST_ID NODE ENDPOINT".into());
    };
    let remote = RemoteActorSystem::new(
        RemoteConfig::tcp(client, None).with_mtls(TlsConfig::from_paths(cert, key, ca)?, []),
        Arc::new(Empty),
    )?;
    remote.start().await?;
    let result = DirectoryClient::new(remote.clone(), group)?
        .submit_and_wait(
            &NodeAddr::tcp(target, address.parse()?),
            id,
            &DirCmd::Upsert {
                node: node.clone(),
                endpoints: vec![endpoint.clone()],
            },
            Duration::from_secs(15),
        )
        .await;
    let cleanup = remote.shutdown().await;
    let result = result?;
    cleanup?;
    match result {
        DirectoryRequestStatus::Applied {
            index,
            term,
            revision,
        } => println!("applied index={index} term={term} revision={revision}"),
        other => return Err(format!("request was not applied: {other:?}").into()),
    }
    Ok(())
}
