use parrot::thread::{
    config::ThreadActorConfig, context::ThreadContext, system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Count(pub i64);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:example.ReplayCount#v1", type_id: std::any::TypeId::of::<Count>(),
        encode: |msg| parrot_api::message::serde_remote_serialize(msg.downcast_ref::<Count>().ok_or("Count required")?),
        decode: |bytes| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<Count>(bytes)?) as BoxedMessage),
    }
}
pub struct Counter {
    value: i64,
    delay: Duration,
    journal: Option<PathBuf>,
    fail_stop: bool,
}
impl Drop for Counter {
    fn drop(&mut self) {
        // Retirement is observable even when a failed actor skips before_stop.
        let _ = note(self.journal.as_deref(), "disposed");
    }
}
fn note(path: Option<&std::path::Path>, line: &str) -> ActorResult<()> {
    use std::io::Write;
    if let Some(path) = path {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| ActorError::InternalError(e.to_string()))?;
        writeln!(file, "{line}").map_err(|e| ActorError::InternalError(e.to_string()))?;
    }
    Ok(())
}
impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let count = message
                .downcast::<Count>()
                .map_err(|_| ActorError::MessageHandlingError("Count required".into()))?;
            note(self.journal.as_deref(), &format!("handling {}", count.0))?;
            tokio::time::sleep(self.delay).await;
            if count.0 < 0 {
                return Err(ActorError::MessageHandlingError(
                    "negative increments are rejected".into(),
                ));
            }
            self.value = self
                .value
                .checked_add(count.0)
                .ok_or_else(|| ActorError::MessageHandlingError("counter overflow".into()))?;
            note(self.journal.as_deref(), &format!("value {}", self.value))?;
            Ok(Box::new(Count(self.value)) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            note(self.journal.as_deref(), "stopped")?;
            if self.fail_stop {
                Err(ActorError::MessageHandlingError(
                    "injected stop failure".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
}
fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let config = config.unwrap_or_default();
        let journal = config
            .get("journal")
            .and_then(toml::Value::as_str)
            .map(PathBuf::from);
        let delay = config
            .get("delay_ms")
            .and_then(toml::Value::as_integer)
            .unwrap_or(0);
        if !(0..=60_000).contains(&delay) {
            return Err(ActorError::InitializationError(
                "delay_ms must be in 0..=60000".into(),
            ));
        }
        note(journal.as_deref(), "started")?;
        let actor = engine
            .spawn_at(
                Counter {
                    value: 0,
                    delay: Duration::from_millis(delay as u64),
                    journal,
                    fail_stop: config
                        .get("fail_stop")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false),
                },
                &path,
                None,
                ThreadActorConfig::default(),
            )
            .await
            .map_err(|e| ActorError::InternalError(e.to_string()))?;
        Ok(Box::new(actor) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! { parrot_app::host::ThreadPropsFactory { name: "example.replay-counter", spawn } }
struct Lookup(BoxedActorRef);
#[async_trait::async_trait]
impl parrot_remote::LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/user/counter").then(|| self.0.clone_boxed())
    }
}
pub async fn capture(path: PathBuf) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let recording =
        parrot_app::debug::Recording::start(&path, Default::default(), Default::default())?;
    let engine = ThreadActorSystem::shared(Default::default());
    let actor = spawn(engine.clone(), "/user/counter".into(), None).await?;
    let lookup = Arc::new(Lookup(actor));
    let server = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::mem("capture-server").with_capture(recording.capture()),
        lookup.clone(),
    )?;
    let client = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::mem("capture-client"),
        lookup,
    )?;
    server.start().await?;
    client.start().await?;
    client.connect_mem_pair(&server).await?;
    let actor = client.remote_ref("parrot://capture-server/user/counter")?;
    let mut results = Vec::new();
    for delta in [2, 3, 4] {
        results.push(
            actor
                .send_with_timeout(Box::new(Count(delta)), Some(Duration::from_secs(5)))
                .await?
                .downcast::<Count>()
                .unwrap()
                .0,
        );
    }
    assert_eq!(results, [2, 5, 9]);
    client.shutdown().await?;
    server.shutdown().await?;
    engine.shutdown_internal().await?;
    tokio::task::spawn_blocking(move || recording.finish()).await??;
    Ok(())
}
