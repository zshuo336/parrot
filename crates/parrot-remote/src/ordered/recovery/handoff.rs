// SPDX-License-Identifier: AGPL-3.0-only
//! Offline receiver handoff between trusted operators of two local state directories.
//! Exchange the invitation, seal and its digest over authenticated administration channels.
//! The seal contains private receiver state. An unavailable source directory cannot issue it.
use super::{error, sender::Journal, Identity, LockedDirectory};
use parrot_config::OrderedReceiverState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Maximum input size for either handoff document.
pub const MAX_BYTES: usize = 8192;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) enum Phase {
    Prepared,
    Sealed { destination: Identity },
    Activated { digest: [u8; 32] },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Invitation {
    version: u8,
    realm: String,
    node: String,
    destination: Identity,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seal {
    invitation: Invitation,
    namespace: Identity,
    epoch: u64,
}

impl Invitation {
    fn verify(&self, config: &OrderedReceiverState, node: &str) -> Result<(), String> {
        if self.version != 1
            || self.realm != config.realm
            || self.node != node
            || self.destination == [0; 16]
        {
            return Err("receiver handoff invitation binding differs".into());
        }
        Ok(())
    }
}

fn document<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value).map_err(error)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_BYTES {
        return Err("receiver handoff document too large".into());
    }
    Ok(bytes)
}

fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    if bytes.len() > MAX_BYTES {
        return Err("receiver handoff document too large".into());
    }
    serde_json::from_slice(bytes).map_err(error)
}

/// Reserve a fresh target without permitting runtime startup. Safe to retry.
pub fn prepare(config: &OrderedReceiverState, node: &str) -> Result<Vec<u8>, String> {
    let (locked, mut saved) = LockedDirectory::load(config, node)?;
    if saved.epoch != 0 {
        return Err("handoff target requires a fresh receiver directory".into());
    }
    for name in ["sender.state", "sender.next"] {
        match std::fs::symlink_metadata(locked.directory.join(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(error(e)),
            Ok(_) => return Err("handoff target contains sender state".into()),
        }
    }
    let invitation = document(&Invitation {
        version: 1,
        realm: saved.realm.clone(),
        node: saved.node.clone(),
        destination: saved.namespace,
    })?;
    saved.handoff = Some(Phase::Prepared);
    locked.publish(&saved)?;
    Ok(invitation)
}

/// Permanently seal the original directory for this target after its last owner exits.
/// A retry with the original invitation returns the same bytes, including after loss of the reply.
pub fn seal(
    config: &OrderedReceiverState,
    node: &str,
    invitation: &[u8],
) -> Result<Vec<u8>, String> {
    let invitation: Invitation = parse(invitation)?;
    invitation.verify(config, node)?;
    let (locked, mut saved) = LockedDirectory::load(config, node)?;
    if saved.epoch == 0 || saved.epoch == u64::MAX || invitation.destination == saved.namespace {
        return Err("receiver has no transferable epoch".into());
    }
    if matches!(saved.handoff, Some(Phase::Sealed { destination }) if destination != invitation.destination)
    {
        return Err("receiver already sealed for another target".into());
    }
    Journal::ensure_drained(&locked.directory, saved.namespace)?;
    let destination = invitation.destination;
    let proof = document(&Seal {
        invitation,
        namespace: saved.namespace,
        epoch: saved.epoch,
    })?;
    saved.handoff = Some(Phase::Sealed { destination });
    locked.publish(&saved)?;
    Ok(proof)
}

/// Activate only the prepared destination using the SHA-256 obtained from the trusted source.
/// The next runtime open advances beyond all epochs previously issued by that source.
pub fn activate(
    config: &OrderedReceiverState,
    node: &str,
    bytes: &[u8],
    digest: [u8; 32],
) -> Result<(), String> {
    let proof: Seal = parse(bytes)?;
    if Sha256::digest(bytes)[..] != digest {
        return Err("receiver handoff digest differs".into());
    }
    proof.invitation.verify(config, node)?;
    if proof.epoch == 0
        || proof.epoch == u64::MAX
        || proof.namespace == [0; 16]
        || proof.namespace == proof.invitation.destination
    {
        return Err("invalid receiver handoff origin".into());
    }
    let (locked, mut saved) = LockedDirectory::load(config, node)?;
    match saved.handoff {
        Some(Phase::Prepared) if saved.namespace == proof.invitation.destination => {
            saved.namespace = proof.namespace;
            saved.epoch = proof.epoch;
            saved.handoff = Some(Phase::Activated { digest });
        }
        Some(Phase::Activated { digest: old }) if old == digest => {}
        _ => return Err("receiver is not the prepared handoff target".into()),
    }
    locked.publish(&saved)
}
