//! Persistent request identities. The request envelope and its business command
//! occupy one Raft entry, so replay rebuilds both state and deduplication together.
use super::{command::MAX_COMMAND_BYTES, DirCmd, DirectoryApplyError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;

// 0xff cannot start any existing DirCmd discriminant. Existing plain entries
// remain readable; old binaries fail closed on this versioned extension.
const MAGIC: &[u8] = b"\xffDIR\x01";
pub(super) const READ_BARRIER: &[u8] = b"\xffDIR\x02";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectoryRequestStatus {
    /// No matching request in this replica's local log. This is not a quorum read.
    Unknown,
    Pending {
        index: u64,
        term: u64,
    },
    /// The original business command has been committed and applied exactly once.
    Applied {
        index: u64,
        term: u64,
        revision: u64,
    },
    /// The same authenticated client and ID already identify different content.
    Conflict,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct RequestCommand {
    pub client: String,
    pub id: String,
    pub command: DirCmd,
}
pub(super) enum StoredCommand {
    ReadBarrier,
    Plain(DirCmd),
    Request(RequestCommand),
}
pub(super) fn identity(client: &str, id: &str) -> Result<(), DirectoryApplyError> {
    parrot_config::endpoint::validate_node_id(client).map_err(DirectoryApplyError::Invalid)?;
    if client.len() > 1024 || id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        return Err(DirectoryApplyError::Invalid(
            "invalid request identity".into(),
        ));
    }
    Ok(())
}
/// Canonical command digest used by status queries and content-conflict checks.
pub fn directory_command_digest(command: &DirCmd) -> Result<[u8; 32], DirectoryApplyError> {
    Ok(Sha256::digest(command.encode()?).into())
}
pub(super) fn encode_request(
    client: &str,
    id: &str,
    command: &DirCmd,
) -> Result<Vec<u8>, DirectoryApplyError> {
    identity(client, id)?;
    command.encode()?;
    let request = RequestCommand {
        client: client.into(),
        id: id.into(),
        command: command.clone(),
    };
    let mut counter = Limit(MAGIC.len());
    bincode::serde::encode_into_std_write(&request, &mut counter, bincode::config::standard())
        .map_err(|_| DirectoryApplyError::Capacity)?;
    let mut bytes = MAGIC.to_vec();
    bincode::serde::encode_into_std_write(&request, &mut bytes, bincode::config::standard())
        .map_err(|e| DirectoryApplyError::Invalid(e.to_string()))?;
    Ok(bytes)
}
pub(super) fn decode(bytes: &[u8]) -> Result<StoredCommand, DirectoryApplyError> {
    if bytes == READ_BARRIER {
        return Ok(StoredCommand::ReadBarrier);
    }
    if bytes.len() > MAX_COMMAND_BYTES {
        return Err(DirectoryApplyError::Capacity);
    }
    if let Some(bytes) = bytes.strip_prefix(MAGIC) {
        let request: RequestCommand = parrot_api::message::serde_remote_deserialize_with_limits(
            bytes,
            parrot_api::message::RemoteDecodeLimits {
                max_payload_bytes: MAX_COMMAND_BYTES,
                max_values: 4200,
                max_depth: 12,
            },
        )
        .map_err(DirectoryApplyError::Invalid)?;
        identity(&request.client, &request.id)?;
        request.command.encode()?;
        Ok(StoredCommand::Request(request))
    } else {
        DirCmd::decode(bytes).map(StoredCommand::Plain)
    }
}
struct Limit(usize);
impl Write for Limit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_COMMAND_BYTES - self.0 {
            return Err(std::io::Error::other("request capacity"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
