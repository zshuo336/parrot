//! Real mTLS/TCP Directory process; run one instance per fixed voting member.
use parrot_remote::{
    raft::Role,
    roles::directory::{network::NetworkDirectory, DirCmd, DirQuery},
    tls::TlsConfig,
    LocalLookup, NodeAddr, RemoteConfig,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 8 {
        return Err(
            "usage: network_directory DIR GROUP NODE BIND CERT KEY CA SECONDS [PEER@IP:PORT ...]"
                .into(),
        );
    }
    let directory = PathBuf::from(&args[0]);
    let bind = args[3].parse()?;
    let seconds: u64 = args[7].parse()?;
    if !(5..=3600).contains(&seconds) {
        return Err("SECONDS must be 5..3600".into());
    }
    let peers = args[8..]
        .iter()
        .map(|s| {
            let (id, address) = s.split_once('@').ok_or("expected PEER@IP:PORT")?;
            Ok(NodeAddr::tcp(id, address.parse()?))
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    let credentials = TlsConfig {
        cert_pem: std::fs::read(&args[4])?,
        key_pem: std::fs::read(&args[5])?,
        ca_pem: std::fs::read(&args[6])?,
    };
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&directory) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let mut owner = NetworkDirectory::start(
        directory,
        &args[1],
        RemoteConfig::tcp(&args[2], Some(bind)).with_mtls(credentials, []),
        peers,
        Arc::new(Empty),
    )
    .await?;
    let handle = owner.handle();
    let advertised = owner.remote().local_addr().ok_or("no listener")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut receipt = None;
    println!(
        "listening={} recovered={:?}",
        owner.remote().local_addr().ok_or("no listener")?,
        handle.observation()
    );
    while tokio::time::Instant::now() < deadline {
        let view = handle.observation();
        if view.closed {
            return Err(format!("Directory closed: {:?}", view.terminal_error).into());
        }
        if receipt.is_none() && view.role == Role::Leader {
            receipt = Some(
                handle
                    .propose(&DirCmd::Upsert {
                        node: "example-service".into(),
                        endpoints: vec![format!("tcp://{advertised}")],
                    })
                    .await?,
            );
            println!("local proposal accepted; still awaiting quorum");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if let Some(receipt) = &receipt {
        println!("proposal={:?}", handle.proposal_status(receipt).await?);
    }
    let entry = handle
        .query_local(&DirQuery::Resolve {
            prefix_or_node: "example-service".into(),
        })
        .await?;
    println!("local applied view={entry:?}");
    owner.close().await?;
    if entry.is_none() {
        return Err("no committed example entry observed before deadline".into());
    }
    Ok(())
}
