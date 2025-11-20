//! Trusted native components. Each instance has one bounded, owned OS worker;
//! native hooks never run on async scheduler threads. No hard interruption or
//! process-memory isolation is provided. A stuck hook retains its worker slot.
use super::DeployError;
use crate::assemble::{hook_phase, ParrotAppHook};
use parrot::thread::{
    config::ThreadActorConfig, context::ThreadContext, error::MailboxError,
    mailbox::admission::MessageAdmission, system::ThreadActorSystem,
};
use parrot_abi::{
    AbiMsg, AbiReplyBuf, AbiTraceParentV1, DylibLoader, LoadError, NativeArtifact,
    NativeArtifactLimits,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    trace::TraceContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

mod budget;
mod lifecycle;
mod messaging;
mod policy;
mod recording;
mod service_policy;
mod services;
pub use messaging::{NativeMessageGrant, NativeMessageOperation};
pub use policy::NativeMessagingPolicy;
pub use service_policy::NativeServicePolicy;
pub use services::{NativeServiceGrant, NativeServiceOperation};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};

/// Maximum live native workers per spawner and all its clones.
pub const MAX_WORKERS: usize = 32;
/// Per-call payload and output limits; these do not bound plugin allocations.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
/// UTF-8 message keys are bounded before native admission.
pub const MAX_KEY_BYTES: usize = 4096;
/// Maximum serialized constructor configuration, including host identity.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DylibAsk {
    pub type_key: String,
    pub payload: Vec<u8>,
}
/// ABI v1 has one message hook. Tell invokes it and discards its output.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DylibTell {
    pub type_key: String,
    pub payload: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DylibReply(pub Vec<u8>);
impl parrot_api::message::Message for DylibAsk {
    type Result = DylibReply;
}
impl parrot_api::message::Message for DylibTell {
    type Result = DylibReply;
}
macro_rules! codec {
    ($ty:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key, type_id: std::any::TypeId::of::<$ty>(),
                encode: |msg| parrot_api::message::serde_remote_serialize(msg.downcast_ref::<$ty>().ok_or("wrong Dylib wire type")?),
                decode: |bytes| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<$ty>(bytes)?) as BoxedMessage),
            }
        }
    };
}
codec!(DylibAsk, "bin:parrot.dylib.Ask#v1");
codec!(DylibTell, "bin:parrot.dylib.Tell#v1");
codec!(DylibReply, "bin:parrot.dylib.Reply#v1");

#[derive(Debug, Default)]
struct Admission(AtomicBool);
impl MessageAdmission for Admission {
    fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError> {
        if message.is::<ParrotAppHook>() {
            return Ok(message);
        }
        let invalid = |s: &str| MailboxError::PushError(s.into());
        if self.0.load(Ordering::Acquire) {
            return Err(invalid("dylib is draining; business admission is closed"));
        }
        let (key, payload) = if let Some(m) = message.downcast_ref::<DylibAsk>() {
            (&m.type_key, &m.payload)
        } else if let Some(m) = message.downcast_ref::<DylibTell>() {
            (&m.type_key, &m.payload)
        } else {
            return Err(invalid("expected DylibAsk or DylibTell"));
        };
        if key.len() > MAX_KEY_BYTES || payload.len() > MAX_MESSAGE_BYTES {
            return Err(invalid("dylib message exceeds host byte limit"));
        }
        Ok(message)
    }
}

type NativeResult = Result<(), String>;
enum Command {
    Message {
        key: String,
        payload: Vec<u8>,
        context: Option<TraceContext>,
        reply: oneshot::Sender<Result<Vec<u8>, String>>,
    },
    Drain {
        context: Option<TraceContext>,
        reply: oneshot::Sender<NativeResult>,
    },
    Stop,
}
#[derive(Clone)]
struct Control {
    commands: mpsc::Sender<Command>,
    finished: watch::Receiver<Option<NativeResult>>,
    stop_requested: Arc<lifecycle::StopRequest>,
    worker: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}
impl Control {
    fn request_stop(&self) {
        self.stop_requested.request();
        // A full queue already wakes the worker; it checks the flag before
        // executing another command. Never block a destructor on queue space.
        let _ = self.commands.try_send(Command::Stop);
    }
    fn join_blocking(&self) -> NativeResult {
        self.request_stop();
        let worker = self.worker.lock().unwrap().take();
        let joined = worker.map_or(Ok(()), |worker| {
            worker
                .join()
                .map_err(|_| "native worker panicked".to_string())
        });
        joined.and(
            self.finished.borrow().clone().unwrap_or_else(|| {
                Err("native worker exited without cleanup acknowledgement".into())
            }),
        )
    }
    async fn stop(&self) -> NativeResult {
        self.request_stop();
        let mut finished = self.finished.clone();
        loop {
            if let Some(result) = finished.borrow_and_update().clone() {
                return result;
            }
            finished
                .changed()
                .await
                .map_err(|_| "native worker exited without cleanup acknowledgement".to_string())?;
        }
    }
    async fn drain(&self) -> NativeResult {
        let context = TraceContext::current();
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Drain { context, reply })
            .await
            .map_err(|_| "native worker closed".to_string())?;
        result
            .await
            .map_err(|_| "native drain lost its acknowledgement".to_string())?
    }
    async fn message(&self, key: String, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        let context = TraceContext::current();
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Message {
                key,
                payload,
                context,
                reply,
            })
            .await
            .map_err(|_| "native worker closed".to_string())?;
        result
            .await
            .map_err(|_| "native message lost its acknowledgement".to_string())?
    }
}
struct NativeInitialization {
    path: String,
    artifact: NativeArtifact,
    config: String,
    permit: tokio::sync::OwnedSemaphorePermit,
    messaging: messaging::Binding,
    service_grants: Vec<NativeServiceGrant>,
}
type Startup = oneshot::Receiver<Result<(), DeployError>>;
struct DylibActor {
    control: Option<Control>,
    initialization: Option<NativeInitialization>,
    startup: Option<oneshot::Sender<Result<(), DeployError>>>,
    admission: Arc<Admission>,
    cleanup_reported: bool,
}
impl Drop for DylibActor {
    fn drop(&mut self) {
        // Failed actors deliberately skip before_stop. Managed Thread disposal
        // runs on an isolated cleanup worker; retain that owner until native
        // drain/destroy/unload and the native OS thread have really completed.
        if let Some(control) = &self.control {
            if let Err(error) = control.join_blocking() {
                if !self.cleanup_reported {
                    panic!("native worker retirement failed: {error}");
                }
            }
        }
    }
}
fn native_error(error: String) -> ActorError {
    ActorError::MessageHandlingError(format!("dylib: {error}"))
}
impl Actor for DylibActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let result = async {
                let mut initialization = self.initialization.take().ok_or_else(|| {
                    DeployError::Engine("native initialization already consumed".into())
                })?;
                // The engine reserves the actual actor path before invoking init.
                let services =
                    services::Binding::new(ctx, std::mem::take(&mut initialization.service_grants));
                let (control, started) = start_worker(initialization, services)?;
                // Retain the worker before suspension so failed or cancelled init
                // still joins native cleanup through this actor's owned disposal.
                self.control = Some(control);
                started
                    .await
                    .map_err(|_| DeployError::Engine("native constructor worker exited".into()))?
            }
            .await;
            if let Some(startup) = self.startup.take() {
                let _ = startup.send(result.clone());
            }
            result.map_err(|error| ActorError::InitializationError(error.to_string()))
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(hook) = msg.downcast_ref::<ParrotAppHook>() {
                match hook.0 {
                    hook_phase::START => {} // ABI construct already completed.
                    hook_phase::DRAIN | hook_phase::STOP => {
                        self.admission.0.store(true, Ordering::Release);
                        self.control
                            .as_ref()
                            .expect("initialized native worker")
                            .drain()
                            .await
                            .map_err(native_error)?;
                    }
                    _ => return Err(native_error("invalid hook phase".into())),
                }
                return Ok(Box::new(*hook) as BoxedMessage);
            }
            let (key, payload, tell) = if let Some(m) = msg.downcast_ref::<DylibAsk>() {
                (m.type_key.clone(), m.payload.clone(), false)
            } else if let Some(m) = msg.downcast_ref::<DylibTell>() {
                (m.type_key.clone(), m.payload.clone(), true)
            } else {
                return Err(native_error("unhandled message".into()));
            };
            let reply = self
                .control
                .as_ref()
                .expect("initialized native worker")
                .message(key, payload)
                .await
                .map_err(native_error)?;
            Ok(Box::new(DylibReply(if tell { Vec::new() } else { reply })) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.admission.0.store(true, Ordering::Release);
        Box::pin(async move {
            let result = match &self.control {
                Some(control) => control.stop().await,
                None => Ok(()),
            };
            self.cleanup_reported = result.is_err();
            result.map_err(native_error)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn load_error(error: LoadError) -> DeployError {
    match error {
        LoadError::Digest { .. } => DeployError::ArtifactDigest(error.to_string()),
        _ => DeployError::Artifact(error.to_string()),
    }
}
pub(super) fn digest_hex(digest: &str) -> Result<String, DeployError> {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DeployError::ArtifactDigest(
            "dylib requires a SHA-256 digest (64 hex digits)".into(),
        ));
    }
    Ok(hex.to_ascii_lowercase())
}
#[derive(Clone)]
pub(super) struct DylibHost {
    trusted: BTreeSet<String>,
    workers: Arc<Semaphore>,
    messaging: Vec<NativeMessageGrant>,
    services: Vec<NativeServiceGrant>,
}
impl Default for DylibHost {
    fn default() -> Self {
        Self {
            trusted: BTreeSet::new(),
            workers: Arc::new(Semaphore::new(MAX_WORKERS)),
            messaging: Vec::new(),
            services: Vec::new(),
        }
    }
}
impl DylibHost {
    pub(super) fn with_services(
        &self,
        mut grants: Vec<NativeServiceGrant>,
    ) -> Result<Self, DeployError> {
        services::validate(&mut grants)?;
        let mut host = self.clone();
        host.services = grants;
        Ok(host)
    }
    pub(super) fn with_messaging(
        &self,
        mut grants: Vec<NativeMessageGrant>,
    ) -> Result<Self, DeployError> {
        messaging::validate(&mut grants)?;
        let mut host = self.clone();
        host.messaging = grants;
        Ok(host)
    }
    pub(super) fn authorize(&self, digest: &str) -> Result<Self, DeployError> {
        let mut host = self.clone();
        host.trusted.insert(digest_hex(digest)?);
        Ok(host)
    }
    pub(super) fn validate(
        &self,
        digest: &str,
        uri: &str,
        abi: u32,
    ) -> Result<PathBuf, DeployError> {
        let digest = digest_hex(digest)?;
        if !self.trusted.contains(&digest) {
            return Err(DeployError::Artifact(
                "dylib digest has no explicit native trust authorization".into(),
            ));
        }
        if abi != parrot_abi::PARROT_ABI_VERSION {
            return Err(DeployError::Artifact(format!(
                "unsupported dylib ABI {abi}"
            )));
        }
        let url =
            url::Url::parse(uri).map_err(|e| DeployError::Artifact(format!("dylib URI: {e}")))?;
        if !uri.starts_with("file:///")
            || url.scheme() != "file"
            || url.host_str().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(DeployError::Artifact("dylib requires an absolute local file:/// URI without authority, query or fragment".into()));
        }
        url.to_file_path()
            .map_err(|_| DeployError::Artifact("invalid dylib file URI".into()))
    }
    pub(super) async fn spawn(
        &self,
        engine: &Arc<ThreadActorSystem>,
        reply: &oneshot::Sender<Result<super::references::OwnedReferences, DeployError>>,
        spec: (&str, &str, u32),
        config: Option<&toml::Table>,
        cleanup: &super::cleanup::Scope,
    ) -> Result<super::references::OwnedReferences, DeployError> {
        let (digest, uri, abi) = spec;
        let path = self.validate(digest, uri, abi)?;
        let count = u32::try_from(cleanup.paths.len())
            .map_err(|_| DeployError::Artifact("native worker capacity exceeded".into()))?;
        let mut permits = self
            .workers
            .clone()
            .try_acquire_many_owned(count)
            .map_err(|_| {
                DeployError::Artifact(format!("native worker capacity exceeded ({MAX_WORKERS})"))
            })?;
        // Preserve all business TOML values under config, without reserving or
        // overwriting any of their keys. Self identity has a separate envelope.
        #[derive(serde::Serialize)]
        struct ConstructorConfig<'a> {
            self_path: &'a str,
            config: &'a toml::Table,
        }
        let empty = toml::Table::new();
        let mut configs = Vec::new();
        for path in &cleanup.paths {
            let envelope = ConstructorConfig {
                self_path: path,
                config: config.unwrap_or(&empty),
            };
            let cfg =
                crate::encoding::canonical(&envelope, MAX_CONFIG_BYTES as u64).map_err(|e| {
                    DeployError::Config(format!("native constructor configuration: {e}"))
                })?;
            if cfg.len() > MAX_CONFIG_BYTES {
                return Err(DeployError::Config(
                    "native constructor configuration exceeds byte limit".into(),
                ));
            }
            configs.push(cfg);
        }
        if reply.is_closed() {
            return Err(DeployError::Engine("dylib caller cancelled".into()));
        }
        let digest = digest.to_owned();
        let preparation_digest = digest.clone();
        let artifact = engine
            .runtime_handle()
            .spawn_blocking(move || {
                NativeArtifact::prepare(&path, &preparation_digest, NativeArtifactLimits::default())
                    .map_err(load_error)
            })
            .await
            .map_err(|e| DeployError::Artifact(format!("native preparation worker: {e}")))??;
        let mut owned = super::references::OwnedReferences::with_capacity(cleanup.paths.len());
        for (path, cfg) in cleanup.paths.iter().zip(configs) {
            let result = async {
                if reply.is_closed() {
                    return Err(DeployError::Engine("dylib caller cancelled".into()));
                }
                let permit = permits.split(1).expect("reserved native worker");
                let admission = Arc::new(Admission::default());
                let (startup, started) = oneshot::channel();
                let actor = DylibActor {
                    control: None,
                    initialization: Some(NativeInitialization {
                        path: path.clone(),
                        artifact: artifact.clone(),
                        config: cfg,
                        permit,
                        messaging: messaging::Binding::new(engine, path, &digest, &self.messaging),
                        service_grants: self
                            .services
                            .iter()
                            .filter(|grant| {
                                grant.source_path == *path
                                    && grant.artifact_digest.eq_ignore_ascii_case(
                                        digest.strip_prefix("sha256:").unwrap_or(&digest),
                                    )
                            })
                            .cloned()
                            .collect(),
                    }),
                    startup: Some(startup),
                    admission: admission.clone(),
                    cleanup_reported: false,
                };
                let actor_config = ThreadActorConfig {
                    message_admission: Some(admission),
                    ..Default::default()
                };
                engine
                    .spawn_at(actor, path, None, actor_config)
                    .await
                    .map(|actor| (Box::new(actor) as BoxedActorRef, started))
                    .map_err(|error| DeployError::Engine(format!("spawn {path}: {error}")))
            }
            .await;
            match result {
                Ok((actor, started)) => {
                    owned.push(actor);
                    let result = started.await.unwrap_or_else(|_| {
                        Err(DeployError::Engine(
                            "native initializer retired before startup acknowledgement".into(),
                        ))
                    });
                    if let Err(error) = result {
                        return Err(cleanup.rollback(owned, error).await);
                    }
                }
                Err(error) => return Err(cleanup.rollback(owned, error).await),
            }
        }
        Ok(owned)
    }
}

fn start_worker(
    initialization: NativeInitialization,
    services: services::Binding,
) -> Result<(Control, Startup), DeployError> {
    let NativeInitialization {
        path,
        artifact,
        config,
        permit,
        messaging,
        service_grants: _,
    } = initialization;
    let (commands, mut receiver) = mpsc::channel(1);
    let (completed, finished) = watch::channel(None);
    let (ready, started) = oneshot::channel();
    let initialization_context = TraceContext::current();
    let stop_requested = Arc::new(lifecycle::StopRequest::default());
    let worker_stop = stop_requested.clone();
    let worker = std::thread::Builder::new()
        .name("parrot-native-actor".into())
        .spawn(move || {
            let mut services = services::State::new(services);
            // Never reuse guest span handles across construction, messages,
            // drain or destruction, including failed startup cleanup.
            let mut next_span_handle = 1;
            let result = (|| {
                // SAFETY: only the unsafe exact-digest authorization can reach here.
                // The worker exclusively owns the handle, instance, and call buffers.
                let handle = match lifecycle::run(
                    initialization_context.clone(),
                    "load",
                    &path,
                    || unsafe { artifact.load() },
                    lifecycle::load_status,
                ) {
                    Ok(handle) => handle,
                    Err(error) => {
                        let _ = ready.send(Err(load_error(error)));
                        return Ok(());
                    }
                };
                let component = match lifecycle::run(
                    initialization_context.clone(),
                    "construct",
                    &path,
                    || lifecycle::construct(&handle, &config, &mut next_span_handle),
                    lifecycle::load_status,
                ) {
                    Ok(component) => component,
                    Err(error) => {
                        let cleanup = lifecycle::run(
                            initialization_context.clone(),
                            "retire",
                            &path,
                            || lifecycle::unload(handle, &mut next_span_handle),
                            lifecycle::unload_status,
                        )
                        .map(|_| ())
                        .map_err(|error| error.to_string());
                        let _ = ready.send(Err(load_error(error)));
                        return cleanup;
                    }
                };
                let published = ready.send(Ok(())).is_ok();
                let mut drain_result = None;
                if published {
                    while let Some(command) = receiver.blocking_recv() {
                        if worker_stop.requested() {
                            break;
                        }
                        match command {
                            Command::Message {
                                key,
                                payload,
                                context,
                                reply,
                            } => {
                                let mut buffer = vec![0; MAX_MESSAGE_BYTES];
                                let mut out = AbiReplyBuf::new(&mut buffer);
                                let parent = context.as_ref().map(|context| AbiTraceParentV1 {
                                    trace_id: context.trace_id(),
                                    span_id: context.span_id(),
                                });
                                let budget = budget::Budget::default();
                                let mut messages =
                                    messaging.session_with_budget(context.clone(), budget.clone());
                                let message_host = messages.host();
                                let mut discovery = services.session(budget.clone());
                                let service_host = discovery.host();
                                let mut session =
                                    recording::Session::new(context, &mut next_span_handle);
                                let host = session.host();
                                let mut result = unsafe {
                                    DylibLoader.handle_msg_with_services(
                                        &handle,
                                        component,
                                        AbiMsg::new(&key, &payload),
                                        &mut out,
                                        parent.as_ref(),
                                        Some(&host),
                                        Some(&message_host),
                                        Some(&service_host),
                                    )
                                };
                                messages.complete(&mut result);
                                if result.is_ok() && out.written as usize > buffer.len() {
                                    result = Err((
                                        parrot_abi::ABI_ERR_OVERFLOW,
                                        "native reply length exceeds host buffer".into(),
                                    ));
                                }
                                session.complete(&mut result);
                                let result = result
                                    .map_err(|(code, detail)| {
                                        format!("native code {code}: {detail}")
                                    })
                                    .map(|()| {
                                        buffer.truncate(out.written as usize);
                                        buffer
                                    });
                                let _ = reply.send(result);
                            }
                            Command::Drain { context, reply } => {
                                let result = drain_result
                                    .get_or_insert_with(|| {
                                        lifecycle::run(
                                            context,
                                            "drain",
                                            &path,
                                            || unsafe {
                                                lifecycle::drain(
                                                    &handle,
                                                    component,
                                                    &mut next_span_handle,
                                                )
                                            },
                                            lifecycle::drain_status,
                                        )
                                        .map_err(
                                            |(code, detail)| {
                                                format!("native code {code}: {detail}")
                                            },
                                        )
                                    })
                                    .clone();
                                let _ = reply.send(result);
                            }
                            Command::Stop => break,
                        }
                    }
                }
                let retirement_context = if worker_stop.requested() {
                    worker_stop.take_context()
                } else if !published {
                    initialization_context.clone()
                } else {
                    None
                };
                lifecycle::run(
                    retirement_context,
                    "retire",
                    &path,
                    || {
                        if drain_result.is_none() {
                            let _ = lifecycle::run(
                                TraceContext::current(),
                                "drain",
                                &path,
                                || unsafe {
                                    lifecycle::drain(&handle, component, &mut next_span_handle)
                                },
                                lifecycle::drain_status,
                            );
                        }
                        lifecycle::unload(handle, &mut next_span_handle)
                    },
                    lifecycle::unload_status,
                )
                .map(|_| ())
                .map_err(|e| e.to_string())
            })();
            let result = result.and(services.retire());
            drop(services);
            drop(artifact);
            drop(permit);
            // Receipt follows drain, destroy, dlclose and release of this worker's
            // staging reference and capacity. Siblings may still own the snapshot.
            completed.send_replace(Some(result));
        })
        .map_err(|e| DeployError::Engine(format!("native worker spawn: {e}")))?;
    let control = Control {
        commands,
        finished,
        stop_requested,
        worker: Arc::new(Mutex::new(Some(worker))),
    };
    Ok((control, started))
}
