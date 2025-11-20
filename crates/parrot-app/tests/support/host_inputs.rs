use parrot::thread::system::ThreadActorSystem;
use parrot_api::{
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use parrot_app::{
    assemble::{DeployError, ParrotSpawner},
    host::{ThreadPropsFactory, ThreadPropsSpawner},
    manifest::ArtifactRef,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
};

pub const FACTORY: &str = "parrot.host-input-probe";

fn calls() -> &'static Mutex<HashMap<String, usize>> {
    static CALLS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    CALLS.get_or_init(Default::default)
}

fn spawn(
    _: Arc<ThreadActorSystem>,
    path: String,
    _: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        *calls().lock().unwrap().entry(path).or_default() += 1;
        Err(ActorError::InitializationError(
            "host input probe invoked".into(),
        ))
    })
}

parrot_api::message::inventory::submit! { ThreadPropsFactory { name: FACTORY, spawn } }

pub fn invocations(path: &str) -> usize {
    calls().lock().unwrap().get(path).copied().unwrap_or(0)
}

pub fn path() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("/user/host-input-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

pub async fn request(
    spawner: &ThreadPropsSpawner,
    paths: &[String],
    config: Option<&toml::Table>,
    props: bool,
) -> Result<Vec<BoxedActorRef>, DeployError> {
    if props {
        spawner.spawn_props(FACTORY, paths, config).await
    } else {
        spawner
            .spawn_artifact(
                &ArtifactRef::Props {
                    factory: FACTORY.into(),
                },
                paths,
                config,
            )
            .await
    }
}

pub fn rejected(result: Result<Vec<BoxedActorRef>, DeployError>, resource: &str) {
    let diagnostic = result.as_ref().err().map(|error| match error {
        DeployError::Config(message)
        | DeployError::Engine(message)
        | DeployError::Artifact(message) => {
            format!(
                "{} bytes, prefix {:?}, probe_invoked={}",
                message.len(),
                message.chars().take(160).collect::<String>(),
                message.contains("host input probe invoked")
            )
        }
        _ => "other deployment error".into(),
    });
    assert!(
        matches!(&result, Err(DeployError::Config(message))
        if message.contains("budget exceeded") && message.contains(resource)),
        "request was not rejected by the {resource} budget: {diagnostic:?}"
    );
}

pub async fn reaches_factory(
    spawner: &ThreadPropsSpawner,
    paths: &[String],
    config: Option<&toml::Table>,
    props: bool,
) {
    let result = request(spawner, paths, config, props).await;
    assert!(matches!(result, Err(DeployError::Engine(message))
        if message.contains("host input probe invoked")));
    assert_eq!(invocations(&paths[0]), 1);
}

pub fn nested_table(depth: usize) -> toml::Table {
    assert!(depth >= 1);
    let mut value = toml::Value::Integer(7);
    for _ in 1..depth {
        value = toml::Value::Array(vec![value]);
    }
    toml::Table::from_iter([("value".into(), value)])
}
