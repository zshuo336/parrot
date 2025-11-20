//! Committed ownership and a fenced state register sharing one Raft apply boundary.
//! The register fences its own writes, not arbitrary actor or external effects.
use super::{client::DirectoryClient, network::NetworkDirectoryError, DirectoryApplyError};
use crate::{NodeAddr, RemoteActorSystem};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::time::Instant;

pub const KEY: &str = "parrot:directory:ownership#v1";
pub const REPLY_KEY: &str = "parrot:directory:ownership-result#v1";
pub const PATH: &str = "/_directory/ownership/v1";
pub(super) const MAGIC: &[u8] = b"\xffOWN\x01";
const MAX_WIRE: usize = 12 * 1024;
const MAX_RESOURCES: usize = 4096;
pub(super) mod checkpoint;
mod cluster;
pub use cluster::OwnershipClusterClient;
/// Maximum externalized state per resource. Empty state still has a revision.
pub const MAX_VALUE_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipCommand {
    /// Assign to the authenticated submitting client. None means never allocated.
    /// A nonempty expected generation explicitly allows replacement of its holder.
    Claim {
        resource: String,
        expected: Option<u64>,
    },
    /// Only the current holder may release its exact generation.
    Release { resource: String, expected: u64 },
    /// One committed observation. Retrying its ID returns that original view.
    Observe { resource: String },
    /// Atomically check the live holder, generation and state revision, then
    /// replace this resource's state. None means the state was never written.
    WriteValue {
        resource: String,
        generation: u64,
        expected_revision: Option<u64>,
        value: Vec<u8>,
    },
    /// Committed ownership and state at one log position; retries are historical.
    ReadValue { resource: String },
}
impl OwnershipCommand {
    fn resource(&self) -> &str {
        match self {
            Self::Claim { resource, .. }
            | Self::Release { resource, .. }
            | Self::Observe { resource }
            | Self::WriteValue { resource, .. }
            | Self::ReadValue { resource } => resource,
        }
    }
    fn validate(&self) -> Result<(), DirectoryApplyError> {
        let resource = self.resource();
        if resource.is_empty()
            || resource.len() > 4096
            || resource.chars().any(char::is_control)
            || matches!(
                self,
                Self::Claim {
                    expected: Some(0),
                    ..
                } | Self::Release { expected: 0, .. }
                    | Self::WriteValue { generation: 0, .. }
                    | Self::WriteValue {
                        expected_revision: Some(0),
                        ..
                    }
            )
            || matches!(self, Self::WriteValue { value, .. } if value.len() > MAX_VALUE_BYTES)
        {
            return Err(DirectoryApplyError::Invalid(
                "invalid ownership resource or generation".into(),
            ));
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<[u8; 32], DirectoryApplyError> {
        self.validate()?;
        Ok(Sha256::digest(
            bincode::serde::encode_to_vec(self, bincode::config::standard())
                .map_err(|e| DirectoryApplyError::Invalid(e.to_string()))?,
        )
        .into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipRecord {
    pub resource: String,
    /// Original committed change's log index, scoped to this persistent group.
    pub generation: u64,
    /// None is a retained released record, not the original absent state.
    pub holder: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipValue {
    /// Original committed write's log index; independent of ownership generation.
    pub revision: u64,
    pub value: Bytes,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipRejection {
    GenerationConflict,
    NotHolder,
    Capacity,
    ValueRevisionConflict,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipOutcome {
    Changed(OwnershipRecord),
    Observed(Option<OwnershipRecord>),
    Rejected {
        reason: OwnershipRejection,
        current: Option<OwnershipRecord>,
    },
    ValueChanged {
        ownership: OwnershipRecord,
        value: OwnershipValue,
    },
    ValueObserved {
        ownership: Option<OwnershipRecord>,
        value: Option<OwnershipValue>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipStatus {
    Unknown,
    Pending {
        index: u64,
        term: u64,
    },
    /// Includes committed rejections. Inspect outcome before treating it as ownership.
    Applied {
        index: u64,
        term: u64,
        outcome: OwnershipOutcome,
    },
    /// The client/id belongs to different command bytes.
    Conflict,
}
impl OwnershipStatus {
    pub(super) fn terminal(&self) -> bool {
        matches!(self, Self::Applied { .. } | Self::Conflict)
    }
}
#[derive(Serialize, Deserialize)]
pub(super) struct Request {
    pub client: String,
    pub id: String,
    pub command: OwnershipCommand,
    #[serde(skip)]
    pub digest: [u8; 32],
}
pub(super) fn encode(
    client: &str,
    id: &str,
    command: &OwnershipCommand,
) -> Result<Vec<u8>, DirectoryApplyError> {
    super::request::identity(client, id)?;
    let digest = command.digest()?;
    let request = Request {
        client: client.into(),
        id: id.into(),
        command: command.clone(),
        digest,
    };
    let mut bytes = MAGIC.to_vec();
    bincode::serde::encode_into_std_write(request, &mut bytes, bincode::config::standard())
        .map_err(|e| DirectoryApplyError::Invalid(e.to_string()))?;
    if bytes.len() > MAX_WIRE {
        return Err(DirectoryApplyError::Capacity);
    }
    Ok(bytes)
}
pub(super) fn decode(bytes: &[u8]) -> Result<Request, DirectoryApplyError> {
    if bytes.len() > MAX_WIRE {
        return Err(DirectoryApplyError::Capacity);
    }
    let payload = bytes
        .strip_prefix(MAGIC)
        .ok_or_else(|| DirectoryApplyError::Invalid("ownership schema".into()))?;
    let mut request: Request =
        parrot_api::message::serde_remote_deserialize_with_limits(payload, limits())
            .map_err(DirectoryApplyError::Invalid)?;
    super::request::identity(&request.client, &request.id)?;
    request.digest = request.command.digest()?;
    Ok(request)
}
fn limits() -> parrot_api::message::RemoteDecodeLimits {
    parrot_api::message::RemoteDecodeLimits {
        max_payload_bytes: MAX_WIRE,
        max_values: MAX_VALUE_BYTES + 64,
        max_depth: 8,
    }
}

#[derive(Default)]
pub(super) struct State {
    records: HashMap<String, OwnershipRecord>,
    requests: HashMap<(String, String), ([u8; 32], OwnershipStatus)>,
    values: HashMap<String, OwnershipValue>,
}
impl State {
    pub fn status(&self, client: &str, id: &str, digest: [u8; 32]) -> Option<OwnershipStatus> {
        self.requests
            .get(&(client.into(), id.into()))
            .map(|(saved, status)| {
                if *saved == digest {
                    status.clone()
                } else {
                    OwnershipStatus::Conflict
                }
            })
    }
    pub fn apply(&mut self, request: Request, index: u64, term: u64) {
        let key = (request.client.clone(), request.id);
        if self.requests.contains_key(&key) {
            return;
        }
        let resource = request.command.resource().to_owned();
        let current = self.records.get(&resource).cloned();
        let observed = current.as_ref().map(|record| record.generation);
        let outcome = match request.command {
            OwnershipCommand::Observe { .. } => OwnershipOutcome::Observed(current),
            OwnershipCommand::ReadValue { .. } => OwnershipOutcome::ValueObserved {
                ownership: current,
                value: self.values.get(&resource).cloned(),
            },
            OwnershipCommand::WriteValue {
                generation,
                expected_revision,
                value,
                ..
            } => {
                // Validate at application, not during admission: ownership can
                // change while this command waits for its quorum commit.
                let rejection = if Some(generation) != observed {
                    Some(OwnershipRejection::GenerationConflict)
                } else if current.as_ref().and_then(|record| record.holder.as_deref())
                    != Some(&request.client)
                {
                    Some(OwnershipRejection::NotHolder)
                } else if self.values.get(&resource).map(|state| state.revision)
                    != expected_revision
                {
                    Some(OwnershipRejection::ValueRevisionConflict)
                } else {
                    None
                };
                if let Some(reason) = rejection {
                    OwnershipOutcome::Rejected { reason, current }
                } else {
                    let state = OwnershipValue {
                        revision: index,
                        value: Bytes::from(value),
                    };
                    self.values.insert(resource, state.clone());
                    OwnershipOutcome::ValueChanged {
                        ownership: current.expect("validated live ownership"),
                        value: state,
                    }
                }
            }
            command => {
                let rejection = match command {
                    OwnershipCommand::Claim { expected, .. } if expected != observed => {
                        Some(OwnershipRejection::GenerationConflict)
                    }
                    OwnershipCommand::Release { expected, .. } if Some(expected) != observed => {
                        Some(OwnershipRejection::GenerationConflict)
                    }
                    OwnershipCommand::Release { .. }
                        if current.as_ref().and_then(|r| r.holder.as_deref())
                            != Some(&request.client) =>
                    {
                        Some(OwnershipRejection::NotHolder)
                    }
                    _ if current.is_none() && self.records.len() >= MAX_RESOURCES => {
                        Some(OwnershipRejection::Capacity)
                    }
                    _ => None,
                };
                if let Some(reason) = rejection {
                    OwnershipOutcome::Rejected { reason, current }
                } else {
                    let record = OwnershipRecord {
                        resource: resource.clone(),
                        generation: index,
                        holder: matches!(command, OwnershipCommand::Claim { .. })
                            .then_some(request.client),
                    };
                    self.records.insert(resource, record.clone());
                    OwnershipOutcome::Changed(record)
                }
            }
        };
        self.requests.insert(
            key,
            (
                request.digest,
                OwnershipStatus::Applied {
                    index,
                    term,
                    outcome,
                },
            ),
        );
    }
}

#[derive(Serialize, Deserialize)]
pub(super) enum Operation {
    Submit {
        id: String,
        command: OwnershipCommand,
    },
    Status {
        id: String,
        digest: [u8; 32],
    },
}
#[derive(Serialize, Deserialize)]
struct WireRequest {
    group: String,
    operation: Operation,
}
pub(super) fn wire_decode(bytes: &[u8], group: &str) -> Result<Operation, NetworkDirectoryError> {
    let request: WireRequest =
        parrot_api::message::serde_remote_deserialize_with_limits(bytes, limits())
            .map_err(NetworkDirectoryError::Invalid)?;
    if request.group != group {
        return Err(NetworkDirectoryError::Forbidden);
    }
    Ok(request.operation)
}

/// Direct authenticated control client. No leader redirect or blind mutation retry.
#[derive(Clone)]
pub struct OwnershipClient {
    directory: DirectoryClient,
    group: String,
    client: String,
}
impl OwnershipClient {
    pub fn new(
        remote: Arc<RemoteActorSystem>,
        group: impl Into<String>,
    ) -> Result<Self, NetworkDirectoryError> {
        let group = group.into();
        Ok(Self {
            client: remote.config.node_id.clone(),
            directory: DirectoryClient::new(remote, group.clone())?,
            group,
        })
    }
    pub async fn submit(
        &self,
        peer: &NodeAddr,
        id: &str,
        command: &OwnershipCommand,
        timeout: Duration,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        let deadline = deadline(timeout)?;
        self.submit_before(peer, id, command, deadline).await
    }
    async fn submit_before(
        &self,
        peer: &NodeAddr,
        id: &str,
        command: &OwnershipCommand,
        deadline: Instant,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        encode(&self.client, id, command)
            .map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
        self.call(
            peer,
            Operation::Submit {
                id: id.into(),
                command: command.clone(),
            },
            deadline,
        )
        .await
    }
    pub async fn status(
        &self,
        peer: &NodeAddr,
        id: &str,
        digest: [u8; 32],
        timeout: Duration,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        let deadline = deadline(timeout)?;
        self.status_before(peer, id, digest, deadline).await
    }
    async fn status_before(
        &self,
        peer: &NodeAddr,
        id: &str,
        digest: [u8; 32],
        deadline: Instant,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        super::request::identity(&self.client, id)
            .map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
        self.call(
            peer,
            Operation::Status {
                id: id.into(),
                digest,
            },
            deadline,
        )
        .await
    }
    pub async fn submit_and_wait(
        &self,
        peer: &NodeAddr,
        id: &str,
        command: &OwnershipCommand,
        timeout: Duration,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        let deadline = deadline(timeout)?;
        let digest = command
            .digest()
            .map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
        tokio::time::timeout_at(deadline, async {
            let mut status = self.submit_before(peer, id, command, deadline).await?;
            while !status.terminal() {
                tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_millis(20)))
                    .await;
                status = self.status_before(peer, id, digest, deadline).await?;
            }
            Ok(status)
        })
        .await
        .map_err(|_| NetworkDirectoryError::Timeout)?
    }
    /// Fresh invocation identity; cannot return a previous read's cached result.
    pub async fn observe(
        &self,
        peer: &NodeAddr,
        resource: &str,
        timeout: Duration,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        self.submit_and_wait(
            peer,
            &uuid::Uuid::new_v4().to_string(),
            &OwnershipCommand::Observe {
                resource: resource.into(),
            },
            timeout,
        )
        .await
    }
    /// Fresh, coherent committed ownership/state. An old explicit ReadValue ID
    /// intentionally returns its original view, even after a later write.
    pub async fn read_value(
        &self,
        peer: &NodeAddr,
        resource: &str,
        timeout: Duration,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        self.submit_and_wait(
            peer,
            &uuid::Uuid::new_v4().to_string(),
            &OwnershipCommand::ReadValue {
                resource: resource.into(),
            },
            timeout,
        )
        .await
    }
    async fn call(
        &self,
        peer: &NodeAddr,
        operation: Operation,
        deadline: Instant,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        if Instant::now() >= deadline {
            return Err(NetworkDirectoryError::Timeout);
        }
        let bytes = bincode::serde::encode_to_vec(
            WireRequest {
                group: self.group.clone(),
                operation,
            },
            bincode::config::standard(),
        )
        .map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
        if bytes.len() > MAX_WIRE {
            return Err(NetworkDirectoryError::Capacity);
        }
        self.directory
            .call_wire(peer, bytes, PATH, KEY, REPLY_KEY, deadline)
            .await
    }
}
fn deadline(timeout: Duration) -> Result<Instant, NetworkDirectoryError> {
    Instant::now()
        .checked_add(timeout)
        .filter(|_| !timeout.is_zero() && timeout <= Duration::from_secs(60))
        .ok_or_else(|| {
            NetworkDirectoryError::Invalid("ownership timeout must be in (0,60s]".into())
        })
}
pub(crate) async fn receive(
    handle: &super::network::DirectoryHandle,
    client: &str,
    payload: Bytes,
) -> Result<OwnershipStatus, NetworkDirectoryError> {
    if payload.len() > MAX_WIRE {
        return Err(NetworkDirectoryError::Capacity);
    }
    handle.ownership_request(client, payload).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_retains_tombstones_and_allows_existing_resource_changes() {
        let mut state = State::default();
        for index in 1..=MAX_RESOURCES as u64 {
            let resource = format!("resource-{index}");
            let command = OwnershipCommand::Claim {
                resource: resource.clone(),
                expected: None,
            };
            state.apply(
                decode(&encode("alice", &format!("claim-{index}"), &command).unwrap()).unwrap(),
                index * 2,
                1,
            );
            let command = OwnershipCommand::Release {
                resource,
                expected: index * 2,
            };
            state.apply(
                decode(&encode("alice", &format!("release-{index}"), &command).unwrap()).unwrap(),
                index * 2 + 1,
                1,
            );
        }
        let full = OwnershipCommand::Claim {
            resource: "new-resource".into(),
            expected: None,
        };
        state.apply(
            decode(&encode("alice", "full", &full).unwrap()).unwrap(),
            9000,
            1,
        );
        assert!(matches!(
            state.status("alice", "full", full.digest().unwrap()),
            Some(OwnershipStatus::Applied {
                outcome: OwnershipOutcome::Rejected {
                    reason: OwnershipRejection::Capacity,
                    current: None
                },
                ..
            })
        ));
        let aba = OwnershipCommand::Claim {
            resource: "resource-1".into(),
            expected: None,
        };
        state.apply(
            decode(&encode("alice", "aba", &aba).unwrap()).unwrap(),
            9001,
            1,
        );
        assert!(
            matches!(state.status("alice", "aba", aba.digest().unwrap()), Some(OwnershipStatus::Applied { outcome: OwnershipOutcome::Rejected { reason: OwnershipRejection::GenerationConflict, current: Some(record) }, .. }) if record.generation == 3 && record.holder.is_none())
        );
        let reclaim = OwnershipCommand::Claim {
            resource: "resource-1".into(),
            expected: Some(3),
        };
        state.apply(
            decode(&encode("bob", "reclaim", &reclaim).unwrap()).unwrap(),
            9002,
            1,
        );
        assert!(
            matches!(state.status("bob", "reclaim", reclaim.digest().unwrap()), Some(OwnershipStatus::Applied { outcome: OwnershipOutcome::Changed(record), .. }) if record.generation == 9002 && record.holder.as_deref() == Some("bob"))
        );
        assert_eq!(state.records.len(), MAX_RESOURCES);
    }
}
