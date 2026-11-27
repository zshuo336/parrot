// SPDX-License-Identifier: AGPL-3.0-only
use super::{enroll, error, handoff, wait_for_exit};
use crate::ordered::recovery::{regular, Access};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

const MAGIC: &[u8; 8] = b"POWIT01\0";
const MAX_RECORD: usize = 4 * handoff::MAX_BYTES;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Kernel {
    boot_id: [u8; 16],
    namespace: [u64; 2],
}

impl Kernel {
    fn current() -> Result<Self, String> {
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(error)?;
        let ns = fs::metadata("/proc/self/ns/pid").map_err(error)?;
        Ok(Self {
            boot_id: *uuid::Uuid::parse_str(boot.trim())
                .map_err(error)?
                .as_bytes(),
            namespace: [ns.dev(), ns.ino()],
        })
    }
}

#[derive(Serialize, Deserialize)]
enum Phase {
    Waiting { kernel: Kernel, pid: i32 },
    Released,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    phase: Phase,
    seal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    guest: Option<[u8; 16]>,
}

struct Journal {
    directory: File,
    path: PathBuf,
}

impl Journal {
    fn open(path: &Path) -> Result<Self, String> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(error)?;
        let stat = directory.metadata().map_err(error)?;
        if stat.uid() != unsafe { libc::geteuid() } || stat.mode() & 0o077 != 0 {
            return Err(
                "witness journal requires a private directory owned by the service UID".into(),
            );
        }
        directory
            .try_lock()
            .map_err(|e| format!("witness journal lock: {e}"))?;
        let pinned = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        File::open(pinned.join(".."))
            .and_then(|parent| parent.sync_all())
            .map_err(error)?;
        // Complete any previous rename whose directory sync did not return.
        directory.sync_all().map_err(error)?;
        Ok(Self {
            directory,
            path: pinned,
        })
    }

    fn read(&self) -> Result<Option<Record>, String> {
        let path = self.path.join("witness.state");
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(error(e)),
            Ok(_) => {}
        }
        let mut bytes = Vec::new();
        regular(&path, Access::Read)?
            .take((MAX_RECORD + 41) as u64)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        if bytes.len() < 40
            || bytes.len() > MAX_RECORD + 40
            || &bytes[..8] != MAGIC
            || Sha256::digest(&bytes[40..])[..] != bytes[8..40]
        {
            return Err("corrupt witness journal".into());
        }
        serde_json::from_slice(&bytes[40..])
            .map(Some)
            .map_err(error)
    }

    fn publish(&self, phase: Phase, proof: &[u8], guest: Option<[u8; 16]>) -> Result<(), String> {
        let bytes = serde_json::to_vec(&Record {
            phase,
            seal: std::str::from_utf8(proof).map_err(error)?.to_owned(),
            guest,
        })
        .map_err(error)?;
        if bytes.len() > MAX_RECORD {
            return Err("witness journal capacity exceeded".into());
        }
        let next = self.path.join("witness.next");
        match fs::symlink_metadata(&next) {
            Ok(_) => {
                drop(regular(&next, Access::Read)?);
                fs::remove_file(&next).map_err(error)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(error(e)),
        }
        let mut file = regular(&next, Access::New)?;
        file.write_all(MAGIC)
            .and_then(|_| file.write_all(&Sha256::digest(&bytes)))
            .and_then(|_| file.write_all(&bytes))
            .and_then(|_| file.sync_all())
            .map_err(error)?;
        fs::rename(next, self.path.join("witness.state")).map_err(error)?;
        self.directory.sync_all().map_err(error)
    }
}

fn resume(kernel: Kernel, pid: i32) -> Result<(), String> {
    if pid <= 0 || kernel != Kernel::current()? {
        return Err(
            "witness recovery requires the original boot and observer PID namespace".into(),
        );
    }
    // Within one PID namespace a number can be reassigned only after its old
    // process exits. If already reused, waiting for the new holder is conservative.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    if fd >= 0 {
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        return wait_for_exit(&pidfd);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ESRCH)
        && unsafe { libc::kill(pid, 0) } == -1
        && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        return Ok(());
    }
    Err(error(e))
}

/// Enroll once and retain the released seal for repeat delivery. Use one private
/// local directory per invitation. Pending recovery requires the same kernel
/// boot and observer PID namespace. Only expose the command output to consumers.
pub fn durable_witness(invitation: &[u8], directory: &Path) -> Result<Vec<u8>, String> {
    observe(invitation, directory, None, |save| enroll(invitation, save))
}

pub(super) type SaveEnrollment<'a> = dyn FnMut(i32, &[u8]) -> Result<(), String> + 'a;

pub(super) fn observe(
    invitation: &[u8],
    directory: &Path,
    guest: Option<[u8; 16]>,
    register: impl FnOnce(&mut SaveEnrollment<'_>) -> Result<(Vec<u8>, OwnedFd), String>,
) -> Result<Vec<u8>, String> {
    let journal = Journal::open(directory)?;
    let proof = match journal.read()? {
        Some(record) => {
            if record.guest != guest {
                return Err("witness guest binding differs".into());
            }
            let proof = record.seal.into_bytes();
            handoff::verify_standby(invitation, &proof)?;
            match record.phase {
                Phase::Released => return Ok(proof),
                Phase::Waiting { kernel, pid } => resume(kernel, pid)?,
            }
            proof
        }
        None => {
            let (proof, pidfd) = register(&mut |pid, proof| {
                journal.publish(
                    Phase::Waiting {
                        kernel: Kernel::current()?,
                        pid,
                    },
                    proof,
                    guest,
                )
            })?;
            wait_for_exit(&pidfd)?;
            proof
        }
    };
    journal.publish(Phase::Released, &proof, guest)?;
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_pid_in_a_different_boot_or_namespace_cannot_release_a_seal() {
        let mut other_boot = Kernel::current().unwrap();
        other_boot.boot_id[0] ^= 1;
        assert!(resume(other_boot, i32::MAX)
            .unwrap_err()
            .contains("original boot"));
        let mut other_namespace = Kernel::current().unwrap();
        other_namespace.namespace[1] ^= 1;
        assert!(resume(other_namespace, i32::MAX)
            .unwrap_err()
            .contains("PID namespace"));
    }
}
