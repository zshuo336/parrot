//! Record actual memory transport traffic, then replay into a fresh Counter.
//! cargo run --locked -p parrot-app --example record_replay -- /tmp/new-capture.jsonl
use parrot::thread::{
    config::ThreadActorConfig, context::ThreadContext, system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::debug::{Recording, ReplayLog, ReplayOptions};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(serde::Serialize, serde::Deserialize)]
struct Count(i64);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:example.Count", type_id: std::any::TypeId::of::<Count>(),
        encode: |msg| parrot_api::message::serde_remote_serialize(msg.downcast_ref::<Count>().ok_or("Count required")?),
        decode: |bytes| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<Count>(bytes)?) as BoxedMessage),
    }
}
struct Counter(i64);
impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0 += msg.downcast_ref::<Count>().unwrap().0;
            Ok(Box::new(Count(self.0)) as BoxedMessage)
        })
    }
}
struct Lookup(BoxedActorRef);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/counter").then(|| self.0.clone_boxed())
    }
}
async fn counter() -> (Arc<ThreadActorSystem>, Arc<Lookup>) {
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_at(Counter(0), "/counter", None, ThreadActorConfig::default())
        .await
        .unwrap();
    (system, Arc::new(Lookup(Box::new(actor))))
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("parrot-capture.jsonl"));
    let recording = Recording::start(&path, Default::default(), Default::default())?;
    let (original, lookup) = counter().await;
    let server = RemoteActorSystem::new(
        RemoteConfig::mem("capture-server").with_capture(recording.capture()),
        lookup.clone(),
    )?;
    let client = RemoteActorSystem::new(RemoteConfig::mem("capture-client"), lookup)?;
    server.start().await?;
    client.start().await?;
    client.connect_mem_pair(&server).await?;
    let remote = client.remote_ref("parrot://capture-server/counter")?;
    let mut expected = Vec::new();
    for delta in [2, 3, 4] {
        expected.push(
            remote
                .send_with_timeout(Box::new(Count(delta)), Some(Duration::from_secs(5)))
                .await?
                .downcast::<Count>()
                .unwrap()
                .0,
        );
    }
    client.shutdown().await?;
    server.shutdown().await?;
    original.shutdown_internal().await?;
    let receipt = tokio::task::spawn_blocking(move || recording.finish()).await??;
    let log = ReplayLog::load_capture(&path)?;
    let (fresh, lookup) = counter().await;
    let report = log
        .replay_local(
            lookup.as_ref(),
            ReplayOptions {
                target_node: Some("capture-server".into()),
                ..Default::default()
            },
        )
        .await?;
    let actual: Vec<_> = report
        .replies
        .iter()
        .map(|reply| {
            parrot_remote::codec_registry::CodecRegistry::global()
                .decode_incoming(&reply.type_key, &reply.payload)
                .unwrap()
                .downcast::<Count>()
                .unwrap()
                .0
        })
        .collect();
    fresh.shutdown_internal().await?;
    assert_eq!(actual, expected);
    assert_eq!(actual, [2, 5, 9]);
    println!(
        "{} frames recorded; fresh actor replies {actual:?}; log {}",
        receipt.written,
        path.display()
    );
    Ok(())
}
