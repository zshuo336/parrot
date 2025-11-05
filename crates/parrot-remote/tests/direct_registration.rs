//! Registration observes physical links, independently of route knowledge.
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::sync::Arc;

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
async fn node(id: &str) -> Arc<RemoteActorSystem> {
    let system = RemoteActorSystem::new(
        RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Empty),
    )
    .unwrap();
    system.start().await.unwrap();
    system
}

#[tokio::test]
async fn known_addresses_and_other_identities_are_not_direct_registration() {
    let client = node("registration-client").await;
    let other = node("other").await;
    client
        .nodes
        .add_seed(NodeAddr::tcp("wanted", other.local_addr().unwrap()));
    client
        .connect(&NodeAddr::tcp("other", other.local_addr().unwrap()))
        .await
        .unwrap();
    assert!(client.nodes.get("wanted").is_some());
    assert_eq!(client.links_snapshot().await.len(), 1);
    assert_eq!(
        client.missing_direct_connections(&["wanted", "other"]),
        ["wanted"]
    );
    assert!(client.missing_direct_connections(&[]).is_empty());
    client.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
}

#[tokio::test]
async fn closed_sender_is_missing_before_asynchronous_link_retirement() {
    let client = node("closed-client").await;
    let wanted = node("wanted").await;
    let address = NodeAddr::tcp("wanted", wanted.local_addr().unwrap());
    client.nodes.add_seed(address.clone());
    client.connect(&address).await.unwrap();
    assert!(client.missing_direct_connections(&["wanted"]).is_empty());
    let links = client.links_snapshot().await;
    links[0].1.close();
    // No yield between closing the sender and checking readiness: the cleanup
    // task has not removed the physical link entry yet on this current thread.
    assert_eq!(client.missing_direct_connections(&["wanted"]), ["wanted"]);
    assert!(client.nodes.get("wanted").is_some());
    client.shutdown().await.unwrap();
    wanted.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_live_replacement_still_satisfies_registration() {
    let client = node("replacement-client").await;
    let wanted = node("wanted").await;
    let address = NodeAddr::tcp("wanted", wanted.local_addr().unwrap());
    client.connect(&address).await.unwrap();
    client.connect(&address).await.unwrap();
    let links = client.links_snapshot().await;
    assert_eq!(links.len(), 2);
    links[0].1.close();
    assert!(client
        .missing_direct_connections(&["wanted", "wanted"])
        .is_empty());
    links[1].1.close();
    assert_eq!(client.missing_direct_connections(&["wanted"]), ["wanted"]);
    client.shutdown().await.unwrap();
    assert_eq!(client.missing_direct_connections(&["wanted"]), ["wanted"]);
    wanted.shutdown().await.unwrap();
}
