//! Direct authenticated Directory writes and persistent request status. Transport
//! failures/timeouts may follow a commit; retry the same ID and identical command.
use super::{
    directory_command_digest,
    network::{DirectoryHandle, NetworkDirectoryError},
    request, DirCmd, DirectoryRequestStatus,
};
use crate::{Frame, NodeAddr, PeerAuthentication, RemoteActorSystem, ReplyPayload};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

fn request_deadline(timeout: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .filter(|_| !timeout.is_zero())
        .ok_or_else(|| NetworkDirectoryError::Invalid("invalid Directory deadline".into()))
}

fn check_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(NetworkDirectoryError::Timeout)
    } else {
        Ok(())
    }
}

pub const CLIENT_KEY: &str = "parrot:directory:client#v1";
pub const CLIENT_REPLY_KEY: &str = "parrot:directory:client-result#v1";
pub const CLIENT_PATH: &str = "/_directory/client/v1";
pub const READ_KEY: &str = "parrot:directory:read#v1";
pub const READ_REPLY_KEY: &str = "parrot:directory:read-result#v1";
pub const READ_PATH: &str = "/_directory/read/v1";
#[derive(Serialize, Deserialize)]
struct WireRead {
    group: String,
    query: super::DirQuery,
    timeout_ms: u64,
}
pub(crate) async fn receive_read(
    handle: &DirectoryHandle,
    bytes: &[u8],
) -> Result<super::DirectoryRead> {
    let request: WireRead = parrot_api::message::serde_remote_deserialize_with_limits(
        bytes,
        parrot_api::message::RemoteDecodeLimits {
            max_payload_bytes: 8192,
            max_values: 32,
            max_depth: 8,
        },
    )
    .map_err(NetworkDirectoryError::Invalid)?;
    if request.group != handle.group() {
        return Err(NetworkDirectoryError::Forbidden);
    }
    handle
        .query_linearizable(&request.query, Duration::from_millis(request.timeout_ms))
        .await
}
const MAX_WIRE: usize = 1024 * 1024 + 2048;
type Result<T> = std::result::Result<T, NetworkDirectoryError>;
#[derive(Serialize, Deserialize)]
pub(super) struct WireRequest {
    pub group: String,
    pub operation: Operation,
}
#[derive(Serialize, Deserialize)]
pub(super) enum Operation {
    Submit { id: String, command: DirCmd },
    Status { id: String, digest: [u8; 32] },
}
pub(super) fn decode(bytes: &[u8]) -> Result<WireRequest> {
    parrot_api::message::serde_remote_deserialize_with_limits(
        bytes,
        parrot_api::message::RemoteDecodeLimits {
            max_payload_bytes: MAX_WIRE,
            max_values: 4300,
            max_depth: 12,
        },
    )
    .map_err(NetworkDirectoryError::Invalid)
}
pub(crate) async fn receive(
    handle: &DirectoryHandle,
    peer: &str,
    payload: Bytes,
) -> Result<DirectoryRequestStatus> {
    if payload.len() > MAX_WIRE {
        return Err(NetworkDirectoryError::Capacity);
    }
    handle.client_request(peer, payload).await
}

/// Uses the remote system's authenticated identity as the durable namespace.
/// The caller selects a direct group member; no implicit relay or leader redirect.
#[derive(Clone)]
pub struct DirectoryClient {
    remote: Arc<RemoteActorSystem>,
    group: String,
}
impl DirectoryClient {
    /// Quorum-confirmed read, including absence. Each call appends a fresh
    /// barrier; local QueryLocal and request status retain their local semantics.
    pub async fn query_linearizable(
        &self,
        peer: &NodeAddr,
        query: &super::DirQuery,
        timeout: Duration,
    ) -> Result<super::DirectoryRead> {
        let deadline = request_deadline(timeout)?;
        super::validate_query(query).map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
        if timeout < Duration::from_millis(1) || timeout > Duration::from_secs(60) {
            return Err(NetworkDirectoryError::Invalid(
                "read timeout must be 1..60000ms".into(),
            ));
        }
        let payload = bincode::serde::encode_to_vec(
            WireRead {
                group: self.group.clone(),
                query: query.clone(),
                timeout_ms: timeout.as_millis() as u64,
            },
            bincode::config::standard(),
        )
        .map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
        self.call_wire(peer, payload, READ_PATH, READ_KEY, READ_REPLY_KEY, deadline)
            .await
    }
    pub fn new(remote: Arc<RemoteActorSystem>, group: impl Into<String>) -> Result<Self> {
        let group = group.into();
        if group.is_empty() || group.len() > 1011 || group.chars().any(char::is_control) {
            return Err(NetworkDirectoryError::Invalid(
                "invalid group identifier".into(),
            ));
        }
        Ok(Self { remote, group })
    }
    pub async fn submit(
        &self,
        peer: &NodeAddr,
        id: &str,
        command: &DirCmd,
        timeout: Duration,
    ) -> Result<DirectoryRequestStatus> {
        let deadline = request_deadline(timeout)?;
        self.submit_before(peer, id, command, deadline).await
    }
    async fn submit_before(
        &self,
        peer: &NodeAddr,
        id: &str,
        command: &DirCmd,
        deadline: Instant,
    ) -> Result<DirectoryRequestStatus> {
        check_deadline(deadline)?;
        request::encode_request(&self.remote.config.node_id, id, command)
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
    ) -> Result<DirectoryRequestStatus> {
        let deadline = request_deadline(timeout)?;
        self.status_before(peer, id, digest, deadline).await
    }
    async fn status_before(
        &self,
        peer: &NodeAddr,
        id: &str,
        digest: [u8; 32],
        deadline: Instant,
    ) -> Result<DirectoryRequestStatus> {
        check_deadline(deadline)?;
        request::identity(&self.remote.config.node_id, id)
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
    /// One total deadline covers encoding, connection, admission and polling. A timeout
    /// never means the write was rolled back. Conflict is a terminal result.
    pub async fn submit_and_wait(
        &self,
        peer: &NodeAddr,
        id: &str,
        command: &DirCmd,
        timeout: Duration,
    ) -> Result<DirectoryRequestStatus> {
        let deadline = request_deadline(timeout)?;
        let digest = directory_command_digest(command)
            .map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
        check_deadline(deadline)?;
        tokio::time::timeout_at(deadline, async {
            let mut status = self.submit_before(peer, id, command, deadline).await?;
            loop {
                check_deadline(deadline)?;
                if matches!(
                    status,
                    DirectoryRequestStatus::Applied { .. } | DirectoryRequestStatus::Conflict
                ) {
                    return Ok(status);
                }
                tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_millis(20)))
                    .await;
                status = self.status_before(peer, id, digest, deadline).await?;
            }
        })
        .await
        .map_err(|_| NetworkDirectoryError::Timeout)?
    }
    async fn call(
        &self,
        peer: &NodeAddr,
        operation: Operation,
        deadline: Instant,
    ) -> Result<DirectoryRequestStatus> {
        check_deadline(deadline)?;
        let payload = bincode::serde::encode_to_vec(
            WireRequest {
                group: self.group.clone(),
                operation,
            },
            bincode::config::standard(),
        )
        .map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
        if payload.len() > MAX_WIRE {
            return Err(NetworkDirectoryError::Capacity);
        }
        self.call_wire(
            peer,
            payload,
            CLIENT_PATH,
            CLIENT_KEY,
            CLIENT_REPLY_KEY,
            deadline,
        )
        .await
    }
    async fn call_wire<T: serde::de::DeserializeOwned>(
        &self,
        peer: &NodeAddr,
        payload: Vec<u8>,
        path: &str,
        key: &str,
        reply_key: &str,
        deadline: Instant,
    ) -> Result<T> {
        if !peer.identity_is_explicit || peer.scheme != self.remote.config.scheme {
            return Err(NetworkDirectoryError::Invalid(
                "explicit matching peer transport required".into(),
            ));
        }
        check_deadline(deadline)?;
        tokio::time::timeout_at(deadline, async {
            if self.remote.sender_of(&peer.node_id).is_none() {
                self.remote
                    .connect(peer)
                    .await
                    .map_err(|e| NetworkDirectoryError::Operation(e.to_string()))?;
            }
            check_deadline(deadline)?;
            let sender = self
                .remote
                .sender_of(&peer.node_id)
                .ok_or(NetworkDirectoryError::Closed)?;
            if sender.node_id() != peer.node_id
                || !matches!(
                    sender.authentication(),
                    PeerAuthentication::MutualTls | PeerAuthentication::InProcess
                )
            {
                return Err(NetworkDirectoryError::Forbidden);
            }
            check_deadline(deadline)?;
            let cid = self.remote.callbacks.next_cid();
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.remote
                .callbacks
                .insert_on(cid, peer.node_id.clone(), tx, &sender)
                .map_err(|e| NetworkDirectoryError::Operation(e.to_string()))?;
            let _pending = self.remote.callbacks.guard(cid);
            let frame = Frame::ask(
                cid,
                &format!("parrot://{}{path}", peer.node_id),
                key,
                Bytes::from(payload),
                None,
            );
            // A ready queue must not win against an already elapsed deadline.
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => return Err(NetworkDirectoryError::Timeout),
                result = sender.send(frame) => result.map_err(|e| NetworkDirectoryError::Operation(e.to_string()))?,
            }
            let reply = tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => return Err(NetworkDirectoryError::Timeout),
                reply = rx => reply.map_err(|_| NetworkDirectoryError::Closed)?,
                _ = sender.closed() => return Err(NetworkDirectoryError::Closed),
            };
            check_deadline(deadline)?;
            let result = match reply {
                ReplyPayload::Ok(bytes, key) if key == reply_key => {
                    parrot_api::message::serde_remote_deserialize_with_limits::<Result<T>>(
                        &bytes,
                        parrot_api::message::RemoteDecodeLimits {
                            max_payload_bytes: 2 * 1024 * 1024,
                            max_values: 4300,
                            max_depth: 8,
                        },
                    )
                    .map_err(NetworkDirectoryError::Invalid)
                    .and_then(std::convert::identity)
                }
                ReplyPayload::Err(crate::ErrCode::Forbidden, _) => {
                    Err(NetworkDirectoryError::Forbidden)
                }
                ReplyPayload::Err(code, _) => Err(NetworkDirectoryError::Operation(format!(
                    "Directory rejected request: {code:?}"
                ))),
                ReplyPayload::Ok(_, _) => Err(NetworkDirectoryError::Invalid(
                    "unexpected Directory reply key".into(),
                )),
            };
            // Decoding is synchronous and cannot be preempted by timeout_at.
            check_deadline(deadline)?;
            result
        })
        .await
        .map_err(|_| NetworkDirectoryError::Timeout)?
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Empty;
    #[async_trait::async_trait]
    impl crate::LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
            None
        }
    }
    static DECODED: AtomicBool = AtomicBool::new(false);
    #[derive(Debug)]
    struct SlowStatus;
    impl<'de> Deserialize<'de> for SlowStatus {
        fn deserialize<D: serde::Deserializer<'de>>(
            decoder: D,
        ) -> std::result::Result<Self, D::Error> {
            let _ = DirectoryRequestStatus::deserialize(decoder)?;
            DECODED.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(700));
            Ok(Self)
        }
    }

    #[tokio::test]
    async fn synchronous_reply_decode_cannot_return_success_after_deadline() {
        let path =
            std::env::temp_dir().join(format!("parrot-client-decode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let mut host = super::super::network::NetworkDirectory::start(
            &path,
            "decode",
            crate::RemoteConfig::mem("decode-owner"),
            vec![],
            Arc::new(Empty),
        )
        .await
        .unwrap();
        let remote =
            RemoteActorSystem::new(crate::RemoteConfig::mem("decode-reader"), Arc::new(Empty))
                .unwrap();
        remote.start().await.unwrap();
        let peer = NodeAddr::mem("decode-owner");
        remote.connect(&peer).await.unwrap();
        let client = DirectoryClient::new(remote.clone(), "decode").unwrap();
        let payload = bincode::serde::encode_to_vec(
            WireRequest {
                group: "decode".into(),
                operation: Operation::Status {
                    id: "unknown".into(),
                    digest: [0; 32],
                },
            },
            bincode::config::standard(),
        )
        .unwrap();
        let timeout = Duration::from_millis(500);
        DECODED.store(false, Ordering::SeqCst);
        let outcome: Result<SlowStatus> = client
            .call_wire(
                &peer,
                payload,
                CLIENT_PATH,
                CLIENT_KEY,
                CLIENT_REPLY_KEY,
                request_deadline(timeout).unwrap(),
            )
            .await;
        assert!(
            DECODED.load(Ordering::SeqCst),
            "the test must exercise synchronous decoding"
        );
        assert!(
            matches!(outcome, Err(NetworkDirectoryError::Timeout)),
            "{outcome:?}"
        );
        assert_eq!(remote.callbacks.len(), 0);
        host.close().await.unwrap();
        remote.shutdown().await.unwrap();
        std::fs::remove_dir_all(path).unwrap();
    }
}
