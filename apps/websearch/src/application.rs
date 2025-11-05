//! Application-specific driver over Parrot's real host, actors and declared wires.
use super::{cli, shutdown, workload, BoxedActorRef, BoxedMessage, RemoteActorSystem};
use parrot::thread::{
    config::ThreadActorSystemConfig, context::ThreadContext, system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    types::{ActorResult, BoxedFuture},
};
use parrot_app::{
    assemble::{effective_config, hook_phase, AssemblingContext, LocalDeployer, ParrotAppHook},
    host::{
        ApplicationLookup, GatewayRoutes, ProcessGatewayFactory, ThreadPropsFactory,
        ThreadPropsSpawner,
    },
    manifest::{wire_endpoint, AppManifest, ArtifactRef, EngineKind, InstancePolicy},
    planner::{plan, LocalTopology},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
type Error = Box<dyn std::error::Error + Send + Sync>;
const FACTORY: &str = "websearch.crawler";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    seeds: Vec<String>,
    pages: u64,
    sites: usize,
    maxdepth: u16,
    port: u16,
    data: String,
    index_data: Option<String>,
    keep: bool,
    serve_only: bool,
    crawl_origins: Vec<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self::from_options(&cli::Options::default())
    }
}
impl Settings {
    fn from_options(o: &cli::Options) -> Self {
        Self {
            seeds: o.seeds.clone(),
            pages: o.pages,
            sites: o.sites_target,
            maxdepth: o.max_depth,
            port: o.web_port,
            data: o.data_dir.to_string_lossy().into_owned(),
            index_data: o.index_data.clone(),
            keep: o.keep_data,
            serve_only: o.serve_only,
            crawl_origins: o.crawl_origins.iter().map(ToString::to_string).collect(),
        }
    }
    fn read(config: Option<&toml::Table>) -> Result<Self, Error> {
        let table = config.ok_or("crawler requires config.settings")?;
        if table.len() != 1 {
            return Err("crawler config accepts only settings JSON".into());
        }
        let value = table
            .get("settings")
            .and_then(toml::Value::as_str)
            .ok_or("crawler requires settings JSON string")?;
        Ok(serde_json::from_str(value)?)
    }
    fn table(&self) -> Result<toml::Table, Error> {
        Ok([(
            "settings".into(),
            toml::Value::String(serde_json::to_string(self)?),
        )]
        .into_iter()
        .collect())
    }
    fn options(&self, flags: &cli::Options) -> Result<cli::Options, Error> {
        let mut args = vec![
            "--pages".into(),
            self.pages.to_string(),
            "--sites".into(),
            self.sites.to_string(),
            "--maxdepth".into(),
            self.maxdepth.to_string(),
            "--port".into(),
            self.port.to_string(),
            "--data".into(),
            self.data.clone(),
            "--wait".into(),
            flags.wait_secs.to_string(),
        ];
        if let Some(path) = &self.index_data {
            args.extend(["--index-data".into(), path.clone()]);
        }
        if self.keep {
            args.push("--keep".into());
        }
        if self.serve_only {
            args.push("--serve-only".into());
        }
        if flags.insecure_development {
            args.push("--insecure-development".into());
        }
        if let Some(bind) = flags.bind_addr {
            args.extend(["--bind".into(), bind.to_string()]);
        }
        for origin in &self.crawl_origins {
            args.extend(["--crawl-origin".into(), origin.clone()]);
        }
        args.push("--".into());
        args.extend(self.seeds.clone());
        let mut options =
            cli::parse(args)?.ok_or("manifest settings unexpectedly requested help")?;
        // Host policy is not supplied by component settings or manifests.
        options.gateway_process_limit = flags.gateway_process_limit;
        Ok(options)
    }
}

pub(crate) struct Prepared {
    runtime: super::host_runtime::Runtime,
    beam_sources: HashMap<String, PathBuf>,
    manifest: AppManifest,
    name: String,
    roles: HashMap<String, String>,
    pub options: cli::Options,
}
fn absolute(path: &Path, directory: &Path) -> Result<PathBuf, Error> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        directory.join(path)
    };
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}
fn resolve_uri(uri: &mut String, directory: &Path) -> Result<(), Error> {
    let path = if uri.starts_with("file:") {
        url::Url::parse(uri)?
            .to_file_path()
            .map_err(|_| "invalid artifact file URI")?
    } else if uri.contains("://") {
        return Err("local manifest host requires local artifact paths".into());
    } else {
        directory.join(&*uri)
    };
    *uri = url::Url::from_file_path(std::fs::canonicalize(path)?)
        .map_err(|_| "invalid absolute artifact path")?
        .to_string();
    Ok(())
}

/// Parse/resolve/schema/config checks precede listener, storage and child creation.
pub(crate) fn prepare(path: &Path, cli: cli::Options) -> Result<Prepared, Error> {
    if !cli.gw_addrs.is_empty() || cli.node_root.is_some() {
        return Err(
            "--manifest owns local gateways; direct endpoints and --node-root are incompatible"
                .into(),
        );
    }
    let mut manifest = AppManifest::from_file(path)?;
    if manifest.wiring.iter().any(|wire| wire.qos == "wan") {
        return Err("local manifest host does not implement per-connection WAN QoS".into());
    }
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let hubs: Vec<_> = manifest
        .components
        .iter()
        .filter(|c| c.engine == EngineKind::Parrot)
        .collect();
    if manifest.components.len() != 4 || hubs.len() != 1 {
        return Err("websearch requires one crawler and three gateway components".into());
    }
    let hub = hubs[0];
    let name = hub.name.clone();
    if hub.artifact
        != (ArtifactRef::Props {
            factory: FACTORY.into(),
        })
    {
        return Err("websearch crawler requires the embedded websearch.crawler Props factory; an executable is not a Dylib".into());
    }
    if [
        hub.hooks.on_start.as_deref(),
        hub.hooks.on_drain.as_deref(),
        hub.hooks.on_stop.as_deref(),
    ] != [Some("."), Some("."), Some(".")]
    {
        return Err("crawler requires start/drain/stop hooks at its actor root".into());
    }
    let mut settings = Settings::read(hub.config.as_ref())?;
    if settings.data.is_empty() || settings.index_data.as_deref() == Some("") {
        return Err("manifest data and index_data must not be empty".into());
    }
    let explicit = &cli.explicit;
    macro_rules! replace {
        ($flag:literal,$field:ident,$value:expr) => {
            if explicit.contains($flag) {
                settings.$field = $value;
            }
        };
    }
    replace!("--pages", pages, cli.pages);
    replace!("--sites", sites, cli.sites_target);
    replace!("--maxdepth", maxdepth, cli.max_depth);
    replace!("--port", port, cli.web_port);
    replace!("--keep", keep, true);
    replace!("--serve-only", serve_only, true);
    if explicit.contains("--data") {
        settings.data = absolute(&cli.data_dir, Path::new("."))?
            .to_string_lossy()
            .into_owned();
    } else {
        settings.data = absolute(Path::new(&settings.data), directory)?
            .to_string_lossy()
            .into_owned();
    }
    if explicit.contains("--index-data") {
        settings.index_data = cli.index_data.clone();
    } else if let Some(path) = &settings.index_data {
        settings.index_data = Some(
            absolute(Path::new(path), directory)?
                .to_string_lossy()
                .into_owned(),
        );
    }
    if explicit.contains("seed") {
        settings.seeds = cli.seeds.clone();
    }
    if explicit.contains("--crawl-origin") {
        settings.crawl_origins = cli.crawl_origins.iter().map(ToString::to_string).collect();
        if !explicit.contains("seed") {
            settings.seeds.clear();
        }
    }
    if settings.serve_only && !explicit.contains("seed") {
        settings.seeds.clear();
    }
    let mut roles = HashMap::new();
    if manifest.wiring.len() != 3 {
        return Err("websearch requires exactly three declared business wires".into());
    }
    for (role, engine) in [
        ("frontier", EngineKind::Erlang),
        ("tokenizer", EngineKind::Ray),
        ("search", EngineKind::Akka),
    ] {
        let source = format!("{name}:/user/{role}");
        let wire = manifest
            .wiring
            .iter()
            .find(|w| w.from == source)
            .ok_or_else(|| format!("missing business wire {source}"))?;
        let (target, path) = wire_endpoint(&wire.to)?;
        let component = manifest.component(target).ok_or("wire component missing")?;
        if component.engine != engine || path != format!("/user/{target}") {
            return Err(format!("{role} wire must target its singleton gateway root").into());
        }
        roles.insert(role.to_string(), target.to_string());
    }
    let search = manifest.component(&roles["search"]).unwrap();
    if let Some(table) = &search.config {
        if table.keys().any(|k| k != "data_dir") {
            return Err("search config accepts only data_dir".into());
        }
        if let Some(value) = table.get("data_dir") {
            let path = value.as_str().ok_or("search data_dir must be a string")?;
            if path.is_empty() {
                return Err("search data_dir must not be empty".into());
            }
            let selected = absolute(Path::new(path), directory)?
                .to_string_lossy()
                .into_owned();
            if !explicit.contains("--index-data") {
                if settings.index_data.as_ref().is_some_and(|p| p != &selected) {
                    return Err("conflicting crawler and search index configuration".into());
                }
                settings.index_data = Some(selected);
            }
        }
    }
    let options = settings.options(&cli)?;
    if options.serve_only {
        let search = roles["search"].clone();
        manifest
            .components
            .retain(|c| c.name == name || c.name == search);
        for component in &mut manifest.components {
            component.deps = if component.name == name {
                vec![search.clone()]
            } else {
                vec![]
            };
        }
        manifest
            .wiring
            .retain(|w| w.from == format!("{name}:/user/search"));
    }
    let mut beam_sources = HashMap::new();
    for component in &mut manifest.components {
        if component.instances != InstancePolicy::Singleton || component.alt_artifact.is_some() {
            return Err("websearch local host requires singleton primary artifacts".into());
        }
        match &mut component.artifact {
            ArtifactRef::Props { factory } if factory == FACTORY => {}
            ArtifactRef::Beam { app, uri } => {
                if app.is_empty() || app.contains(['/', '\\', '\0']) {
                    return Err("invalid BEAM application filename".into());
                }
                let uri = uri.as_mut().ok_or("gateway artifact requires URI")?;
                resolve_uri(uri, directory)?;
                let directory = url::Url::parse(uri)?
                    .to_file_path()
                    .map_err(|_| "invalid BEAM directory")?;
                // Preserve explicitly supplied BEAM bundles (including their
                // sibling dependencies). Source-only releases compile on the
                // target OTP into private storage before deployment.
                if !directory.join(format!("{app}.beam")).is_file() {
                    let source = directory.join(format!("{app}.erl"));
                    if !source.is_file() {
                        return Err(
                            "BEAM component requires its source or compiled artifact".into()
                        );
                    }
                    beam_sources.insert(component.name.clone(), source);
                }
            }
            ArtifactRef::PyModule { uri, .. } | ArtifactRef::Jvm { uri, .. } => resolve_uri(
                uri.as_mut().ok_or("gateway artifact requires URI")?,
                directory,
            )?,
            _ => return Err("unsupported websearch artifact".into()),
        }
    }
    plan(&manifest, &LocalTopology)?;
    effective_config(&manifest, &base_config(&options))?;
    let runtime =
        super::host_runtime::Runtime::discover(cli.runtime_root.as_deref(), !options.serve_only)?;
    Ok(Prepared {
        runtime,
        beam_sources,
        manifest,
        name,
        roles,
        options,
    })
}

struct Start {
    work: workload::Workload,
    refs: workload::Refs,
    remote: Arc<RemoteActorSystem>,
    stop: shutdown::Stop,
}
struct Started(tokio::sync::oneshot::Receiver<Result<(), String>>);
struct Crawler {
    settings: Settings,
    ready: bool,
    closing: bool,
    stop: Option<shutdown::Stop>,
    task: Option<tokio::task::JoinHandle<Result<(), String>>>,
}
impl Crawler {
    async fn finish(&mut self) -> ActorResult<()> {
        self.closing = true;
        if let Some(stop) = &self.stop {
            stop.request();
        }
        if let Some(task) = self.task.as_mut() {
            let result = task.await;
            self.task = None;
            result
                .map_err(|e| ActorError::InternalError(e.to_string()))?
                .map_err(ActorError::InternalError)?;
        }
        Ok(())
    }
}
impl Drop for Crawler {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.request();
        }
    }
}
impl Actor for Crawler {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(hook) = message.downcast_ref::<ParrotAppHook>() {
                match hook.0 {
                    hook_phase::START if !self.closing => {
                        self.ready = true;
                    }
                    hook_phase::DRAIN | hook_phase::STOP => self.finish().await?,
                    _ => {
                        return Err(ActorError::InternalError(
                            "invalid crawler lifecycle phase".into(),
                        ))
                    }
                }
                return Ok(Box::new(*hook) as BoxedMessage);
            }
            let Start {
                work,
                refs,
                remote,
                stop,
            } = *message.downcast::<Start>().map_err(|_| {
                ActorError::InternalError("expected assembled crawler start".into())
            })?;
            if !self.ready
                || self.closing
                || self.task.is_some()
                || self.stop.is_some()
                || Settings::from_options(&work.options) != self.settings
            {
                return Err(ActorError::InternalError(
                    "crawler start state/config mismatch".into(),
                ));
            }
            let (send, receive) = tokio::sync::oneshot::channel();
            self.stop = Some(stop.clone());
            self.task = Some(tokio::spawn(async move {
                let result = work
                    .run(refs, remote, stop)
                    .await
                    .map_err(|e| e.to_string());
                let _ = send.send(result.clone());
                result
            }));
            Ok(Box::new(Started(receive)) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(self.finish())
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
        let settings = Settings::read(config.as_ref())
            .map_err(|e| ActorError::InternalError(e.to_string()))?;
        settings
            .options(&cli::Options::default())
            .map_err(|e| ActorError::InternalError(e.to_string()))?;
        let actor = engine
            .spawn_at(
                Crawler {
                    settings,
                    ready: false,
                    closing: false,
                    stop: None,
                    task: None,
                },
                &path,
                None,
                Default::default(),
            )
            .await?;
        Ok(Box::new(actor) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! {ThreadPropsFactory{name:FACTORY,spawn}}

fn base_config(options: &cli::Options) -> parrot_config::ParrotConfig {
    let bind = options
        .bind_addr
        .unwrap_or_else(|| "127.0.0.1:0".parse().unwrap());
    let mut base = parrot_config::ParrotConfig::builder()
        .remote_node_id("websearch-host")
        .remote_bind(bind.to_string())
        .thread_shutdown_timeout_ms(60_000);
    if options.insecure_development {
        base.remote.security.mode = Some("insecure-development".into());
    }
    base
}

pub(crate) async fn run(mut prepared: Prepared, stop: shutdown::Stop) -> Result<(), Error> {
    let base = base_config(&prepared.options);
    let resolved = effective_config(&prepared.manifest, &base)?;
    let config =
        parrot_remote::RemoteConfig::from_resolved_async(&resolved, Duration::from_secs(5)).await?;
    let mut work = workload::Workload::prepare(prepared.options).await?;
    let _storage_owner = work.retain_lease()?;
    if work.options.index_data.is_none() {
        let selected = absolute(&work.options.data_dir, Path::new("."))?
            .to_string_lossy()
            .into_owned();
        work.options.index_data = work
            .identity
            .select_index(Some(selected), work.options.keep_data)?;
    }
    let settings = Settings::from_options(&work.options);
    for component in &mut prepared.manifest.components {
        if component.name == prepared.name {
            component.config = Some(settings.table()?);
        } else if component.name == prepared.roles["search"] {
            component.config = Some(
                [(
                    "data_dir".into(),
                    toml::Value::String(work.options.index_data.clone().unwrap()),
                )]
                .into_iter()
                .collect(),
            );
        }
    }
    let plan = plan(&prepared.manifest, &LocalTopology)?;
    let engine = ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&resolved));
    let routes = Arc::new(GatewayRoutes::default());
    let remote = match RemoteActorSystem::new(
        config,
        Arc::new(ApplicationLookup {
            ts: engine.clone(),
            routes: routes.clone(),
        }),
    ) {
        Ok(remote) => remote,
        Err(e) => {
            engine.shutdown_internal().await?;
            return Err(e.into());
        }
    };
    let gateway = ProcessGatewayFactory::new_with_routes_and_budget(
        prepared.runtime.root,
        remote.clone(),
        routes,
        Arc::new(parrot_app::host::GatewayProcessBudget::new(
            work.options.gateway_process_limit,
        )?),
    )
    .with_startup_timeout(Duration::from_secs(work.options.wait_secs))
    .with_beam_sources(prepared.beam_sources)
    .with_jvm_classpath(prepared.runtime.classpath);
    let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    context.set_hooks_timeout(Duration::from_secs(60));
    let outcome: Result<(), Error> = async {
        if stop.requested() {
            return Ok(());
        }
        remote.start().await?;
        context
            .assemble(&plan, &deployer, &prepared.manifest, &base)
            .await?;
        for component in prepared
            .manifest
            .components
            .iter()
            .filter(|c| c.engine != EngineKind::Parrot)
        {
            let node = gateway
                .component_node(&component.name)
                .ok_or("missing owned gateway")?;
            let paths = context
                .component_paths(&component.name)
                .ok_or("missing assembled paths")?;
            let states = remote
                .component_status(&node, &format!("/user/{}", component.name))
                .await?;
            if states.len() != paths.len()
                || paths.iter().any(|p| {
                    !states
                        .iter()
                        .any(|s| s.path == *p && s.version == prepared.manifest.version)
                })
            {
                return Err("manifest deployment receipt/status mismatch".into());
            }
            println!(
                "[ws] manifest deployed {} node={} version={} paths={paths:?}",
                component.name, node, prepared.manifest.version
            );
        }
        let wire = |role| {
            context
                .wired_ref(&format!("{}:/user/{role}", prepared.name))
                .ok_or_else(|| format!("missing assembled {role} wire"))
        };
        let refs = workload::Refs {
            search: wire("search")?,
            frontier: if work.options.serve_only {
                None
            } else {
                Some(wire("frontier")?)
            },
            tokenizer: if work.options.serve_only {
                None
            } else {
                Some(wire("tokenizer")?)
            },
        };
        println!(
            "[ws] manifest assembled {} version={} components={:?}",
            prepared.manifest.name,
            prepared.manifest.version,
            context.started()
        );
        let actor = context
            .component_ref(&prepared.name)
            .ok_or("missing crawler actor")?;
        let Started(done) = *actor
            .send(Box::new(Start {
                work,
                refs,
                remote: remote.clone(),
                stop: stop.clone(),
            }))
            .await?
            .downcast::<Started>()
            .map_err(|_| "unexpected crawler start acknowledgement")?;
        // Finish persistence and HTTP before context teardown closes declared wires.
        done.await
            .map_err(|_| "crawler task terminated before reporting completion")??;
        Ok(())
    }
    .await;
    stop.request();
    let mut errors = Vec::new();
    if let Err(e) = outcome {
        errors.push(e.to_string());
    }
    if let Err(e) = context.teardown(&deployer).await {
        errors.push(e.to_string());
    }
    if let Err(e) = gateway.procs().kill_all().await {
        errors.push(e.to_string());
    }
    if let Err(e) = remote.shutdown().await {
        errors.push(e.to_string());
    }
    if let Err(e) = engine.shutdown_internal().await {
        errors.push(e.to_string());
    }
    if errors.is_empty() {
        println!("[ws] manifest shutdown complete");
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}

#[cfg(test)]
#[path = "application_tests.rs"]
mod tests;
