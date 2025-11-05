//! Application-owned local and Wire mirrors. Raw host lookup remains unwrapped.
use crate::manifest::{AppManifest, EngineKind};
use parrot_api::{
    address::ActorRef,
    message::MessageOptions,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::mirror::{WireMirrorHandle, WireMirrorOptions, WireMirrorPolicy};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorSpec {
    /// Component name. Every declared source instance gets its own budget.
    pub source: String,
    /// Local Parrot component. One shadow instance or the same count as source.
    pub shadow: String,
    #[serde(default = "capacity")]
    pub capacity: usize,
    #[serde(default = "message_bytes")]
    pub max_message_bytes: usize,
    #[serde(default = "pending_bytes")]
    pub max_pending_bytes: usize,
    #[serde(default = "timeout_ms")]
    pub delivery_timeout_ms: u64,
}
fn capacity() -> usize {
    256
}
fn message_bytes() -> usize {
    1024 * 1024
}
fn pending_bytes() -> usize {
    8 * 1024 * 1024
}
fn timeout_ms() -> u64 {
    100
}
impl MirrorSpec {
    pub fn new(source: impl Into<String>, shadow: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            shadow: shadow.into(),
            capacity: capacity(),
            max_message_bytes: message_bytes(),
            max_pending_bytes: pending_bytes(),
            delivery_timeout_ms: timeout_ms(),
        }
    }
    pub fn options(&self) -> WireMirrorOptions {
        WireMirrorOptions {
            capacity: self.capacity,
            max_message_bytes: self.max_message_bytes,
            max_pending_bytes: self.max_pending_bytes,
            delivery_timeout: Duration::from_millis(self.delivery_timeout_ms),
        }
    }
}
/// Validate before instance expansion. The registry's fixed 64-policy limit also
/// bounds expansion independently of a host's broader application limits.
pub(crate) fn validate(manifest: &AppManifest) -> Result<(), String> {
    if manifest.mirrors.len() > 64 {
        return Err("at most 64 mirror policies".into());
    }
    let mut sources = std::collections::BTreeSet::new();
    let mut count = 0usize;
    for policy in &manifest.mirrors {
        if policy.source == policy.shadow || !sources.insert(policy.source.as_str()) {
            return Err("mirror source must be unique and distinct from shadow".into());
        }
        let source = manifest
            .component(&policy.source)
            .ok_or("unknown mirror source component")?;
        let shadow = manifest
            .component(&policy.shadow)
            .ok_or("unknown mirror shadow component")?;
        if shadow.engine != EngineKind::Parrot {
            return Err("mirror shadow must be a local Parrot component".into());
        }
        if source.name.len() > 4080 || shadow.name.len() > 4080 {
            return Err("mirror instance path exceeds the Wire path limit".into());
        }
        let n = source.instances.instance_count();
        let m = shadow.instances.instance_count();
        if n == 0 || (m != 1 && m != n) {
            return Err("mirror requires one shadow instance or equal source/shadow counts".into());
        }
        count = count
            .checked_add(n)
            .ok_or("mirror instance count overflow")?;
        if count > 64 {
            return Err("at most 64 expanded mirror policies".into());
        }
        policy.options().validate().map_err(|e| e.to_string())?;
    }
    Ok(())
}
pub(crate) fn expand(
    manifest: &AppManifest,
) -> Result<Vec<(WireMirrorPolicy, WireMirrorOptions)>, String> {
    validate(manifest)?;
    let mut policies = Vec::new();
    for policy in &manifest.mirrors {
        let source = manifest
            .component(&policy.source)
            .expect("validated source")
            .instance_paths()
            .map_err(|e| e.to_string())?;
        let shadow = manifest
            .component(&policy.shadow)
            .expect("validated shadow")
            .instance_paths()
            .map_err(|e| e.to_string())?;
        for (index, path) in source.into_iter().enumerate() {
            let target = &shadow[if shadow.len() == 1 { 0 } else { index }];
            let wire = WireMirrorPolicy::new(path, target);
            wire.validate().map_err(|e| e.to_string())?;
            policies.push((wire, policy.options()));
        }
    }
    Ok(policies)
}
pub(crate) struct MirroredRef {
    pub target: BoxedActorRef,
    pub path: String,
    pub handle: WireMirrorHandle,
}
impl std::fmt::Debug for MirroredRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplicationMirrorRef")
            .field("path", &self.path)
            .finish()
    }
}
fn remaining(
    start: tokio::time::Instant,
    budget: Option<Duration>,
) -> ActorResult<Option<Duration>> {
    budget
        .map(|budget| {
            budget
                .checked_sub(start.elapsed())
                .filter(|d| !d.is_zero())
                .ok_or(parrot_api::errors::ActorError::Timeout)
        })
        .transpose()
}
impl ActorRef for MirroredRef {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.handle.offer_local(&self.path, &message);
            self.target.send(message).await
        }))
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            let start = tokio::time::Instant::now();
            self.handle.offer_local(&self.path, &message);
            self.target
                .send_with_timeout(message, remaining(start, timeout)?)
                .await
        }))
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.handle.offer_local(&self.path, &message);
            self.target.deliver(message).await
        }))
    }
    fn send_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        mut options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            options.validate_single_attempt()?;
            let start = tokio::time::Instant::now();
            self.handle.offer_local(&self.path, &message);
            options.timeout = remaining(start, options.timeout)?;
            self.target.send_with_options(message, options).await
        }))
    }
    fn deliver_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        mut options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            options.validate_single_attempt()?;
            let start = tokio::time::Instant::now();
            self.handle.offer_local(&self.path, &message);
            options.timeout = remaining(start, options.timeout)?;
            self.target.deliver_with_options(message, options).await
        }))
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        self.target.stop()
    }
    fn path(&self) -> String {
        self.path.clone()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.target.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            target: self.target.clone_boxed(),
            path: self.path.clone(),
            handle: self.handle.clone(),
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
