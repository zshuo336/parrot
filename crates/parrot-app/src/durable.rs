//! Manifest configuration and Wire messages for the owned CloudProxy actor.
use bytes::Bytes;
use parrot_remote::{
    durable::{OutboxConfig, WalLimits},
    NodeAddr,
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

pub const CLOUD_PROXY_FACTORY: &str = "parrot.cloud-proxy";
pub const CLOUD_PROXY_MAX_BATCH_RECORDS: usize = 256;
/// Logical bookkeeping charge in addition to compact key/payload byte lengths.
pub const CLOUD_PROXY_REQUEST_OVERHEAD: usize = 128;
pub const CLOUD_PROXY_RECORD_OVERHEAD: usize = 64;

/// One component owns one receiver/path/session and one absolute storage directory.
/// Numeric TCP/QUIC endpoints require an explicit node ID; mem endpoints are local.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloudProxyConfig {
    pub directory: PathBuf,
    pub receiver: String,
    pub path: String,
    pub session: String,
    pub transactional: bool,
    /// Persist caller-chosen request IDs in a v2 outbox directory binding.
    pub idempotent_submissions: bool,
    pub max_submission_receipts: usize,
    pub max_submission_receipt_bytes: usize,
    pub mailbox_capacity: usize,
    /// Aggregate admission charge held through queueing and handler completion.
    pub max_inflight_bytes: usize,
    /// Per-request charge, including all members of an explicit batch.
    pub max_request_bytes: usize,
    pub max_pending_records: usize,
    pub max_pending_bytes: usize,
    pub max_disk_bytes: u64,
    pub segment_bytes: u64,
    pub group_commit_max_records: usize,
    pub group_commit_interval_ms: u64,
    pub attempt_timeout_ms: u64,
    pub retry_min_ms: u64,
    pub retry_max_ms: u64,
    pub drain_timeout_ms: u64,
}
impl Default for CloudProxyConfig {
    fn default() -> Self {
        let outbox = OutboxConfig::default();
        Self {
            directory: PathBuf::new(),
            receiver: String::new(),
            path: String::new(),
            session: String::new(),
            transactional: true,
            idempotent_submissions: false,
            max_submission_receipts: outbox.wal_limits.max_submission_receipts,
            max_submission_receipt_bytes: outbox.wal_limits.max_submission_receipt_bytes,
            mailbox_capacity: 1024,
            max_inflight_bytes: 64 * 1024 * 1024,
            max_request_bytes: 16 * 1024 * 1024
                + CLOUD_PROXY_REQUEST_OVERHEAD
                + CLOUD_PROXY_MAX_BATCH_RECORDS * CLOUD_PROXY_RECORD_OVERHEAD,
            max_pending_records: outbox.wal_limits.max_pending_records,
            max_pending_bytes: outbox.wal_limits.max_pending_bytes,
            max_disk_bytes: outbox.wal_limits.max_disk_bytes,
            segment_bytes: outbox.segment_bytes,
            group_commit_max_records: outbox.group_commit_max_records,
            group_commit_interval_ms: 10,
            attempt_timeout_ms: 10_000,
            retry_min_ms: 100,
            retry_max_ms: 30_000,
            drain_timeout_ms: 5_000,
        }
    }
}
impl CloudProxyConfig {
    pub fn from_table(table: Option<&toml::Table>) -> Result<Self, String> {
        let config: Self = toml::Value::Table(table.cloned().ok_or("cloud proxy config required")?)
            .try_into()
            .map_err(|e: toml::de::Error| e.to_string())?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<NodeAddr, String> {
        let receiver: NodeAddr = self.receiver.parse()?;
        if !receiver.identity_is_explicit
            || !self.directory.is_absolute()
            || !(1..=65536).contains(&self.mailbox_capacity)
            || !(1..=1024 * 1024 * 1024).contains(&self.max_inflight_bytes)
            || self.max_inflight_bytes > tokio::sync::Semaphore::MAX_PERMITS
            || !(CLOUD_PROXY_REQUEST_OVERHEAD + CLOUD_PROXY_RECORD_OVERHEAD..=32 * 1024 * 1024)
                .contains(&self.max_request_bytes)
            || self.max_request_bytes > self.max_inflight_bytes
            || self.max_submission_receipts == 0
            || self.max_submission_receipts >= isize::MAX as usize
            || self.max_submission_receipt_bytes == 0
            || self.max_submission_receipt_bytes >= isize::MAX as usize
            || self.max_pending_records == 0
            || self.max_pending_records >= isize::MAX as usize
            || self.max_pending_bytes == 0
            || self.max_pending_bytes >= isize::MAX as usize
            || self.max_disk_bytes == 0
            || self.max_disk_bytes > i64::MAX as u64
            || self.segment_bytes == 0
            || self.segment_bytes > self.max_disk_bytes
            || !(1..=65536).contains(&self.group_commit_max_records)
            || self.group_commit_interval_ms > 1000
            || !(1..=86_400_000).contains(&self.attempt_timeout_ms)
            || !(1..=86_400_000).contains(&self.retry_max_ms)
            || self.retry_min_ms == 0
            || self.retry_min_ms > self.retry_max_ms
            || !(1..=86_400_000).contains(&self.drain_timeout_ms)
        {
            return Err(
                "invalid cloud proxy identity, absolute directory or resource/deadline limits"
                    .into(),
            );
        }
        parrot_remote::durable::DurableEnvelope {
            source: "validation".into(),
            receiver: receiver.node_id.clone(),
            path: self.path.clone(),
            session: self.session.clone(),
            sequence: 1,
            type_key: "bin:validation#v1".into(),
            payload: Bytes::new(),
        }
        .validate()?;
        Ok(receiver)
    }
    pub fn outbox_config(&self) -> OutboxConfig {
        OutboxConfig {
            idempotent_submissions: self.idempotent_submissions,
            transactional: self.transactional,
            segment_bytes: self.segment_bytes,
            group_commit_max_records: self.group_commit_max_records,
            group_commit_interval: Duration::from_millis(self.group_commit_interval_ms),
            attempt_timeout: Duration::from_millis(self.attempt_timeout_ms),
            retry_min: Duration::from_millis(self.retry_min_ms),
            retry_max: Duration::from_millis(self.retry_max_ms),
            wal_limits: WalLimits {
                max_submission_receipts: self.max_submission_receipts,
                max_submission_receipt_bytes: self.max_submission_receipt_bytes,
                max_pending_records: self.max_pending_records,
                max_pending_bytes: self.max_pending_bytes,
                max_disk_bytes: self.max_disk_bytes,
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

/// Explicit opaque submission. Successful ASK returns CloudProxyAccepted after
/// WAL sync. Ordinary TELL still acknowledges only mailbox admission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxySubmit {
    pub type_key: String,
    pub payload: Bytes,
}
/// Stable caller-chosen ID scoped to this proxy's bound outbox. Same content
/// returns the original sequence across retry, ACK and restart; conflicting
/// content is rejected. Requires idempotent_submissions=true.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxySubmitIdempotent {
    pub request_id: String,
    pub type_key: String,
    pub payload: Bytes,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudProxyAccepted {
    pub sequence: u64,
}
/// Bounded submissions with independent per-item outcomes, in input order.
/// This is not an atomic business transaction. Retrying the whole batch can
/// duplicate items that already succeeded, even when the response was lost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxySubmitBatch {
    pub messages: Vec<CloudProxySubmit>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxyBatchResult {
    pub outcomes: Vec<Result<CloudProxyAccepted, String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxyInspect;
/// Local-open acceptance sync counters; excludes ACK/rotation/identity syncs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxyInspectStorage;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxyInspectAdmission;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxyAdmissionStats {
    pub used_bytes: u64,
    pub max_inflight_bytes: u64,
    pub max_request_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxyStorageStats {
    pub acceptance_syncs: u64,
    pub last_group_records: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxyStatus {
    pub pending: u64,
    pub last_enqueued: u64,
    pub last_acked: u64,
    pub attempts: u64,
    pub draining: bool,
    pub closed: bool,
    pub last_error: Option<String>,
    pub terminal_error: Option<String>,
}
/// Permanently close this actor's business admission, then wait for all accepted
/// messages. A timeout leaves the WAL intact and admission closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProxyDrain;

impl parrot_api::Message for CloudProxySubmit {
    type Result = CloudProxyAccepted;
}
impl parrot_api::Message for CloudProxySubmitIdempotent {
    type Result = CloudProxyAccepted;
}
impl parrot_api::Message for CloudProxySubmitBatch {
    type Result = CloudProxyBatchResult;
}
impl parrot_api::Message for CloudProxyInspect {
    type Result = CloudProxyStatus;
}
impl parrot_api::Message for CloudProxyInspectStorage {
    type Result = CloudProxyStorageStats;
}
impl parrot_api::Message for CloudProxyInspectAdmission {
    type Result = CloudProxyAdmissionStats;
}
impl parrot_api::Message for CloudProxyDrain {
    type Result = CloudProxyStatus;
}
macro_rules! codec {
    ($ty:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key, type_id: std::any::TypeId::of::<$ty>(),
                encode: |msg| parrot_api::message::serde_remote_serialize(msg.downcast_ref::<$ty>().ok_or("cloud proxy message type mismatch")?),
                decode: |bytes| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<$ty>(bytes)?) as parrot_api::types::BoxedMessage),
            }
        }
    };
}
codec!(CloudProxySubmit, "bin:parrot.app.cloud-proxy.submit#v1");
codec!(
    CloudProxySubmitIdempotent,
    "bin:parrot.app.cloud-proxy.submit-idempotent#v1"
);
codec!(
    CloudProxySubmitBatch,
    "bin:parrot.app.cloud-proxy.submit-batch#v1"
);
codec!(
    CloudProxyBatchResult,
    "bin:parrot.app.cloud-proxy.batch-result#v1"
);
codec!(CloudProxyAccepted, "bin:parrot.app.cloud-proxy.accepted#v1");
codec!(CloudProxyInspect, "bin:parrot.app.cloud-proxy.inspect#v1");
codec!(
    CloudProxyInspectAdmission,
    "bin:parrot.app.cloud-proxy.inspect-admission#v1"
);
codec!(
    CloudProxyAdmissionStats,
    "bin:parrot.app.cloud-proxy.admission-stats#v1"
);
codec!(
    CloudProxyInspectStorage,
    "bin:parrot.app.cloud-proxy.inspect-storage#v1"
);
codec!(
    CloudProxyStorageStats,
    "bin:parrot.app.cloud-proxy.storage-stats#v1"
);
codec!(CloudProxyStatus, "bin:parrot.app.cloud-proxy.status#v1");
codec!(CloudProxyDrain, "bin:parrot.app.cloud-proxy.drain#v1");
