use super::{DirCmd, DirectorySm};
use crate::raft::LogEntry;
use std::io::Write;

pub const MAX_COMMAND_BYTES: usize = 1024 * 1024;
const MAX_MEMBERS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DirectoryApplyError {
    #[error("invalid directory command: {0}")]
    Invalid(String),
    #[error("directory command capacity exceeded")]
    Capacity,
    #[error("directory revision exhausted")]
    RevisionExhausted,
}
impl DirCmd {
    /// Strict, bounded decoding: the entire input must be exactly one command.
    pub fn decode(bytes: &[u8]) -> Result<Self, DirectoryApplyError> {
        if bytes.len() > MAX_COMMAND_BYTES {
            return Err(DirectoryApplyError::Capacity);
        }
        let cmd: Self = parrot_api::message::serde_remote_deserialize_with_limits(
            bytes,
            parrot_api::message::RemoteDecodeLimits {
                max_payload_bytes: MAX_COMMAND_BYTES,
                max_values: MAX_MEMBERS + 16,
                max_depth: 8,
            },
        )
        .map_err(DirectoryApplyError::Invalid)?;
        cmd.validate()?;
        Ok(cmd)
    }
    /// Encoding and decoding obey the same limits; reject before appending logs.
    pub fn encode(&self) -> Result<Vec<u8>, DirectoryApplyError> {
        self.validate()?;
        let mut counter = Counter(0);
        bincode::serde::encode_into_std_write(self, &mut counter, bincode::config::standard())
            .map_err(|_| DirectoryApplyError::Capacity)?;
        bincode::serde::encode_to_vec(self, bincode::config::standard())
            .map_err(|e| DirectoryApplyError::Invalid(e.to_string()))
    }
    fn validate(&self) -> Result<(), DirectoryApplyError> {
        let text = |s: &str, max: usize| {
            if s.is_empty() || s.len() > max || s.chars().any(char::is_control) {
                Err(DirectoryApplyError::Invalid(
                    "empty, oversized or control-containing field".into(),
                ))
            } else {
                Ok(())
            }
        };
        let members = |items: &[String], max: usize| {
            if items.len() > MAX_MEMBERS {
                return Err(DirectoryApplyError::Capacity);
            }
            for s in items {
                text(s, max)?;
            }
            Ok(())
        };
        match self {
            Self::Upsert { node, endpoints } => {
                text(node, 1024)?;
                members(endpoints, 4096)?;
            }
            Self::Remove { node } => text(node, 1024)?,
            Self::BorderDeclare { prefix, node } => {
                text(prefix, 1024)?;
                text(node, 1024)?;
            }
            Self::KeyAggregate { key, nodes } => {
                text(key, 1024)?;
                members(nodes, 1024)?;
            }
        }
        Ok(())
    }
}
impl DirectorySm {
    // Decode every entry before changing business state. A malformed later entry
    // must not apply a prefix or consume the host's pending committed queue.
    pub(super) fn prepare(&self, entries: &[LogEntry]) -> Result<Vec<DirCmd>, DirectoryApplyError> {
        self.version
            .checked_add(entries.len() as u64)
            .ok_or(DirectoryApplyError::RevisionExhausted)?;
        entries.iter().map(|e| DirCmd::decode(&e.cmd)).collect()
    }
}
struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_COMMAND_BYTES - self.0 {
            return Err(std::io::Error::other("directory command capacity"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
