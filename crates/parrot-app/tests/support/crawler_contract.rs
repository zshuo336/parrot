use parrot_api::types::{BoxedActorRef, BoxedMessage};
use std::time::Duration;

macro_rules! wire {
    ($name:ident, $key:literal) => {
        #[derive(Debug)]
        pub struct $name(pub Vec<u8>);
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$name>(),
                encode: |message: &BoxedMessage| Ok(message.downcast_ref::<$name>().ok_or($key)?.0.clone()),
                decode: |bytes: &[u8]| Ok(Box::new($name(bytes.to_vec())) as BoxedMessage),
            }
        }
    };
}
wire!(FrontierPush, "bin:crawl/FrontierPush");
wire!(FrontierAck, "bin:crawl/FrontierAck");
wire!(FrontierNext, "bin:crawl/FrontierNext");
wire!(FrontierBatch, "bin:crawl/FrontierBatch");
wire!(IndexPage, "bin:crawl/IndexPage");
wire!(IndexAck, "bin:crawl/IndexAck");
wire!(IndexStats, "bin:crawl/IndexStats");
wire!(IndexStatsR, "bin:crawl/IndexStatsR");
wire!(IndexTerms, "bin:crawl/IndexTerms");
wire!(Search, "bin:crawl/Search");
wire!(SearchResult, "bin:crawl/SearchResult");

async fn ask(actor: &BoxedActorRef, message: BoxedMessage) -> BoxedMessage {
    actor
        .send_with_timeout(message, Some(Duration::from_secs(5)))
        .await
        .expect("component business reply")
}
fn bytes_with_length(bytes: &[u8]) -> Vec<u8> {
    [(bytes.len() as u32).to_le_bytes().as_slice(), bytes].concat()
}
pub async fn frontier(actor: &BoxedActorRef) {
    let payload = [
        1u32.to_le_bytes().to_vec(),
        7u64.to_le_bytes().to_vec(),
        bytes_with_length(b"https://parrot.dev/"),
        0u16.to_le_bytes().to_vec(),
    ]
    .concat();
    let reply = ask(actor, Box::new(FrontierPush(payload))).await;
    assert_eq!(
        reply.downcast_ref::<FrontierAck>().unwrap().0,
        1u32.to_le_bytes()
    );
}
pub async fn ray_index(actor: &BoxedActorRef) {
    let payload = [
        1u32.to_le_bytes().to_vec(),
        7u64.to_le_bytes().to_vec(),
        bytes_with_length(b"parrot parrot actor"),
    ]
    .concat();
    let reply = ask(actor, Box::new(IndexPage(payload))).await;
    assert_eq!(
        reply.downcast_ref::<IndexAck>().unwrap().0,
        [1u32.to_le_bytes(), 3u32.to_le_bytes()].concat()
    );
    let reply = ask(actor, Box::new(IndexStats(vec![]))).await;
    assert_eq!(
        reply.downcast_ref::<IndexStatsR>().unwrap().0,
        [2u32.to_le_bytes().to_vec(), 2u64.to_le_bytes().to_vec()].concat()
    );
}
pub async fn jvm_search(actor: &BoxedActorRef) {
    let payload = [
        1u32.to_le_bytes().to_vec(),
        bytes_with_length(b"parrot"),
        7u64.to_le_bytes().to_vec(),
        3u32.to_le_bytes().to_vec(),
    ]
    .concat();
    let reply = ask(actor, Box::new(IndexTerms(payload))).await;
    assert_eq!(
        reply.downcast_ref::<IndexAck>().unwrap().0,
        1u32.to_le_bytes()
    );
    let query = [1u32.to_le_bytes().to_vec(), bytes_with_length(b"parrot")].concat();
    let reply = ask(actor, Box::new(Search(query))).await;
    let value: serde_json::Value =
        serde_json::from_slice(&reply.downcast_ref::<SearchResult>().unwrap().0).unwrap();
    assert_eq!(value, serde_json::json!([{"doc": 7, "score": 3}]));
}
