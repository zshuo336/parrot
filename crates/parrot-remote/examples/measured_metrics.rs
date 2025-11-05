//! Finite real-transport example: measured zero differs from an unavailable field.
use parrot_remote::{metrics::field, LocalLookup, RemoteActorSystem, RemoteConfig};
use std::sync::Arc;
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = RemoteActorSystem::new(RemoteConfig::mem("metrics-reader"), Arc::new(Empty))?;
    let b = RemoteActorSystem::new(RemoteConfig::mem("metrics-server"), Arc::new(Empty))?;
    let result = async {
        a.start().await?;
        b.start().await?;
        a.connect_mem_pair(&b).await?;
        let sample = a.measured_metrics("metrics-server").await?;
        assert_eq!(sample.value(field::CONNECTIONS), Some(1));
        assert_eq!(sample.value(field::ASKS_RX), Some(0));
        assert_eq!(sample.value(field::COMPONENTS), None);
        println!(
            "epoch={} connections=1 asks=0 components=unavailable",
            sample.epoch
        );
        Ok::<_, parrot_remote::RemoteError>(())
    }
    .await;
    let a_stopped = a.shutdown().await;
    let b_stopped = b.shutdown().await;
    result?;
    a_stopped?;
    b_stopped?;
    Ok(())
}
