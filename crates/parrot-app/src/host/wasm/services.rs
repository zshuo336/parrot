//! Exact, operator-owned guest discovery authority bound after actor admission.
use super::*;
use parrot_api::receptionist::{
    ReceptionistEvent, ReceptionistGateway, ReceptionistKey, ReceptionistStream, RegistrationScope,
};
use parrot_wasm::runtime::{
    GuestServices, GuestSubscription, ServiceError, ServiceEvent, ServiceFuture,
};
use std::time::Duration;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum WasmServiceOperation {
    /// Publish and explicitly withdraw this actor's own registration.
    Register,
    Subscribe,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmServiceGrant {
    pub source_path: String,
    pub artifact_digest: String,
    pub key: String,
    pub operation: WasmServiceOperation,
}

pub(super) fn validate(grants: &mut [WasmServiceGrant]) -> Result<(), DeployError> {
    if grants.len() > 256 {
        return Err(DeployError::Config(
            "at most 256 Wasm service grants".into(),
        ));
    }
    let mut signatures = std::collections::BTreeSet::new();
    for grant in grants {
        let digest = grant
            .artifact_digest
            .strip_prefix("sha256:")
            .unwrap_or(&grant.artifact_digest);
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DeployError::ArtifactDigest(
                "Wasm service grant requires SHA-256".into(),
            ));
        }
        grant.artifact_digest = digest.to_ascii_lowercase();
        if !grant.source_path.starts_with('/')
            || grant.source_path.len() > 4096
            || grant.source_path.chars().any(char::is_control)
            || grant.key.len() > 1024
            || ReceptionistKey::new(grant.key.clone()).is_err()
        {
            return Err(DeployError::Config(
                "invalid Wasm service grant path or key".into(),
            ));
        }
        if !signatures.insert((
            grant.source_path.clone(),
            grant.artifact_digest.clone(),
            grant.key.clone(),
            grant.operation,
        )) {
            return Err(DeployError::Config(
                "overlapping Wasm service grants".into(),
            ));
        }
    }
    Ok(())
}

pub(super) fn for_component(
    grants: &[WasmServiceGrant],
    path: &str,
    digest: &str,
) -> Vec<WasmServiceGrant> {
    let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
    grants
        .iter()
        .filter(|grant| {
            grant.source_path == path && grant.artifact_digest.eq_ignore_ascii_case(digest)
        })
        .cloned()
        .collect()
}

pub(super) fn bind<A: Actor + Send + Sync + 'static>(
    ctx: &ThreadContext<A>,
    grants: Vec<WasmServiceGrant>,
) -> Option<Arc<dyn GuestServices>> {
    if grants.is_empty() {
        return None;
    }
    let gateway = ctx.system_weak_ref().upgrade()?.receptionist_gateway()?;
    Some(Arc::new(Services {
        path: ctx.path_str().into(),
        gateway,
        scope: ctx.receptionist_registration_scope(),
        grants,
    }))
}

struct Services {
    path: String,
    gateway: Arc<dyn ReceptionistGateway>,
    scope: Arc<RegistrationScope>,
    grants: Vec<WasmServiceGrant>,
}
impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("path", &self.path)
            .field("grants", &self.grants)
            .finish_non_exhaustive()
    }
}
impl Services {
    fn authorize(
        &self,
        key: String,
        operation: WasmServiceOperation,
    ) -> Result<ReceptionistKey, ServiceError> {
        if !self
            .grants
            .iter()
            .any(|grant| grant.key == key && grant.operation == operation)
        {
            return Err(ServiceError::Denied);
        }
        let key = ReceptionistKey::new(key).map_err(|_| ServiceError::InvalidRequest)?;
        self.scope
            .with_live(|| ())
            .map_err(|_| ServiceError::Closed)?;
        Ok(key)
    }
    async fn publish(
        &self,
        key: String,
        timeout_ms: u32,
        remove: bool,
    ) -> Result<(), ServiceError> {
        let key = self.authorize(key, WasmServiceOperation::Register)?;
        tokio::time::timeout(Duration::from_millis(timeout_ms.into()), async {
            if remove {
                self.gateway
                    .deregister_scoped(key, self.path.clone(), self.scope.clone())
                    .await
            } else {
                self.gateway
                    .register_scoped(key, self.path.clone(), self.scope.clone())
                    .await
            }
            .map_err(|_| ServiceError::Failed)
        })
        .await
        .map_err(|_| ServiceError::Timeout)?
    }
}
impl GuestServices for Services {
    fn register(&self, key: String, timeout_ms: u32) -> ServiceFuture<'_, ()> {
        Box::pin(self.publish(key, timeout_ms, false))
    }
    fn deregister(&self, key: String, timeout_ms: u32) -> ServiceFuture<'_, ()> {
        Box::pin(self.publish(key, timeout_ms, true))
    }
    fn subscribe(
        &self,
        key: String,
        timeout_ms: u32,
    ) -> ServiceFuture<'_, Box<dyn GuestSubscription>> {
        Box::pin(async move {
            let key = self.authorize(key, WasmServiceOperation::Subscribe)?;
            let stream = tokio::time::timeout(
                Duration::from_millis(timeout_ms.into()),
                self.gateway.subscribe(key),
            )
            .await
            .map_err(|_| ServiceError::Timeout)?
            .map_err(|_| ServiceError::Failed)?;
            // A delayed subscribe must not publish resources to a closed actor.
            self.scope
                .with_live(|| ())
                .map_err(|_| ServiceError::Closed)?;
            Ok(Box::new(ServiceStream {
                stream,
                scope: self.scope.clone(),
            }) as Box<dyn GuestSubscription>)
        })
    }
}

#[derive(Debug)]
struct ServiceStream {
    stream: ReceptionistStream,
    scope: Arc<RegistrationScope>,
}
impl GuestSubscription for ServiceStream {
    fn next(&mut self, timeout_ms: u32) -> ServiceFuture<'_, Option<ServiceEvent>> {
        Box::pin(async move {
            tokio::time::timeout(Duration::from_millis(timeout_ms.into()), async {
                tokio::select! {
                    biased;
                    () = self.scope.closed() => Err(ServiceError::Closed),
                    event = self.stream.recv() => Ok(event.map(|event| match event {
                        ReceptionistEvent::Registered { remote_path, .. } => ServiceEvent { registered: true, path: remote_path },
                        ReceptionistEvent::Unregistered { remote_path, .. } => ServiceEvent { registered: false, path: remote_path },
                    })),
                }
            }).await.map_err(|_| ServiceError::Timeout)?
        })
    }
}
