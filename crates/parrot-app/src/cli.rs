//! Application runner: validate/resolve first, then build and own host resources.
use crate::{
    assemble::{effective_config, AssemblingContext, LocalDeployer},
    host::{
        ApplicationLookup, GatewayProcessBudget, GatewayRoutes, ProcessGatewayFactory,
        ThreadPropsSpawner,
    },
    manifest::AppManifest,
    planner::{plan, LocalNodeTopology},
};
use parrot::thread::{config::ThreadActorSystemConfig, system::ThreadActorSystem};
use parrot_remote::{RemoteActorSystem, RemoteConfig};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
pub type Error = Box<dyn std::error::Error + Send + Sync>;

fn next_path(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &str,
) -> Result<PathBuf, Error> {
    let path = args
        .next()
        .ok_or_else(|| format!("{flag} requires a path"))?;
    if path.is_empty() {
        return Err(format!("{flag} path must not be empty").into());
    }
    Ok(path.into())
}

/// Run the shared application command line in an active Tokio runtime.
/// Applications with inventory-registered Props/codecs can call this same entry
/// point from their own binary. Keep the future and runtime alive through cleanup.
pub async fn execute(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Result<(), Error> {
    let mut args = arguments.into_iter();
    let replay_mode = match args.next().as_deref().and_then(std::ffi::OsStr::to_str) {
            #[cfg(unix)]
            Some("trace") => return trace_query::execute(args).await,
            Some("run") => false,
            Some("replay") => true,
            _ => return Err("usage: parrot-app run --manifest <path> [--bind <ip:port>] [--record <new-path>] [--trace-otlp <https-url>] [--trace-allow-http] [--trace-capacity <1..=65536>] [--trace-socket <private-dir/socket>] [--gateway-process-limit <positive-integer>] [--wasm-epoch-ticks <1..=4294967295>] [--wasm-messaging-policy <operator-toml>] [--wasm-service-policy <operator-toml>] [--native-messaging-policy <operator-toml>] [--native-service-policy <operator-toml>] [--trust-native-sha256 <hex64>]...; or parrot-app trace <app> --socket <path> [--follow [--interval-ms <100..=60000>]] (Unix only); or parrot-app replay --manifest <path> --input <complete-capture> --report <new-path> [--direction rx|tx] [--tell-mode completion|admission] [--target-node <original-node>] [--call-timeout-ms <1..=86400000>] [--allow-legacy-log] (host options also apply; native trust executes code and dependencies in this process)".into()),
        };
    let mut input = None;
    let mut report = None;
    let mut direction = None;
    let mut tell_mode = None;
    let mut target_node = None;
    let mut call_timeout = None;
    let mut allow_legacy = false;
    let mut manifest = None;
    let mut bind = None;
    let mut record = None;
    let mut wasm_epoch_ticks = None;
    let mut wasm_policy_path = None;
    let mut wasm_service_policy_path = None;
    let mut trusted_native = Vec::new();
    let mut native_policy_path = None;
    let mut native_service_policy_path = None;
    let mut gateway_process_limit = None;
    let mut trace_endpoint = None;
    let mut trace_allow_http = false;
    let mut trace_capacity = None;
    let mut trace_socket = None;
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--native-service-policy") if native_service_policy_path.is_none() => {
                if !cfg!(feature = "dylib") {
                    return Err("--native-service-policy requires the dylib feature".into());
                }
                native_service_policy_path = Some(next_path(&mut args, "--native-service-policy")?);
            }
            Some("--native-messaging-policy") if native_policy_path.is_none() => {
                if !cfg!(feature = "dylib") {
                    return Err("--native-messaging-policy requires the dylib feature".into());
                }
                native_policy_path = Some(next_path(&mut args, "--native-messaging-policy")?);
            }
            Some("--wasm-service-policy") if wasm_service_policy_path.is_none() => {
                if !cfg!(feature = "wasm") {
                    return Err("--wasm-service-policy requires the wasm feature".into());
                }
                wasm_service_policy_path = Some(next_path(&mut args, "--wasm-service-policy")?);
            }
            Some("--wasm-messaging-policy") if wasm_policy_path.is_none() => {
                if !cfg!(feature = "wasm") {
                    return Err("--wasm-messaging-policy requires the wasm feature".into());
                }
                wasm_policy_path = Some(next_path(&mut args, "--wasm-messaging-policy")?);
            }
            Some("--trace-socket") if !replay_mode && trace_socket.is_none() => {
                if !cfg!(unix) {
                    return Err("--trace-socket requires Unix".into());
                }
                trace_socket = Some(next_path(&mut args, "--trace-socket")?);
            }
            Some("--trace-otlp") if !replay_mode && trace_endpoint.is_none() => {
                trace_endpoint = Some(
                    args.next()
                        .ok_or("--trace-otlp requires an endpoint")?
                        .into_string()
                        .map_err(|_| "--trace-otlp must be UTF-8")?,
                );
            }
            Some("--trace-capacity") if !replay_mode && trace_capacity.is_none() => {
                let value = args.next().ok_or("--trace-capacity requires a value")?;
                let value: usize = value
                    .to_str()
                    .ok_or("--trace-capacity must be UTF-8")?
                    .parse()
                    .map_err(|_| "--trace-capacity must be in 1..=65536")?;
                if !(1..=parrot_api::trace::MAX_RECORD_CAPACITY).contains(&value) {
                    return Err("--trace-capacity must be in 1..=65536".into());
                }
                trace_capacity = Some(value);
            }
            Some("--trace-allow-http") if !replay_mode && !trace_allow_http => {
                trace_allow_http = true;
            }
            Some("--input") if replay_mode && input.is_none() => {
                input = Some(next_path(&mut args, "--input")?);
            }
            Some("--report") if replay_mode && report.is_none() => {
                report = Some(next_path(&mut args, "--report")?);
            }
            Some("--direction") if replay_mode && direction.is_none() => {
                direction = Some(
                    match args.next().as_deref().and_then(std::ffi::OsStr::to_str) {
                        Some("rx") => crate::debug::ReplayDirection::Rx,
                        Some("tx") => crate::debug::ReplayDirection::Tx,
                        _ => return Err("--direction must be rx or tx".into()),
                    },
                );
            }
            Some("--tell-mode") if replay_mode && tell_mode.is_none() => {
                tell_mode = Some(
                    match args.next().as_deref().and_then(std::ffi::OsStr::to_str) {
                        Some("completion") => crate::debug::ReplayTellMode::Completion,
                        Some("admission") => crate::debug::ReplayTellMode::Admission,
                        _ => return Err("--tell-mode must be completion or admission".into()),
                    },
                );
            }
            Some("--target-node") if replay_mode && target_node.is_none() => {
                let value = args
                    .next()
                    .ok_or("--target-node requires a node")?
                    .into_string()
                    .map_err(|_| "--target-node must be UTF-8")?;
                if value.is_empty()
                    || value.len() > 1024
                    || value.contains('/')
                    || value.chars().any(char::is_control)
                {
                    return Err("invalid --target-node".into());
                }
                target_node = Some(value);
            }
            Some("--call-timeout-ms") if replay_mode && call_timeout.is_none() => {
                let value = args.next().ok_or("--call-timeout-ms requires a value")?;
                let value: u64 = value
                    .to_str()
                    .ok_or("timeout must be UTF-8")?
                    .parse()
                    .map_err(|_| "timeout must be an unsigned integer")?;
                if !(1..=86_400_000).contains(&value) {
                    return Err("--call-timeout-ms must be in 1..=86400000".into());
                }
                call_timeout = Some(std::time::Duration::from_millis(value));
            }
            Some("--allow-legacy-log") if replay_mode && !allow_legacy => {
                allow_legacy = true;
            }
            Some("--manifest") if manifest.is_none() => {
                manifest = Some(next_path(&mut args, "--manifest")?);
            }
            Some("--record") if record.is_none() && !replay_mode => {
                record = Some(next_path(&mut args, "--record")?);
            }
            Some("--bind") if bind.is_none() => {
                bind = Some(
                    args.next()
                        .ok_or("--bind requires an address")?
                        .into_string()
                        .map_err(|_| "--bind must be UTF-8")?,
                );
            }
            Some("--gateway-process-limit") if gateway_process_limit.is_none() => {
                let value = args
                    .next()
                    .ok_or("--gateway-process-limit requires a value")?;
                let value: usize = value
                    .to_str()
                    .ok_or("--gateway-process-limit must be UTF-8")?
                    .parse()
                    .map_err(|_| "--gateway-process-limit must be a positive integer")?;
                gateway_process_limit = Some(value);
            }
            Some("--wasm-epoch-ticks") if wasm_epoch_ticks.is_none() => {
                let value = args.next().ok_or("--wasm-epoch-ticks requires a value")?;
                let value: u64 = value
                    .to_str()
                    .ok_or("--wasm-epoch-ticks must be UTF-8")?
                    .parse()
                    .map_err(|_| "--wasm-epoch-ticks must be an unsigned integer")?;
                if !(1..=u32::MAX as u64).contains(&value) {
                    return Err("--wasm-epoch-ticks must be in 1..=4294967295".into());
                }
                if !cfg!(feature = "wasm") {
                    return Err("--wasm-epoch-ticks requires the wasm feature".into());
                }
                wasm_epoch_ticks = Some(value);
            }
            Some("--trust-native-sha256") => {
                let value = args
                    .next()
                    .ok_or("--trust-native-sha256 requires a digest")?
                    .into_string()
                    .map_err(|_| "native digest must be UTF-8")?;
                let hex = value.strip_prefix("sha256:").unwrap_or(&value);
                if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("--trust-native-sha256 requires 64 hex digits".into());
                }
                if !cfg!(feature = "dylib") {
                    return Err("--trust-native-sha256 requires the dylib feature".into());
                }
                trusted_native.push(value);
            }
            _ => {
                return Err(format!(
                    "unknown or duplicate argument: {}",
                    argument.to_string_lossy()
                )
                .into())
            }
        }
    }
    let manifest = manifest.ok_or("--manifest is required")?;
    // Reject incomplete commands and invalid budgets before policy/capture I/O
    // or reserving the replay report. Effective manifest failures are reported
    // later through the ordinary replay teardown/report path.
    let replay_paths = if replay_mode {
        Some((
            input.ok_or("--input is required")?,
            report.ok_or("--report is required")?,
        ))
    } else {
        None
    };
    let budget = Arc::new(GatewayProcessBudget::new(
        gateway_process_limit.unwrap_or(GatewayProcessBudget::DEFAULT_MAX_GROUPS),
    )?);
    #[cfg(feature = "dylib")]
    let native_messaging = match native_policy_path {
        Some(path) => Some(crate::host::dylib::NativeMessagingPolicy::load_file(path).await?),
        None => None,
    };
    #[cfg(feature = "dylib")]
    let native_services = match native_service_policy_path {
        Some(path) => Some(crate::host::dylib::NativeServicePolicy::load_file(path).await?),
        None => None,
    };
    #[cfg(feature = "wasm")]
    let wasm_messaging = match wasm_policy_path {
        Some(path) => Some(crate::host::wasm::WasmMessagingPolicy::load_file(path).await?),
        None => None,
    };
    #[cfg(feature = "wasm")]
    let wasm_services = match wasm_service_policy_path {
        Some(path) => Some(crate::host::wasm::WasmServicePolicy::load_file(path).await?),
        None => None,
    };
    let replay = if let Some((input, report)) = replay_paths {
        let mut options = crate::debug::ReplayOptions {
            direction: direction.unwrap_or_default(),
            tell_mode: tell_mode.unwrap_or(crate::debug::ReplayTellMode::Completion),
            target_node,
            ..Default::default()
        };
        if let Some(timeout) = call_timeout {
            options.call_timeout = timeout;
        }
        Some(replay_command::ReplayCommand::prepare(input, report, options, allow_legacy).await?)
    } else {
        None
    };
    let mut replay_result = None;
    let result = cmd_run(
        &manifest,
        bind.as_deref(),
        RunTracing {
            record: record.as_deref(),
            endpoint: trace_endpoint.as_deref(),
            allow_http: trace_allow_http,
            capacity: trace_capacity,
            socket: trace_socket.as_deref(),
        },
        RunHost {
            wasm_epoch_ticks,
            trusted_native: &trusted_native,
            #[cfg(feature = "dylib")]
            native_messaging,
            #[cfg(feature = "dylib")]
            native_services,
            budget,
            #[cfg(feature = "wasm")]
            wasm_messaging,
            #[cfg(feature = "wasm")]
            wasm_services,
        },
        replay_command::Execution {
            command: replay.as_ref(),
            result: &mut replay_result,
        },
    )
    .await;
    if let Some(replay) = replay {
        replay.finish(replay_result, &result).await?;
    }
    result
}
mod replay_command;
#[cfg(unix)]
mod trace_query;
mod trace_session;

struct RunTracing<'a> {
    record: Option<&'a Path>,
    endpoint: Option<&'a str>,
    allow_http: bool,
    capacity: Option<usize>,
    socket: Option<&'a Path>,
}

struct RunHost<'a> {
    wasm_epoch_ticks: Option<u64>,
    trusted_native: &'a [String],
    #[cfg(feature = "dylib")]
    native_messaging: Option<crate::host::dylib::NativeMessagingPolicy>,
    #[cfg(feature = "dylib")]
    native_services: Option<crate::host::dylib::NativeServicePolicy>,
    budget: Arc<GatewayProcessBudget>,
    #[cfg(feature = "wasm")]
    wasm_messaging: Option<crate::host::wasm::WasmMessagingPolicy>,
    #[cfg(feature = "wasm")]
    wasm_services: Option<crate::host::wasm::WasmServicePolicy>,
}

async fn cmd_run(
    path: &Path,
    bind: Option<&str>,
    tracing: RunTracing<'_>,
    host: RunHost<'_>,
    replay: replay_command::Execution<'_>,
) -> Result<(), Error> {
    let RunHost {
        wasm_epoch_ticks,
        trusted_native,
        #[cfg(feature = "dylib")]
        native_messaging,
        #[cfg(feature = "dylib")]
        native_services,
        budget,
        #[cfg(feature = "wasm")]
        wasm_messaging,
        #[cfg(feature = "wasm")]
        wasm_services,
    } = host;
    let replay_command::Execution {
        command: replay,
        result: replay_result,
    } = replay;
    #[cfg(not(feature = "wasm"))]
    let _ = wasm_epoch_ticks;
    #[cfg(not(feature = "dylib"))]
    let _ = trusted_native;
    let manifest = AppManifest::from_file(path)?;
    let mut base = parrot_config::ParrotConfig::builder().load_default_locations()?;
    if let Some(endpoint) = tracing.endpoint {
        base.tracing.otlp_endpoint = Some(endpoint.into());
        base.tracing.enabled = Some(true);
    }
    if tracing.allow_http {
        base.tracing.allow_insecure_http = Some(true);
    }
    if let Some(capacity) = tracing.capacity {
        base.tracing.capacity = Some(capacity);
    }
    if let Some(socket) = tracing.socket {
        base.tracing.socket = Some(socket.to_owned());
    }
    if let Some(bind) = bind {
        base.remote.node.bind = Some(bind.into());
    }
    base = base.with_fallback(
        parrot_config::ParrotConfig::builder()
            .remote_node_id("app-host")
            .remote_bind("127.0.0.1:19880"),
    );
    // Application overlay is the highest priority, including explicit --bind (Y3).
    let resolved = effective_config(&manifest, &base)?;
    let topology = LocalNodeTopology::from_resolved(&resolved);
    let plan = plan(&manifest, &topology)?;
    // Validate effective tracing before owning actors, sockets or files. Replay
    // keeps its original execution policy even when the manifest enables tracing.
    let trace_config = if replay.is_none() {
        trace_session::configuration(&resolved.tracing, &manifest.name)?
    } else {
        None
    };
    let trace_socket = resolved.tracing.socket.as_deref();
    let mut remote_config =
        RemoteConfig::from_resolved_async(&resolved, std::time::Duration::from_secs(5)).await?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    #[cfg(windows)]
    let mut interrupt = tokio::signal::windows::ctrl_c()?;
    let recording = if let Some(path) = tracing.record {
        let path = path.to_owned();
        Some(
            tokio::task::spawn_blocking(move || {
                crate::debug::Recording::start(&path, Default::default(), Default::default())
            })
            .await??,
        )
    } else {
        None
    };
    if let Some(recording) = &recording {
        remote_config = remote_config.with_capture(recording.capture());
    }
    let thread = ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&resolved));
    let routes = Arc::new(GatewayRoutes::default());
    let remote = match RemoteActorSystem::new(
        remote_config,
        Arc::new(ApplicationLookup {
            ts: thread.clone(),
            routes: routes.clone(),
        }),
    ) {
        Ok(remote) => remote,
        Err(error) => {
            let mut errors = vec![error.to_string()];
            if let Err(error) = thread.shutdown_internal().await {
                errors.push(error.to_string());
            }
            if let Some(recording) = recording {
                match tokio::task::spawn_blocking(move || recording.finish()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => errors.push(error.to_string()),
                    Err(error) => errors.push(error.to_string()),
                }
            }
            return Err(errors.join("; ").into());
        }
    };
    let root = std::env::var_os("PARROT_REPO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(find_repo_root);
    let gateway =
        ProcessGatewayFactory::new_with_routes_and_budget(root, remote.clone(), routes, budget);
    let spawner = ThreadPropsSpawner::with_remote(thread.clone(), remote.clone());
    #[cfg(feature = "dylib")]
    let spawner = match native_messaging {
        Some(policy) => spawner.with_dylib_messaging(policy.grants().to_vec())?,
        None => spawner,
    };
    #[cfg(feature = "dylib")]
    let spawner = match native_services {
        Some(policy) => spawner.with_dylib_services(policy.grants().to_vec())?,
        None => spawner,
    };
    #[cfg(feature = "wasm")]
    let spawner = match wasm_messaging {
        Some(policy) => spawner.with_wasm_messaging(policy.grants().to_vec())?,
        None => spawner,
    };
    #[cfg(feature = "wasm")]
    let spawner = match wasm_services {
        Some(policy) => spawner.with_wasm_services(policy.grants().to_vec())?,
        None => spawner,
    };
    #[cfg(feature = "dylib")]
    let spawner = {
        let mut spawner = spawner;
        for digest in trusted_native {
            // SAFETY: the operator explicitly authorizes this exact digest via
            // a CLI flag, accepting the documented native ABI/dependency trust
            // contract. The manifest alone cannot opt into native execution.
            spawner = unsafe { spawner.with_trusted_dylib(digest) }?;
        }
        spawner
    };
    #[cfg(feature = "wasm")]
    let spawner = if let Some(epoch_deadline) = wasm_epoch_ticks {
        spawner.with_wasm_limits(
            parrot_wasm::WasmConfig {
                epoch_deadline,
                ..Default::default()
            },
            Default::default(),
        )
    } else {
        spawner
    };
    let deployer = LocalDeployer {
        topology: &topology,
        gateway_factory: &gateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new().with_mirror_host(&remote);
    let mut trace_session = None;
    // Observe shutdown while startup owns partially deployed components, too.
    // Dropping assembly first cancels its pending operation; the same teardown
    // below then retires published components and unpublished gateway owners.
    let result: Result<(), Error> = tokio::select! {
        biased;
        signal = async {
            #[cfg(unix)]
            tokio::select! { _ = terminate.recv() => {}, _ = interrupt.recv() => {} }
            #[cfg(windows)]
            interrupt.recv().await;
            #[cfg(not(any(unix, windows)))]
            tokio::signal::ctrl_c().await?;
            Ok::<(), Error>(())
        } => if replay.is_some() { signal.and_then(|()| Err("replay interrupted; execution may already have occurred".into())) } else { signal },
        result = async {
            if let Some(config) = trace_config {
                trace_session = Some(trace_session::Session::start(config, &remote, trace_socket).await?);
            }
            tokio::select! {
                error = async {
                    match &trace_session {
                        Some(session) => session.failed().await,
                        None => std::future::pending().await,
                    }
                } => Err(error),
                result = async {
            let startup = async {
                remote.start().await?;
                context.assemble(&plan, &deployer, &manifest, &base).await?;
                if replay.is_none() {
                    let endpoint = if resolved.remote.node.scheme == "mem" {
                        format!("mem://{}", remote.config.node_id)
                    } else {
                        remote.local_addr().ok_or("host has no listening endpoint")?.to_string()
                    };
                    if let Some(path) = std::env::var_os("PARROT_APP_READY_FILE") {
                        publish_readiness(Path::new(&path), &endpoint)?;
                    }
                    use std::io::Write;
                    writeln!(std::io::stdout(), "PARROT_APP_READY={endpoint}")?;
                    std::io::stdout().flush()?;
                }
                Ok::<(), Error>(())
            };
            match &trace_session {
                Some(session) => session.startup(startup).await?,
                None => startup.await?,
            }
            if let Some(replay) = replay {
                let result = replay.replay(&context, &thread).await;
                let error = result.as_ref().err().map(ToString::to_string);
                *replay_result = Some(result);
                return match error { Some(error) => Err(error.into()), None => Ok(()) };
            }
            // Readiness was published; serve until the outer signal branch wins.
            std::future::pending::<Result<(), Error>>().await
                } => result,
            }
        } => result,
    };
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
    if let Some(recording) = recording {
        match tokio::task::spawn_blocking(move || recording.finish()).await {
            Ok(Ok(report)) => eprintln!(
                "[app] recorded {} frames to {}",
                report.written,
                report.path.display()
            ),
            Ok(Err(error)) => errors.push(error.to_string()),
            Err(error) => errors.push(error.to_string()),
        }
    }
    if let Err(error) = thread.shutdown_internal().await {
        errors.push(error.to_string());
    }
    if let Some(session) = trace_session {
        if let Err(error) = session.finish().await {
            errors.push(error.to_string());
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}
fn publish_readiness(path: &Path, endpoint: &str) -> Result<(), Error> {
    use std::io::Write;
    let mut name = path
        .file_name()
        .ok_or("ready file requires a filename")?
        .to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let temporary = path.with_file_name(name);
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(endpoint.as_bytes())?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(Into::into)
}
fn find_repo_root() -> PathBuf {
    let mut directory = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    loop {
        if directory.join("interop/erlang/parrot_gw.erl").exists() {
            return directory;
        }
        if !directory.pop() {
            return PathBuf::from(".");
        }
    }
}
