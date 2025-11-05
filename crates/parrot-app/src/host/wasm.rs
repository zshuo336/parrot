//! Feature-gated Wasm manifest host. Only local `file://` artifacts with a
//! mandatory SHA-256 are admitted. Reading, verification, compilation and guest
//! initialization run off the async executor; one pool is prepared at a time
//! per spawner (including its clones). Guest memory and retained compiler cache
//! have separate limits, neither of which bounds total process RSS.
//!
//! `config-get` returns strings unchanged and other top-level TOML values as
//! TOML value text, including arrays and inline tables. It never drops nested
//! business configuration or consumes business keys as host resource settings.

use super::DeployError;
use crate::assemble::{hook_phase, ParrotAppHook};
use futures::FutureExt;
use parrot::thread::{
    config::ThreadActorConfig, context::ThreadContext, error::MailboxError,
    mailbox::admission::MessageAdmission, system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    trace::TraceContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_wasm::{
    runtime::{CompilationLimits, WasmComponent, WasmRuntime},
    ComponentError, HostCtx, TraceParent, WasmConfig,
};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
};

/// Invoke WIT `handle`; the guest controls the payload encoding.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WasmAsk {
    pub type_key: String,
    pub payload: Vec<u8>,
}
/// Raw bytes returned by WIT `handle` (also an empty completion for `WasmTell`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WasmReply(pub Vec<u8>);
/// Invoke WIT `tell`. Delivery acknowledgement alone does not confirm execution.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WasmTell {
    pub type_key: String,
    pub payload: Vec<u8>,
}
impl parrot_api::message::Message for WasmAsk {
    type Result = WasmReply;
}
impl parrot_api::message::Message for WasmTell {
    type Result = WasmReply;
}

macro_rules! codec {
    ($ty:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key, type_id: std::any::TypeId::of::<$ty>(),
                encode: |msg| parrot_api::message::serde_remote_serialize(msg.downcast_ref::<$ty>().ok_or("wrong Wasm wire type")?),
                decode: |bytes| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<$ty>(bytes)?) as BoxedMessage),
            }
        }
    };
}
codec!(WasmAsk, "bin:parrot.wasm.Ask#v1");
codec!(WasmTell, "bin:parrot.wasm.Tell#v1");
codec!(WasmReply, "bin:parrot.wasm.Reply#v1");

#[derive(Debug, Default)]
struct Admission(AtomicBool);
impl MessageAdmission for Admission {
    fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError> {
        if message.is::<ParrotAppHook>() {
            return Ok(message);
        }
        if self.0.load(Ordering::Acquire) {
            return Err(MailboxError::PushError(
                "wasm is draining; business admission is closed".into(),
            ));
        }
        if !message.is::<WasmAsk>() && !message.is::<WasmTell>() {
            return Err(MailboxError::PushError(
                "expected WasmAsk or WasmTell".into(),
            ));
        }
        Ok(message)
    }
}

/// One sandbox per actor. Construction runs `on-start`; START acknowledges that
/// successful initialization. DRAIN/STOP and `before_stop` share one idempotent
/// `on-drain` attempt, including its error. WIT has no `on-stop` operation.
///
/// Host-created actors reject new business admission after drain. Work already
/// admitted can finish; `on-drain` is a cooperative notification, not a barrier
/// for concurrent producers. Stop retires the mailbox. Guest errors use the
/// engine's configured supervision; Resume is never implicitly selected.
pub struct WasmActor {
    component: WasmComponent,
    admission: Arc<Admission>,
    drain_result: Option<Result<(), ComponentError>>,
}
impl WasmActor {
    /// Construct from an instantiated component and run its start hook once.
    pub fn new(mut component: WasmComponent) -> ActorResult<Self> {
        component
            .on_start()
            .map_err(|e| ActorError::InitializationError(format!("wasm on_start: {e}")))?;
        Ok(Self {
            component,
            admission: Arc::default(),
            drain_result: None,
        })
    }
    /// Take accumulated guest metrics, resetting the counters.
    pub fn take_metrics(&mut self) -> parrot_wasm::WasmMetrics {
        self.component.take_metrics()
    }
    /// Close business admission and attempt the guest drain hook exactly once.
    pub fn drain(&mut self) -> Result<(), ComponentError> {
        self.admission.0.store(true, Ordering::Release);
        self.drain_result
            .get_or_insert_with(|| self.component.on_drain())
            .clone()
    }
    /// Use this policy when embedding the adapter directly via `spawn_at` to
    /// reject new business requests after drain without invoking supervision.
    pub fn message_admission(&self) -> Arc<dyn MessageAdmission> {
        self.admission.clone()
    }
}
fn guest_error(e: ComponentError) -> ActorError {
    ActorError::MessageHandlingError(match e {
        ComponentError::OutOfFuel => "wasm OverQuota: out of fuel".into(),
        e => format!("wasm trap: {e}"),
    })
}
impl Actor for WasmActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(hook) = msg.downcast_ref::<ParrotAppHook>() {
                match hook.0 {
                    hook_phase::START => {}
                    hook_phase::DRAIN | hook_phase::STOP => self.drain().map_err(guest_error)?,
                    _ => {
                        return Err(ActorError::MessageHandlingError(
                            "invalid Wasm hook phase".into(),
                        ))
                    }
                }
                return Ok(Box::new(*hook) as BoxedMessage);
            }
            if let Some(ask) = msg.downcast_ref::<WasmAsk>() {
                let parent = TraceContext::current().map(|context| TraceParent {
                    trace_id: context.trace_id(),
                    span_id: context.span_id(),
                });
                return self
                    .component
                    .handle_with_trace(&ask.type_key, &ask.payload, parent.as_ref())
                    .map(|bytes| Box::new(WasmReply(bytes)) as BoxedMessage)
                    .map_err(guest_error);
            }
            if let Some(tell) = msg.downcast_ref::<WasmTell>() {
                let parent = TraceContext::current().map(|context| TraceParent {
                    trace_id: context.trace_id(),
                    span_id: context.span_id(),
                });
                self.component
                    .tell_with_trace(&tell.type_key, &tell.payload, parent.as_ref())
                    .map_err(guest_error)?;
                return Ok(Box::new(WasmReply(Vec::new())) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError(
                "wasm actor: unhandled message".into(),
            ))
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { self.drain().map_err(guest_error) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

pub(super) fn validate_artifact(
    digest: &str,
    uri: &str,
) -> Result<(PathBuf, [u8; 32]), DeployError> {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DeployError::ArtifactDigest(
            "wasm requires a SHA-256 digest (64 hex digits, optionally prefixed sha256:)".into(),
        ));
    }
    let mut expected = [0u8; 32];
    for (index, byte) in expected.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).expect("validated hex");
    }
    let url =
        url::Url::parse(uri).map_err(|e| DeployError::Artifact(format!("wasm file URI: {e}")))?;
    if !uri.starts_with("file:///")
        || url.scheme() != "file"
        || url.host_str().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(DeployError::Artifact(
            "wasm requires an absolute local file:/// URI without authority, query or fragment"
                .into(),
        ));
    }
    let path = url
        .to_file_path()
        .map_err(|_| DeployError::Artifact("invalid wasm file URI".into()))?;
    if !path.is_absolute() {
        return Err(DeployError::Artifact(
            "wasm file path must be absolute".into(),
        ));
    }
    Ok((path, expected))
}

pub(super) struct WasmHost {
    config: WasmConfig,
    limits: CompilationLimits,
    runtime: OnceLock<Result<Arc<WasmRuntime>, String>>,
    preparation: Arc<tokio::sync::Semaphore>,
}
impl Default for WasmHost {
    fn default() -> Self {
        Self::new(WasmConfig::default(), CompilationLimits::default())
    }
}
impl WasmHost {
    pub(super) fn new(config: WasmConfig, limits: CompilationLimits) -> Self {
        Self {
            config,
            limits,
            runtime: OnceLock::new(),
            preparation: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
    fn runtime(&self) -> Result<Arc<WasmRuntime>, DeployError> {
        self.runtime
            .get_or_init(|| {
                WasmRuntime::new_with_compilation_limits(self.config.clone(), self.limits)
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
            .clone()
            .map_err(DeployError::Artifact)
    }
    fn read_verified(
        &self,
        path: &std::path::Path,
        expected: [u8; 32],
    ) -> Result<Arc<[u8]>, DeployError> {
        // Validate runtime limits before using them in arithmetic or reading.
        let _runtime = self.runtime()?;
        let read = || -> Result<Vec<u8>, Box<dyn std::error::Error>> {
            let mut open = std::fs::OpenOptions::new();
            open.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                open.custom_flags(libc::O_NONBLOCK);
            }
            let file = open.open(path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err("expected a regular file".into());
            }
            let limit = self.limits.max_input_bytes;
            if metadata.len() > limit as u64 {
                return Err("component input exceeds byte limit".into());
            }
            let mut bytes = Vec::new();
            file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > limit {
                return Err("component input exceeds byte limit".into());
            }
            Ok(bytes)
        };
        let bytes =
            read().map_err(|e| DeployError::Artifact(format!("wasm {}: {e}", path.display())))?;
        if <[u8; 32]>::from(Sha256::digest(&bytes)) != expected {
            return Err(DeployError::ArtifactDigest(format!(
                "wasm {}: SHA-256 digest mismatch",
                path.display()
            )));
        }
        Ok(Arc::from(bytes))
    }
    pub(super) async fn spawn(
        self: &Arc<Self>,
        engine: &Arc<ThreadActorSystem>,
        reply: &tokio::sync::oneshot::Sender<
            Result<super::references::OwnedReferences, DeployError>,
        >,
        digest: &str,
        uri: &str,
        config: Option<&toml::Table>,
        cleanup: &super::cleanup::Scope,
    ) -> Result<super::references::OwnedReferences, DeployError> {
        let paths = &cleanup.paths;
        let (path, expected) = validate_artifact(digest, uri)?;
        let _permit = self
            .preparation
            .acquire()
            .await
            .map_err(|e| DeployError::Engine(e.to_string()))?;
        if reply.is_closed() {
            return Err(DeployError::Engine("wasm caller cancelled".into()));
        }
        let host = self.clone();
        let bytes = engine
            .runtime_handle()
            .spawn_blocking(move || host.read_verified(&path, expected))
            .await
            .map_err(|e| DeployError::Artifact(format!("wasm read worker: {e}")))??;
        let mut overlay = parrot_wasm::toml_table::Table::default();
        for (key, value) in config.into_iter().flatten() {
            overlay.insert(
                key,
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string()),
            );
        }
        let mut owned = super::references::OwnedReferences::with_capacity(paths.len());
        for path in paths {
            let result = std::panic::AssertUnwindSafe(async {
                if reply.is_closed() {
                    return Err(DeployError::Engine("wasm caller cancelled".into()));
                }
                let host = self.clone();
                let bytes = bytes.clone();
                let ctx = HostCtx {
                    self_path: path.clone(),
                    config_overlay: overlay.clone(),
                    ..Default::default()
                };
                let actor = engine
                    .runtime_handle()
                    .spawn_blocking(move || {
                        let component = host
                            .runtime()?
                            .instantiate_bytes(&bytes, ctx)
                            .map_err(|e| DeployError::Artifact(e.to_string()))?;
                        WasmActor::new(component).map_err(|e| DeployError::Engine(e.to_string()))
                    })
                    .await
                    .map_err(|e| {
                        DeployError::Engine(format!("wasm initialization worker: {e}"))
                    })??;
                let config = ThreadActorConfig {
                    message_admission: Some(actor.message_admission()),
                    ..Default::default()
                };
                engine
                    .spawn_at(actor, path, None, config)
                    .await
                    .map(|actor| Box::new(actor) as BoxedActorRef)
                    .map_err(|e| DeployError::Engine(format!("spawn {path}: {e}")))
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(DeployError::Engine(format!(
                    "wasm spawn panicked at {path}"
                )))
            });
            match result {
                Ok(actor) => owned.push(actor),
                Err(error) => {
                    return Err(cleanup.rollback(owned, error).await);
                }
            }
        }
        Ok(owned)
    }
}
