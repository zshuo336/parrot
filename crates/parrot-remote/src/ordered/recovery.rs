//! Cooperative local-filesystem ownership, retained through real worker cleanup.
use super::Identity;
pub mod handoff;
pub(crate) mod sender;
use parrot_config::OrderedReceiverState;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::Arc,
};

#[cfg(all(test, unix))]
mod tests;

const MAGIC: &[u8; 8] = b"PORCV01\0";
const LIMIT: usize = 8192;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    realm: String,
    node: String,
    anchor: [u64; 2],
    namespace: Identity,
    epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff: Option<handoff::Phase>,
}

struct Lock {
    file: File,
    pid: u32,
}
impl Drop for Lock {
    fn drop(&mut self) {
        if self.pid == std::process::id() {
            let _ = self.file.unlock();
        }
    }
}

pub(crate) struct ReceiverOwner {
    directory: std::path::PathBuf,
    receiver: Identity,
    namespace: Identity,
    epoch: u64,
    lock: Lock,
}
struct LockedDirectory {
    directory: std::path::PathBuf,
    lock: Lock,
}

impl LockedDirectory {
    fn load(config: &OrderedReceiverState, node: &str) -> Result<(Self, Saved), String> {
        config.validate()?;
        parrot_config::endpoint::validate_node_id(node)?;
        if !std::fs::symlink_metadata(&config.directory)
            .map_err(error)?
            .is_dir()
        {
            return Err("ordered receiver requires an existing real directory".into());
        }
        let directory = std::fs::canonicalize(&config.directory).map_err(error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&directory).map_err(error)?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
                return Err(
                    "ordered receiver directory must be owned and not writable by other users"
                        .into(),
                );
            }
        }
        let file = regular(&directory.join(".parrot-ordered.lock"), Access::Lock)?;
        file.try_lock()
            .map_err(|e| format!("ordered receiver owner lock: {e}"))?;
        let lock = Lock {
            file,
            pid: std::process::id(),
        };
        let anchor = anchor(&lock.file)?;
        lock.file.sync_all().map_err(error)?;
        let main = directory.join("ordered.state");
        let next = directory.join("ordered.next");
        let saved = match std::fs::symlink_metadata(&main) {
            Ok(_) => {
                let mut bytes = Vec::new();
                regular(&main, Access::Read)?
                    .take((LIMIT + 41) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(error)?;
                if bytes.len() < 40
                    || bytes.len() > LIMIT + 40
                    || &bytes[..8] != MAGIC
                    || Sha256::digest(&bytes[40..])[..] != bytes[8..40]
                {
                    return Err("corrupt ordered receiver state".into());
                }
                let saved: Saved = serde_json::from_slice(&bytes[40..]).map_err(error)?;
                if saved.realm != config.realm
                    || saved.node != node
                    || saved.anchor != anchor
                    || saved.namespace == [0; 16]
                    || (saved.epoch == 0) != matches!(saved.handoff, Some(handoff::Phase::Prepared))
                {
                    return Err("ordered receiver state binding differs".into());
                }
                saved
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(&next).is_ok() {
                    return Err("incomplete initial ordered receiver publication".into());
                }
                let namespace = super::identity();
                Saved {
                    realm: config.realm.clone(),
                    node: node.into(),
                    anchor,
                    namespace,
                    epoch: 0,
                    handoff: None,
                }
            }
            Err(e) => return Err(error(e)),
        };
        Ok((Self { directory, lock }, saved))
    }

    fn publish(&self, saved: &Saved) -> Result<(), String> {
        let directory = &self.directory;
        let main = directory.join("ordered.state");
        let next = directory.join("ordered.next");
        match std::fs::symlink_metadata(&next) {
            Ok(_) => {
                // A previous open never advertised its unpublished temporary.
                drop(regular(&next, Access::Read)?);
                std::fs::remove_file(&next).map_err(error)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(error(e)),
        }
        let bytes = serde_json::to_vec(saved).map_err(error)?;
        if bytes.len() > LIMIT {
            return Err("ordered receiver state capacity exceeded".into());
        }
        let mut file = regular(&next, Access::New)?;
        file.write_all(MAGIC).map_err(error)?;
        file.write_all(&Sha256::digest(&bytes)).map_err(error)?;
        file.write_all(&bytes).map_err(error)?;
        file.sync_all().map_err(error)?;
        std::fs::rename(&next, &main).map_err(error)?;
        File::open(directory)
            .map_err(error)?
            .sync_all()
            .map_err(error)?;
        File::open(directory.parent().unwrap_or(Path::new("/")))
            .map_err(error)?
            .sync_all()
            .map_err(error)?;
        Ok(())
    }
}

impl ReceiverOwner {
    /// Open only on a startup owner thread; publication performs synchronous IO.
    pub fn open(config: &OrderedReceiverState, node: &str) -> Result<Arc<Self>, String> {
        let (locked, mut saved) = LockedDirectory::load(config, node)?;
        match saved.handoff {
            Some(handoff::Phase::Prepared) => return Err("receiver awaits handoff".into()),
            Some(handoff::Phase::Sealed { .. }) => return Err("receiver sealed for handoff".into()),
            _ => {}
        }
        saved.epoch = saved
            .epoch
            .checked_add(1)
            .ok_or("ordered receiver epoch exhausted")?;
        locked.publish(&saved)?;
        let receiver = receiver_identity(saved.namespace, saved.epoch);
        if receiver == [0; 16] {
            return Err("invalid ordered receiver identity".into());
        }
        Ok(Arc::new(Self {
            directory: locked.directory,
            receiver,
            namespace: saved.namespace,
            epoch: saved.epoch,
            lock: locked.lock,
        }))
    }

    pub fn valid_process(&self) -> bool {
        self.lock.pid == std::process::id()
    }
    pub fn receiver(&self) -> Identity {
        self.receiver
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn proves_retired(&self, receiver: Identity, epoch: u64) -> bool {
        self.valid_process()
            && epoch > 0
            && epoch < self.epoch
            && receiver_identity(self.namespace, epoch) == receiver
    }
}

fn receiver_identity(namespace: Identity, epoch: u64) -> Identity {
    let mut hash = Sha256::new();
    hash.update(b"parrot:ordered-receiver#v1");
    hash.update(namespace);
    hash.update(epoch.to_le_bytes());
    hash.finalize()[..16].try_into().unwrap()
}

enum Access {
    Read,
    Lock,
    New,
}
fn regular(path: &Path, access: Access) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    match access {
        Access::Read => {}
        Access::Lock => {
            options.write(true).create(true);
        }
        Access::New => {
            options.write(true).create_new(true);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    let file = options.open(path).map_err(error)?;
    if !file.metadata().map_err(error)?.is_file() {
        return Err("regular ordered state file required".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().map_err(error)?;
        if metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err("private single-link ordered state file required".into());
        }
    }
    Ok(file)
}
#[cfg(unix)]
fn anchor(file: &File) -> Result<[u64; 2], String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(error)?;
    Ok([metadata.dev(), metadata.ino()])
}
#[cfg(not(unix))]
fn anchor(_: &File) -> Result<[u64; 2], String> {
    Err("ordered receiver recovery requires Unix local file identity and locking".into())
}
fn error(error: impl std::fmt::Display) -> String {
    format!("ordered receiver state: {error}")
}
