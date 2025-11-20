//! Retry one immutable invocation through a bounded, explicitly trusted route set.
use super::{
    deadline, encode, NetworkDirectoryError, OwnershipClient, OwnershipCommand, OwnershipStatus,
};
use crate::{NodeAddr, RemoteActorSystem};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::Instant;

/// Fixed authenticated Directory routes, with bounded failover and no mutation
/// of request identity. This is not a membership or quorum implementation.
///
/// Successful historical retries retain their original outcome; use a fresh
/// observation to learn current ownership. Cancellation/timeout does not undo
/// committed work. All routes must belong to the same persistent group.
#[derive(Clone)]
pub struct OwnershipClusterClient {
    direct: OwnershipClient,
    peers: Arc<[NodeAddr]>,
    preferred: Arc<AtomicUsize>,
    attempt_timeout: Duration,
    shutdown: tokio::sync::watch::Receiver<bool>,
}

impl OwnershipClusterClient {
    /// One to 64 explicit routes, including alternate addresses for a member.
    /// Duplicate routes, invalid identities and mixed transports are rejected.
    /// Routes and authentication remain operator controlled; hints never add
    /// destinations. This does not change the group's voting configuration.
    pub fn new(
        remote: Arc<RemoteActorSystem>,
        group: impl Into<String>,
        peers: Vec<NodeAddr>,
    ) -> Result<Self, NetworkDirectoryError> {
        let mut distinct = HashSet::new();
        if peers.is_empty()
            || peers.len() > 64
            || peers.iter().any(|peer| {
                !peer.identity_is_explicit
                    || peer.scheme != remote.config.scheme
                    || parrot_config::endpoint::validate_node_id(&peer.node_id).is_err()
                    || !distinct.insert(peer.clone())
            })
        {
            return Err(NetworkDirectoryError::Invalid(
                "invalid ownership routes".into(),
            ));
        }
        Ok(Self {
            shutdown: remote.shutdown_signal(),
            direct: OwnershipClient::new(remote, group)?,
            peers: peers.into(),
            preferred: Arc::new(AtomicUsize::new(0)),
            attempt_timeout: Duration::from_millis(500),
        })
    }

    /// Set the per-route budget for the deployment's expected latency. The
    /// default is 500ms; (0,60s] is accepted and the total deadline still wins.
    pub fn with_attempt_timeout(
        mut self,
        timeout: Duration,
    ) -> Result<Self, NetworkDirectoryError> {
        if timeout.is_zero() || timeout > Duration::from_secs(60) {
            return Err(NetworkDirectoryError::Invalid(
                "invalid ownership attempt timeout".into(),
            ));
        }
        self.attempt_timeout = timeout;
        Ok(self)
    }

    /// One total deadline includes every connection, submission and backoff.
    /// A stalled member gets the configured per-attempt budget (default 500ms).
    /// Pending and transient failures rotate through the fixed
    /// routes, always submitting exactly the original client/ID/command bytes.
    /// Invalid, forbidden and capacity errors return immediately.
    pub async fn submit_and_wait(
        &self,
        id: &str,
        command: &OwnershipCommand,
        timeout: Duration,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        let end = deadline(timeout)?;
        encode(&self.direct.client, id, command)
            .map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
        let mut index = self.preferred.load(Ordering::Relaxed);
        let retry = tokio::time::timeout_at(end, async {
            loop {
                let now = Instant::now();
                if now >= end {
                    return Err(NetworkDirectoryError::Timeout);
                }
                let attempt = end.min(now + self.attempt_timeout);
                match self
                    .direct
                    .submit_before(&self.peers[index], id, command, attempt)
                    .await
                {
                    Ok(status) if status.terminal() => {
                        self.preferred.store(index, Ordering::Relaxed);
                        return Ok(status);
                    }
                    Err(
                        error @ (NetworkDirectoryError::Invalid(_)
                        | NetworkDirectoryError::Forbidden
                        | NetworkDirectoryError::Capacity),
                    ) => return Err(error),
                    Ok(_) | Err(_) => {}
                }
                index = (index + 1) % self.peers.len();
                tokio::time::sleep_until(end.min(Instant::now() + Duration::from_millis(20))).await;
            }
        });
        let mut shutdown = self.shutdown.clone();
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|closed| *closed) => Err(NetworkDirectoryError::Closed),
            result = retry => result.map_err(|_| NetworkDirectoryError::Timeout)?,
        }
    }

    /// A fresh quorum-committed observation, not a cached previous invocation.
    pub async fn observe(
        &self,
        resource: &str,
        timeout: Duration,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        self.submit_and_wait(
            &uuid::Uuid::new_v4().to_string(),
            &OwnershipCommand::Observe {
                resource: resource.into(),
            },
            timeout,
        )
        .await
    }

    /// Fresh coherent ownership and state; retries within this invocation retain
    /// its original ID while subsequent calls allocate distinct IDs.
    pub async fn read_value(
        &self,
        resource: &str,
        timeout: Duration,
    ) -> Result<OwnershipStatus, NetworkDirectoryError> {
        self.submit_and_wait(
            &uuid::Uuid::new_v4().to_string(),
            &OwnershipCommand::ReadValue {
                resource: resource.into(),
            },
            timeout,
        )
        .await
    }
}
