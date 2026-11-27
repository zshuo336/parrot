// SPDX-License-Identifier: AGPL-3.0-only
//! Offline receiver handoff between trusted operators of two local state directories.
//! Exchange the invitation, seal and its digest over authenticated administration channels.
//! The seal contains private receiver state. An unavailable source directory cannot issue it.
use super::{error, receiver_identity, sender::Journal, Identity, LockedDirectory};
use parrot_config::OrderedReceiverState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Maximum input size for either handoff document.
pub const MAX_BYTES: usize = 8192;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) enum Phase {
    Prepared,
    PreparedFrom { receiver: Identity },
    Sealed { destination: Identity },
    Activated { digest: [u8; 32] },
}

impl Phase {
    fn prepared(receiver: Option<Identity>) -> Self {
        match receiver {
            Some(receiver) => Self::PreparedFrom { receiver },
            None => Self::Prepared,
        }
    }

    pub(super) fn is_prepared(&self) -> bool {
        matches!(self, Self::Prepared | Self::PreparedFrom { .. })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Invitation {
    version: u8,
    realm: String,
    node: String,
    destination: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<Identity>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seal {
    invitation: Invitation,
    namespace: Identity,
    epoch: u64,
}

impl Invitation {
    fn verify(&self, realm: &str, node: &str) -> Result<(), String> {
        let expected_version = if self.source.is_some() { 2 } else { 1 };
        if self.version != expected_version
            || self.source == Some([0; 16])
            || self.realm != realm
            || self.node != node
            || self.destination == [0; 16]
        {
            return Err("receiver handoff invitation binding differs".into());
        }
        Ok(())
    }

    fn verify_source(&self, namespace: Identity, epoch: u64) -> Result<(), String> {
        if self
            .source
            .is_some_and(|id| id != receiver_identity(namespace, epoch))
        {
            return Err("handoff requires the enrolled source receiver and epoch".into());
        }
        Ok(())
    }
}

impl crate::RemoteActorSystem {
    /// Identity of this persistent ordered receiver's current incarnation. Obtain it
    /// over a trusted administration channel before preparing a source-bound standby.
    pub fn ordered_receiver_id(&self) -> Option<[u8; 16]> {
        self.ingress
            .ordered_receiver_owner()
            .filter(|owner| owner.valid_process())
            .map(|owner| owner.receiver())
    }
}

#[cfg(target_os = "linux")]
impl super::ReceiverOwner {
    pub(super) fn reserve_standby(&self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        if !self.valid_process() {
            return Err("standby registration requires the owning process".into());
        }
        let invitation: Invitation = parse(bytes)?;
        let mut saved = self.saved.lock().map_err(error)?;
        use std::os::fd::AsRawFd;
        let dir = std::fs::File::open(&self.directory).map_err(error)?;
        let pinned = std::path::PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        let lock = super::regular(&pinned.join(".parrot-ordered.lock"), super::Access::Read)?;
        if super::anchor(&lock)? != saved.anchor {
            return Err("standby source directory was replaced".into());
        }
        invitation.verify(&saved.realm, &saved.node)?;
        invitation.verify_source(saved.namespace, saved.epoch)?;
        if invitation.destination == saved.namespace || saved.epoch == u64::MAX {
            return Err("standby invitation does not match this receiver".into());
        }
        if matches!(saved.handoff, Some(Phase::Sealed { destination }) if destination != invitation.destination)
        {
            return Err("receiver already reserved for another standby".into());
        }
        let destination = invitation.destination;
        let proof = document(&Seal {
            invitation,
            namespace: saved.namespace,
            epoch: saved.epoch,
        })?;
        // Keep the same target on retry even if directory sync fails after rename.
        saved.handoff = Some(Phase::Sealed { destination });
        saved.publish(&pinned)?;
        Ok(proof)
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

#[cfg(target_os = "linux")]
pub(super) fn verify_standby(invitation: &[u8], proof: &[u8]) -> Result<(), String> {
    let seal: Seal = parse(proof)?;
    if document(&seal.invitation)? != invitation
        || seal.epoch == 0
        || seal.epoch == u64::MAX
        || seal.namespace == [0; 16]
        || seal.namespace == seal.invitation.destination
    {
        return Err("invalid standby enrollment".into());
    }
    seal.invitation
        .verify(&seal.invitation.realm, &seal.invitation.node)?;
    seal.invitation.verify_source(seal.namespace, seal.epoch)
}

/// Reserve a fresh target without permitting runtime startup. Safe to retry.
pub fn prepare(config: &OrderedReceiverState, node: &str) -> Result<Vec<u8>, String> {
    prepare_target(config, node, None)
}

/// Reserve the destination for one specific receiver incarnation. A different
/// process with the same node name, including a later restart, cannot seal for it.
pub fn prepare_from(
    config: &OrderedReceiverState,
    node: &str,
    receiver: [u8; 16],
) -> Result<Vec<u8>, String> {
    if receiver == [0; 16] {
        return Err("invalid source receiver identity".into());
    }
    prepare_target(config, node, Some(receiver))
}

fn prepare_target(
    config: &OrderedReceiverState,
    node: &str,
    source: Option<Identity>,
) -> Result<Vec<u8>, String> {
    let (locked, mut saved) = LockedDirectory::load(config, node)?;
    if saved.epoch != 0 {
        return Err("handoff target requires a fresh receiver directory".into());
    }
    let prepared = Phase::prepared(source);
    if saved
        .handoff
        .as_ref()
        .is_some_and(|phase| *phase != prepared)
    {
        return Err("handoff target already prepared with a different source binding".into());
    }
    for name in ["sender.state", "sender.next"] {
        match std::fs::symlink_metadata(locked.directory.join(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(error(e)),
            Ok(_) => return Err("handoff target contains sender state".into()),
        }
    }
    let invitation = document(&Invitation {
        version: if source.is_some() { 2 } else { 1 },
        realm: saved.realm.clone(),
        node: saved.node.clone(),
        destination: saved.namespace,
        source,
    })?;
    saved.handoff = Some(prepared);
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
    invitation.verify(&config.realm, node)?;
    let (locked, mut saved) = LockedDirectory::load(config, node)?;
    invitation.verify_source(saved.namespace, saved.epoch)?;
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
    proof.invitation.verify(&config.realm, node)?;
    proof
        .invitation
        .verify_source(proof.namespace, proof.epoch)?;
    if proof.epoch == 0
        || proof.epoch == u64::MAX
        || proof.namespace == [0; 16]
        || proof.namespace == proof.invitation.destination
    {
        return Err("invalid receiver handoff origin".into());
    }
    let (locked, mut saved) = LockedDirectory::load(config, node)?;
    match saved.handoff {
        Some(ref phase)
            if *phase == Phase::prepared(proof.invitation.source)
                && saved.namespace == proof.invitation.destination =>
        {
            saved.namespace = proof.namespace;
            saved.epoch = proof.epoch;
            saved.handoff = Some(Phase::Activated { digest });
        }
        Some(Phase::Activated { digest: old }) if old == digest => {}
        _ => return Err("receiver is not the prepared handoff target".into()),
    }
    locked.publish(&saved)
}
