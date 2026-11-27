// SPDX-License-Identifier: AGPL-3.0-only
//! Linux same-kernel standby. A trusted local watcher activates a prepared
//! receiver only after the kernel reports the enrolled receiver process exited.
use super::{error, handoff};
use crate::{RemoteActorSystem, RemoteError};
use parrot_config::OrderedReceiverState;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::{
            fs::MetadataExt,
            net::{SocketAddr, UnixListener, UnixStream},
        },
    },
    time::Duration,
};

// Hashing the invitation gives each prepared directory its own abstract address.
// SO_PEERCRED authenticates both endpoints; no socket pathname survives a crash.
fn address(digest: &[u8; 32]) -> Result<SocketAddr, String> {
    SocketAddr::from_abstract_name(format!("parrot.standby.{}", hex::encode(digest))).map_err(error)
}

fn peer(stream: &UnixStream) -> Result<libc::ucred, String> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = size_of_val(&cred) as libc::socklen_t;
    // Both buffers are valid for the kernel's fixed SO_PEERCRED result.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(error(io::Error::last_os_error()));
    }
    if len as usize != size_of_val(&cred) || cred.uid != unsafe { libc::geteuid() } {
        return Err("standby requires the same trusted service UID".into());
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(error)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(error)?;
    Ok(cred)
}

fn peer_pidfd(stream: &UnixStream) -> Result<OwnedFd, String> {
    let mut fd: libc::c_int = -1;
    let mut len = size_of_val(&fd) as libc::socklen_t;
    // SO_PEERPIDFD pins the actual connecting process, without a PID lookup race.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut fd as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(error(io::Error::last_os_error()));
    }
    if fd < 0 {
        return Err("invalid standby pidfd".into());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    if len as usize != size_of_val(&fd) {
        return Err("invalid standby pidfd length".into());
    }
    Ok(owned)
}

// Validate the peer as seen in its own PID namespace before disclosing a seal.
// This also prevents a stream proxy from substituting its lifetime for the owner.
fn process_identity(pid: i32) -> Result<[u8; 12], String> {
    if pid <= 0
        || fs::read_link("/proc/self").map_err(error)?.to_str()
            != Some(&std::process::id().to_string())
    {
        return Err("standby requires procfs for the observer PID namespace".into());
    }
    let directory = format!("/proc/{pid}");
    let namespace = fs::metadata(format!("{directory}/ns/pid"))
        .map_err(error)?
        .ino();
    let status = fs::read_to_string(format!("{directory}/status")).map_err(error)?;
    let nested: u32 = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .and_then(|ids| ids.split_whitespace().last())
        .and_then(|id| id.parse().ok())
        .ok_or("missing standby process namespace identity")?;
    let mut id = [0; 12];
    id[..8].copy_from_slice(&namespace.to_le_bytes());
    id[8..].copy_from_slice(&nested.to_le_bytes());
    Ok(id)
}

fn send(stream: &mut UnixStream, bytes: &[u8]) -> Result<(), String> {
    stream
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .map_err(error)?;
    stream.write_all(bytes).map_err(error)
}

fn receive(stream: &mut UnixStream) -> Result<Vec<u8>, String> {
    let mut len = [0; 4];
    stream.read_exact(&mut len).map_err(error)?;
    let size = u32::from_le_bytes(len) as usize;
    if size > handoff::MAX_BYTES {
        return Err("standby document too large".into());
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).map_err(error)?;
    Ok(bytes)
}

fn wait_for_exit(owner: &OwnedFd) -> Result<(), String> {
    let mut poll = libc::pollfd {
        fd: owner.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // A pidfd becomes readable when the process exits, including all threads.
        if unsafe { libc::poll(&mut poll, 1, -1) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error(e));
        }
        if poll.revents & libc::POLLIN != 0 {
            return Ok(());
        }
        return Err(format!("unexpected standby pidfd events {}", poll.revents));
    }
}

/// Wait for one enrollment, then activate the prepared directory after process
/// exit. Run in an independent process on Linux 6.5+ in the same network
/// namespace with visibility of the receiver PID. Failure leaves startup gated.
pub fn watch(config: &OrderedReceiverState, node: &str) -> Result<(), String> {
    let invitation = handoff::prepare(config, node)?;
    let proof = witness(&invitation)?;
    handoff::activate(config, node, &proof, Sha256::digest(&proof).into())
}

/// Release a private seal for a remote standby after the enrolled process exits.
/// No access to the source or destination state directory is required.
pub fn witness(invitation: &[u8]) -> Result<Vec<u8>, String> {
    if invitation.len() > handoff::MAX_BYTES {
        return Err("standby invitation too large".into());
    }
    let listener =
        UnixListener::bind_addr(&address(&Sha256::digest(invitation).into())?).map_err(error)?;
    let (mut stream, cred) = loop {
        let (stream, _) = listener.accept().map_err(error)?;
        if let Ok(cred) = peer(&stream) {
            break (stream, cred);
        }
    };
    let pidfd = peer_pidfd(&stream)?;
    stream
        .write_all(&process_identity(cred.pid)?)
        .map_err(error)?;
    send(&mut stream, invitation)?;
    let proof = receive(&mut stream)?;
    handoff::verify_standby(invitation, &proof)?;
    // Once the complete registration arrives, loss of the ACK cannot cancel it.
    let _ = stream.write_all(b"R");
    drop(stream);
    drop(listener);
    wait_for_exit(&pidfd)?;
    Ok(proof)
}

impl RemoteActorSystem {
    /// Permanently reserve this receiver directory for one standby and enroll the
    /// current process with its same-UID Linux watcher. Supply the SHA-256 of
    /// the trusted target invitation. Retrying the same target is safe.
    pub async fn register_receiver_standby(
        &self,
        invitation_sha256: [u8; 32],
    ) -> Result<(), RemoteError> {
        if self.outbound_ordered.persists_tickets() {
            return Err(RemoteError::Transport(
                "standby cannot transfer a live sender journal".into(),
            ));
        }
        let owner = self.ingress.ordered_receiver_owner().ok_or_else(|| {
            RemoteError::Transport("standby requires a live persistent receiver".into())
        })?;
        tokio::task::spawn_blocking(move || {
            let mut stream =
                UnixStream::connect_addr(&address(&invitation_sha256)?).map_err(error)?;
            peer(&stream)?;
            let mut observed = [0; 12];
            stream.read_exact(&mut observed).map_err(error)?;
            if observed != process_identity(std::process::id() as i32)? {
                return Err("standby watcher observed a different process".into());
            }
            let invitation = receive(&mut stream)?;
            if Sha256::digest(&invitation)[..] != invitation_sha256 {
                return Err("standby invitation digest differs".into());
            }
            send(&mut stream, &owner.reserve_standby(&invitation)?)?;
            let mut ack = [0; 1];
            stream.read_exact(&mut ack).map_err(error)?;
            if ack != *b"R" {
                return Err("standby enrollment was not acknowledged".into());
            }
            Ok(())
        })
        .await
        .map_err(|e| RemoteError::Transport(error(e)))?
        .map_err(RemoteError::Transport)
    }
}
