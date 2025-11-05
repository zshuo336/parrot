//! Crawler-specific driver over the standard application host and declared wires.
use crate::{cli, hub::BusinessHub};
use crawler_lab_core::{Config, Hub, MAX_BYTES};
use parrot::thread::{
    config::{ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{effective_config, AssemblingContext, LocalDeployer},
    host::{
        dylib::{DylibAsk, DylibReply},
        ApplicationLookup, GatewayRoutes, ProcessGatewayFactory, ThreadPropsFactory,
        ThreadPropsSpawner,
    },
    manifest::{wire_endpoint, AppManifest, ArtifactRef, EngineKind, InstancePolicy},
    planner::{plan, LocalTopology},
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Default)]
struct Admission(AtomicBool);
impl parrot::thread::mailbox::admission::MessageAdmission for Admission {
    fn prepare(
        &self,
        message: BoxedMessage,
    ) -> Result<BoxedMessage, parrot::thread::error::MailboxError> {
        use parrot::thread::error::MailboxError;
        if message.is::<parrot_app::assemble::ParrotAppHook>() {
            return Ok(message);
        }
        if self.0.load(Ordering::Acquire) {
            return Err(MailboxError::PushError(
                "crawler is draining; business admission is closed".into(),
            ));
        }
        if !message.is::<DylibAsk>() {
            return Err(MailboxError::PushError(
                "expected crawler business request".into(),
            ));
        }
        Ok(message)
    }
}
struct Embedded {
    hub: Hub,
    admission: Arc<Admission>,
}
impl Embedded {
    fn drain(&mut self) {
        self.admission.0.store(true, Ordering::Release);
        self.hub.drain();
    }
}
impl Actor for Embedded {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(hook) = message.downcast_ref::<parrot_app::assemble::ParrotAppHook>() {
                use parrot_app::assemble::hook_phase;
                match hook.0 {
                    hook_phase::START => {}
                    hook_phase::DRAIN | hook_phase::STOP => self.drain(),
                    _ => {
                        return Err(ActorError::InternalError(
                            "invalid crawler hook phase".into(),
                        ))
                    }
                }
                return Ok(Box::new(*hook) as BoxedMessage);
            }
            let ask = message.downcast_ref::<DylibAsk>().ok_or_else(|| {
                ActorError::InternalError("expected crawler business request".into())
            })?;
            let bytes = self
                .hub
                .handle(&ask.type_key, &ask.payload, MAX_BYTES)
                .map_err(|e| ActorError::InternalError(e.to_string()))?;
            Ok(Box::new(DylibReply(bytes)) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.drain();
        Box::pin(async { Ok(()) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let result = async {
            let config = settings(config.as_ref())?;
            let hub = Hub::new(config)?;
            let admission = Arc::new(Admission::default());
            let actor = engine
                .spawn_at(
                    Embedded {
                        hub,
                        admission: admission.clone(),
                    },
                    &path,
                    None,
                    ThreadActorConfig {
                        message_admission: Some(admission),
                        ..Default::default()
                    },
                )
                .await?;
            Ok::<_, Error>(Box::new(actor) as BoxedActorRef)
        }
        .await;
        result.map_err(|e| ActorError::InternalError(e.to_string()))
    })
}
parrot_api::message::inventory::submit! { ThreadPropsFactory { name: "crawler.business", spawn } }

fn settings(table: Option<&toml::Table>) -> Result<Config, Error> {
    let json = table
        .and_then(|t| t.get("settings"))
        .and_then(toml::Value::as_str)
        .ok_or("crawler component requires config.settings JSON")?;
    Ok(Config::from_json(json.as_bytes())?)
}
fn resolve_uri(uri: &mut String, directory: &Path) -> Result<(), Error> {
    let path = if uri.starts_with("file:") {
        url::Url::parse(uri)?
            .to_file_path()
            .map_err(|_| "invalid local artifact file URI")?
    } else if uri.contains("://") {
        return Err("crawler local host requires local artifact paths or file URIs".into());
    } else {
        directory.join(&*uri)
    };
    *uri = url::Url::from_file_path(std::fs::canonicalize(path)?)
        .map_err(|_| "artifact path is not absolute")?
        .to_string();
    Ok(())
}

/// All parsing and path/config validation precede resource creation.
fn prepare(
    path: &Path,
    override_values: Config,
    explicit: [bool; 4],
    selected: &cli::Hub,
) -> Result<(AppManifest, String, Config), Error> {
    let mut manifest = AppManifest::from_file(path)?;
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let hubs: Vec<_> = manifest
        .components
        .iter()
        .enumerate()
        .filter(|(_, c)| c.engine == EngineKind::Parrot)
        .map(|(i, _)| i)
        .collect();
    if hubs.len() != 1 {
        return Err("crawler requires exactly one parrot business component".into());
    }
    let component = &mut manifest.components[hubs[0]];
    if component.instances != InstancePolicy::Singleton {
        return Err("crawler business component must be a singleton".into());
    }
    let name = component.name.clone();
    let mut workload = settings(component.config.as_ref())?;
    if explicit[0] {
        workload.pages = override_values.pages;
    }
    if explicit[1] {
        workload.depth = override_values.depth;
    }
    if explicit[2] {
        workload.fanout = override_values.fanout;
    }
    if explicit[3] {
        workload.batch = override_values.batch;
    }
    workload.validate()?;
    component.config.as_mut().unwrap().insert(
        "settings".into(),
        toml::Value::String(serde_json::to_string(&workload)?),
    );
    // CLI artifact paths are relative to cwd; manifest artifact paths to its file.
    match selected {
        cli::Hub::Embedded => {}
        cli::Hub::Dylib { path, digest } | cli::Hub::Wasm { path, digest } => {
            let uri = url::Url::from_file_path(std::fs::canonicalize(path)?)
                .map_err(|_| "hub path must be absolute")?
                .to_string();
            component.artifact = if matches!(selected, cli::Hub::Wasm { .. }) {
                ArtifactRef::Wasm {
                    uri,
                    digest: digest.clone(),
                }
            } else {
                ArtifactRef::Dylib {
                    uri,
                    digest: digest.clone(),
                    abi: 1,
                }
            };
            component.alt_artifact = None;
        }
    }
    for component in &mut manifest.components {
        for artifact in
            std::iter::once(&mut component.artifact).chain(component.alt_artifact.iter_mut())
        {
            match artifact {
                ArtifactRef::Wasm { uri, .. } | ArtifactRef::Dylib { uri, .. } => {
                    resolve_uri(uri, directory)?
                }
                ArtifactRef::Beam { uri, .. }
                | ArtifactRef::PyModule { uri, .. }
                | ArtifactRef::Jvm { uri, .. } => resolve_uri(
                    uri.as_mut()
                        .ok_or("crawler gateway requires an explicit artifact URI")?,
                    directory,
                )?,
                ArtifactRef::Props { factory } if factory == "crawler.business" => {}
                _ => return Err("unsupported crawler Props factory".into()),
            }
        }
    }
    for alias in ["frontier", "index", "search"] {
        let source = format!("{name}:/user/{alias}");
        if !manifest.wiring.iter().any(|wire| wire.from == source) {
            return Err(format!("missing business wire {source}").into());
        }
    }
    plan(&manifest, &LocalTopology)?;
    Ok((manifest, name, workload))
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    path: &str,
    bind: std::net::SocketAddr,
    query: Option<(String, String)>,
    wait_secs: u64,
    overrides: Config,
    explicit: [bool; 4],
    selected: cli::Hub,
) -> i32 {
    match execute(
        Path::new(path),
        bind,
        query,
        wait_secs,
        overrides,
        explicit,
        selected,
    )
    .await
    {
        Ok(()) => {
            println!("\ncrawler-lab PASS（四运行时全链集成）");
            0
        }
        Err(error) => {
            eprintln!("crawler-lab: {error}");
            1
        }
    }
}
#[allow(clippy::too_many_arguments)]
async fn execute(
    path: &Path,
    bind: std::net::SocketAddr,
    query: Option<(String, String)>,
    wait_secs: u64,
    overrides: Config,
    explicit: [bool; 4],
    selected: cli::Hub,
) -> Result<(), Error> {
    let (manifest, name, workload) = prepare(path, overrides, explicit, &selected)?;
    if let Some((ready, done)) = &query {
        if Path::new(ready).exists() || Path::new(done).exists() {
            return Err("query coordination paths must not already exist".into());
        }
    }
    let plan = plan(&manifest, &LocalTopology)?;
    let mut base = parrot_config::ParrotConfig::builder()
        .remote_node_id("crawler-lab")
        .remote_bind(bind.to_string());
    base.remote.security.mode = Some("insecure-development".into());
    let resolved = effective_config(&manifest, &base)?;
    let remote_config =
        parrot_remote::RemoteConfig::from_resolved_async(&resolved, Duration::from_secs(5)).await?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let thread = ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&resolved));
    let routes = Arc::new(GatewayRoutes::default());
    let remote = match parrot_remote::RemoteActorSystem::new(
        remote_config,
        Arc::new(ApplicationLookup {
            ts: thread.clone(),
            routes: routes.clone(),
        }),
    ) {
        Ok(remote) => remote,
        Err(error) => {
            thread.shutdown_internal().await?;
            return Err(error.into());
        }
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let gateway = ProcessGatewayFactory::new_with_routes(root, remote.clone(), routes);
    let mut spawner = ThreadPropsSpawner::with_remote(thread.clone(), remote.clone())
        .with_wasm_limits(
            parrot_wasm::WasmConfig {
                fuel_per_message: 1_000_000_000,
                epoch_deadline: 100,
                memory_limit_mb: 128,
            },
            Default::default(),
        );
    if let cli::Hub::Dylib { digest, .. } = &selected {
        // SAFETY: only the operator's explicit native SHA-256 CLI authorization
        // grants trust; a manifest cannot authorize its own native code.
        spawner = unsafe { spawner.with_trusted_dylib(digest) }?;
    }
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    let business = async {
        remote.start().await?;
        context.assemble(&plan, &deployer, &manifest, &base).await?;
        // Verify the gateways' actual deployment receipts, including version;
        // logging the input manifest alone would not prove it was applied.
        for component in manifest
            .components
            .iter()
            .filter(|c| c.engine != EngineKind::Parrot)
        {
            let node = gateway
                .component_node(&component.name)
                .ok_or("missing owned gateway identity")?;
            let states = remote
                .component_status(&node, &format!("/user/{}", component.name))
                .await?;
            let paths = context
                .component_paths(&component.name)
                .ok_or("missing assembled paths")?;
            if states.len() != paths.len()
                || paths.iter().any(|path| {
                    !states
                        .iter()
                        .any(|state| state.path == *path && state.version == manifest.version)
                })
            {
                return Err(format!(
                    "deployment state differs from manifest for {}: {states:?}",
                    component.name
                )
                .into());
            }
            println!(
                "[lab] verified deployment {} version={} paths={paths:?}",
                component.name, manifest.version
            );
        }
        println!(
            "[lab] assembled application {} version={} components={:?}",
            manifest.name,
            manifest.version,
            context.started()
        );
        let actor = context
            .component_ref(&name)
            .ok_or("missing assembled hub")?
            .clone_boxed();
        let artifact = &manifest.component(&name).unwrap().artifact;
        let mut hub = BusinessHub::assembled(
            actor,
            matches!(artifact, ArtifactRef::Wasm { .. }),
            artifact.kind(),
        );
        let wire = |alias| {
            context
                .wired_ref(&format!("{name}:/user/{alias}"))
                .ok_or_else(|| format!("missing assembled wire {alias}"))
        };
        let result = crate::run_business(
            wire("frontier")?.as_ref(),
            wire("index")?.as_ref(),
            wire("search")?.as_ref(),
            &workload,
            &mut hub,
        )
        .await;
        let closed = hub.close().await;
        let expected_hits = result.map_err(|e| format!("business: {e}; hub close: {closed:?}"))?;
        closed?;
        if let Some((ready, done)) = &query {
            let wire = manifest
                .wiring
                .iter()
                .find(|w| w.from == format!("{name}:/user/search"))
                .unwrap();
            let (_, search_path) = wire_endpoint(&wire.to)?;
            let endpoint = remote
                .local_addr()
                .ok_or("application has no TCP listener")?;
            // Publish a complete document atomically; create_new prevents stale
            // readiness from granting success to a later run.
            let temp = format!("{ready}.{}.tmp", std::process::id());
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            use std::io::Write;
            file.write_all(serde_json::to_string(&serde_json::json!({"endpoint": endpoint.to_string(), "path": search_path, "pages": workload.pages, "expected_hits": expected_hits}))?.as_bytes())?;
            drop(file);
            std::fs::hard_link(&temp, ready)?;
            std::fs::remove_file(&temp)?;
            println!("PARROT_CRAWLER_QUERY_READY={endpoint}");
            tokio::time::timeout(Duration::from_secs(wait_secs), async {
                loop {
                    match query_ack(Path::new(done)).await {
                        Ok(bytes) if bytes == b"ok\n" => return Ok::<(), Error>(()),
                        Ok(_) => {
                            return Err(
                                "external query failed: expected exact ok acknowledgement".into()
                            )
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            tokio::time::sleep(Duration::from_millis(50)).await
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            })
            .await
            .map_err(|_| "external query completion timed out")??;
        }
        Ok::<(), Error>(())
    };
    #[cfg(unix)]
    let result = tokio::select! { result = business => result, _ = terminate.recv() => Err("application interrupted by SIGTERM".into()), _ = interrupt.recv() => Err("application interrupted by SIGINT".into()) };
    #[cfg(not(unix))]
    let result = tokio::select! { result = business => result, _ = tokio::signal::ctrl_c() => Err("application interrupted".into()) };
    let mut errors = Vec::new();
    if let Err(error) = result {
        errors.push(error.to_string());
    }
    if let Err(error) = context.teardown(&deployer).await {
        errors.push(error.to_string());
    }
    if let Err(error) = gateway.procs().kill_all().await {
        errors.push(error.to_string());
    }
    if let Err(error) = remote.shutdown().await {
        errors.push(error.to_string());
    }
    if let Err(error) = thread.shutdown_internal().await {
        errors.push(error.to_string());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}

async fn query_ack(path: &Path) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if !metadata.is_file() || metadata.len() != 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "query acknowledgement must be a three-byte regular file",
        ));
    }
    let mut bytes = Vec::with_capacity(4);
    tokio::fs::File::open(path)
        .await?
        .take(4)
        .read_to_end(&mut bytes)
        .await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        for path in ["erlang", "python", "jvm/target"] {
            std::fs::create_dir_all(dir.path().join(path)).unwrap();
        }
        std::fs::write(dir.path().join("jvm/target/crawler-lab-jvm-1.0.0.jar"), []).unwrap();
        let path = dir.path().join("crawler.app.toml");
        std::fs::write(&path, include_str!("../crawler.app.toml")).unwrap();
        (dir, path)
    }
    #[test]
    fn preparation_preserves_manifest_settings_and_explicit_zero() {
        let (_dir, path) = fixture();
        let (manifest, _, config) =
            prepare(&path, Config::default(), [false; 4], &cli::Hub::Embedded).unwrap();
        assert_eq!(config.pages, 60);
        let ArtifactRef::Beam { uri: Some(uri), .. } = &manifest.components[0].artifact else {
            panic!("wrong artifact")
        };
        assert_eq!(
            url::Url::parse(uri).unwrap().to_file_path().unwrap(),
            path.parent()
                .unwrap()
                .join("erlang")
                .canonicalize()
                .unwrap()
        );
        let (manifest, _, config) = prepare(
            &path,
            Config {
                pages: 0,
                batch: 7,
                ..Config::default()
            },
            [true, false, false, true],
            &cli::Hub::Embedded,
        )
        .unwrap();
        assert_eq!((config.pages, config.batch), (0, 7));
        let actual = settings(manifest.component("crawler").unwrap().config.as_ref()).unwrap();
        assert_eq!((actual.pages, actual.batch), (0, 7));
    }
    #[test]
    fn missing_alias_and_invalid_business_settings_fail_before_execution() {
        let (_dir, path) = fixture();
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            original.replace("crawler:/user/search", "crawler:/user/other"),
        )
        .unwrap();
        assert!(
            prepare(&path, Config::default(), [false; 4], &cli::Hub::Embedded)
                .unwrap_err()
                .to_string()
                .contains("missing business wire")
        );
        std::fs::write(&path, original.replace("\"batch\":32", "\"batch\":0")).unwrap();
        assert!(
            prepare(&path, Config::default(), [false; 4], &cli::Hub::Embedded)
                .unwrap_err()
                .to_string()
                .contains("bounds")
        );
    }
    #[tokio::test]
    async fn acknowledgement_requires_regular_exact_payload() {
        let dir = tempfile::tempdir().unwrap();
        assert!(query_ack(dir.path()).await.is_err());
        let path = dir.path().join("ack");
        std::fs::write(&path, b"ok\n").unwrap();
        assert_eq!(query_ack(&path).await.unwrap(), b"ok\n");
        std::fs::write(&path, b"ok\nexcess").unwrap();
        assert!(query_ack(&path).await.is_err());
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(query_ack(&link).await.is_err());
        }
    }
    #[tokio::test]
    async fn closing_context_owned_hub_keeps_it_available_until_context_stops_it() {
        let engine = ThreadActorSystem::shared(Default::default());
        let mut config = toml::Table::new();
        config.insert(
            "settings".into(),
            toml::Value::String(serde_json::to_string(&Config::default()).unwrap()),
        );
        let actor = spawn(engine.clone(), "/user/test-business".into(), Some(config))
            .await
            .unwrap();
        let mut hub = BusinessHub::assembled(actor.clone_boxed(), false, "props");
        hub.tick(&[(1, crawler_lab_core::url_of(1), 0)])
            .await
            .unwrap();
        hub.close().await.unwrap();
        use parrot_api::address::ActorRefExt;
        let progress = actor
            .ask(DylibAsk {
                type_key: "crawl/Progress".into(),
                payload: vec![],
            })
            .await
            .unwrap();
        assert_eq!(
            progress.0,
            [1u64.to_le_bytes(), 1u64.to_le_bytes()].concat()
        );
        use parrot_app::assemble::{hook_phase, ParrotAppHook};
        actor
            .send(Box::new(ParrotAppHook(hook_phase::DRAIN)))
            .await
            .unwrap();
        assert!(actor
            .ask(DylibAsk {
                type_key: "crawl/Tick".into(),
                payload: crawler_lab_core::encode_request(2, &[]).unwrap()
            })
            .await
            .is_err());
        // Admission rejection must not trigger supervision and kill the actor:
        // lifecycle hooks still have to reach this same instance afterward.
        actor
            .send(Box::new(ParrotAppHook(hook_phase::STOP)))
            .await
            .unwrap();
        assert!(engine.get_actor_ref("/user/test-business").is_some());
        actor.stop().await.unwrap();
        engine.shutdown_internal().await.unwrap();
    }
}
