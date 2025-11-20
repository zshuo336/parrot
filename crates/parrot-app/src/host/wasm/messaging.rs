//! Host-owned exact grants. Guest data can never create or widen authority.
use super::*;
use parrot_remote::codec_registry::CodecRegistry;
use parrot_wasm::runtime::{
    GuestMessaging, MessagingError, MessagingRequest, Msg, MAX_MESSAGE_BYTES,
};
use std::{future::Future, pin::Pin, sync::Weak, time::Duration};

/// One exact messaging permission for a verified guest artifact and actor path.
/// Reply authorization applies before encoding the actual ASK response.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmMessageGrant {
    pub source_path: String,
    pub artifact_digest: String,
    pub destination: String,
    pub request_type_key: String,
    pub operation: WasmMessageOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum WasmMessageOperation {
    Ask { reply_type_key: String },
    Tell,
}

impl<'de> serde::Deserialize<'de> for WasmMessageOperation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A tagged unit variant otherwise accepts extra fields in serde.
        #[derive(serde::Deserialize)]
        #[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
        enum Operation {
            Ask { reply_type_key: String },
            Tell {},
        }
        Ok(
            match <Operation as serde::Deserialize>::deserialize(deserializer)? {
                Operation::Ask { reply_type_key } => Self::Ask { reply_type_key },
                Operation::Tell {} => Self::Tell,
            },
        )
    }
}

pub(super) fn validate(grants: &mut [WasmMessageGrant]) -> Result<(), DeployError> {
    if grants.len() > 256 {
        return Err(DeployError::Artifact(
            "at most 256 Wasm messaging grants".into(),
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
                "Wasm messaging grant requires SHA-256".into(),
            ));
        }
        grant.artifact_digest = digest.to_ascii_lowercase();
        if !grant.source_path.starts_with('/')
            || grant.source_path.len() > 4096
            || grant.source_path.chars().any(char::is_control)
            || grant.source_path == grant.destination
            || grant.destination.is_empty()
            || grant.destination.len() > 4096
            || grant.destination.chars().any(char::is_control)
        {
            return Err(DeployError::Artifact(
                "invalid Wasm messaging grant path".into(),
            ));
        }
        let valid_key = |key: &str| {
            key.len() <= 1024 && parrot_remote::codec::CodecStack::of_type_key(key).is_ok()
        };
        if !valid_key(&grant.request_type_key)
            || matches!(&grant.operation, WasmMessageOperation::Ask { reply_type_key } if !valid_key(reply_type_key))
        {
            return Err(DeployError::Artifact(
                "invalid Wasm messaging grant codec key".into(),
            ));
        }
        if !signatures.insert((
            grant.source_path.clone(),
            grant.artifact_digest.clone(),
            grant.destination.clone(),
            grant.request_type_key.clone(),
            matches!(grant.operation, WasmMessageOperation::Ask { .. }),
        )) {
            return Err(DeployError::Artifact(
                "overlapping Wasm messaging grants".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct Messaging {
    engine: Weak<ThreadActorSystem>,
    grants: Vec<WasmMessageGrant>,
}
impl Messaging {
    pub(super) fn bind(
        engine: &Arc<ThreadActorSystem>,
        source: &str,
        digest: &str,
        grants: &[WasmMessageGrant],
    ) -> Option<Arc<dyn GuestMessaging>> {
        let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
        let grants: Vec<_> = grants
            .iter()
            .filter(|grant| {
                grant.source_path == source && grant.artifact_digest.eq_ignore_ascii_case(digest)
            })
            .cloned()
            .collect();
        (!grants.is_empty()).then(|| {
            Arc::new(Self {
                engine: Arc::downgrade(engine),
                grants,
            }) as Arc<dyn GuestMessaging>
        })
    }

    fn authorize(
        &self,
        request: &MessagingRequest,
        ask: bool,
    ) -> Result<&WasmMessageGrant, MessagingError> {
        self.grants
            .iter()
            .find(|grant| {
                grant.destination == request.destination
                    && grant.request_type_key == request.message.type_key
                    && matches!(
                        (&grant.operation, ask),
                        (WasmMessageOperation::Ask { .. }, true)
                            | (WasmMessageOperation::Tell, false)
                    )
            })
            .ok_or(MessagingError::Denied)
    }

    fn target(
        &self,
        request: &MessagingRequest,
    ) -> Result<(BoxedActorRef, BoxedMessage), MessagingError> {
        let registry = CodecRegistry::global();
        if registry.get(&request.message.type_key).is_none() {
            return Err(MessagingError::UnknownType);
        }
        let message = registry
            .decode_incoming(&request.message.type_key, &request.message.payload)
            .map_err(|_| MessagingError::Codec)?;
        // Lifecycle control cannot be acquired through a business codec alias.
        if message.is::<ParrotAppHook>() {
            return Err(MessagingError::Denied);
        }
        let actor = self
            .engine
            .upgrade()
            .and_then(|engine| engine.resolve_actor(&request.destination))
            .ok_or(MessagingError::UnknownTarget)?;
        Ok((actor, message))
    }
}
impl GuestMessaging for Messaging {
    fn ask(
        &self,
        request: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Msg, MessagingError>> + Send + '_>> {
        Box::pin(async move {
            let grant = self.authorize(&request, true)?;
            tokio::time::timeout(Duration::from_millis(u64::from(request.timeout_ms)), async {
                let (actor, message) = self.target(&request)?;
                let response = actor.send(message).await.map_err(|_| MessagingError::Failed)?;
                let registry = CodecRegistry::global();
                let key = registry.key_of((*response).type_id()).ok_or(MessagingError::UnexpectedReply)?;
                if !matches!(&grant.operation, WasmMessageOperation::Ask { reply_type_key } if reply_type_key == key) {
                    return Err(MessagingError::UnexpectedReply);
                }
                let (_, payload) = registry.encode_outgoing(&response).map_err(|_| MessagingError::Codec)?;
                if payload.len() > MAX_MESSAGE_BYTES { return Err(MessagingError::Limit); }
                Ok(Msg { type_key: key.into(), payload })
            }).await.map_err(|_| MessagingError::Timeout)?
        })
    }
    fn tell(
        &self,
        request: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessagingError>> + Send + '_>> {
        Box::pin(async move {
            self.authorize(&request, false)?;
            tokio::time::timeout(
                Duration::from_millis(u64::from(request.timeout_ms)),
                async {
                    let (actor, message) = self.target(&request)?;
                    actor
                        .deliver(message)
                        .await
                        .map_err(|_| MessagingError::Failed)
                },
            )
            .await
            .map_err(|_| MessagingError::Timeout)?
        })
    }
}
