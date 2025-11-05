//! A single cooperative disk owner for committed application state. Call these
//! synchronous methods on an owner thread, not an async IO worker. The private
//! directory and lock inode must remain stable; hostile filesystem writers and
//! distributed quorum/leadership are outside this local durability contract.
use super::state::{check_transition, DesiredStateRecord, DesiredStateStore};
use crate::ApplicationLimits;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};
const MAGIC: &[u8; 8] = b"PDES01\0\0";
const MAX_RECORD: usize = 32 * 1024 * 1024;
struct Inner {
    latest: Option<DesiredStateRecord>,
    failed: bool,
}
pub struct FileStateStore {
    directory: PathBuf,
    limits: ApplicationLimits,
    pid: u32,
    inner: Mutex<Inner>,
    _owner: File,
    #[cfg(test)]
    fail_stage: std::sync::atomic::AtomicU8,
}
impl FileStateStore {
    pub fn open(directory: impl AsRef<Path>, limits: ApplicationLimits) -> Result<Self, String> {
        let directory = directory.as_ref().to_path_buf();
        if !std::fs::symlink_metadata(&directory)
            .map_err(error)?
            .is_dir()
        {
            return Err("desired state requires an existing real directory".into());
        }
        let directory = std::fs::canonicalize(directory).map_err(error)?;
        let owner = regular(&directory.join(".parrot-desired.lock"), Access::Lock)?;
        owner
            .try_lock()
            .map_err(|e| format!("desired owner lock: {e}"))?;
        let main = directory.join("desired.state");
        let next = directory.join("desired.next");
        let latest = if exists(&main)? {
            let mut bytes = Vec::new();
            regular(&main, Access::Read)?
                .take((MAX_RECORD + 41) as u64)
                .read_to_end(&mut bytes)
                .map_err(error)?;
            if bytes.len() < 40
                || bytes.len() > MAX_RECORD + 40
                || &bytes[..8] != MAGIC
                || Sha256::digest(&bytes[40..])[..] != bytes[8..40]
            {
                return Err("corrupt or oversized desired state snapshot".into());
            }
            let state: DesiredStateRecord = serde_json::from_slice(&bytes[40..]).map_err(error)?;
            state.validate(&limits)?;
            // Publish could have reached rename before a failed directory sync.
            // Reopen establishes durability before exposing that generation.
            regular(&main, Access::Read)?.sync_all().map_err(error)?;
            File::open(&directory)
                .and_then(|f| f.sync_all())
                .map_err(error)?;
            Some(state)
        } else {
            if exists(&next)? {
                return Err("incomplete initial desired commit; refusing reset".into());
            }
            None
        };
        // Validate the committed snapshot before touching abandoned temporary data.
        if exists(&next)? {
            drop(regular(&next, Access::Read)?);
            std::fs::remove_file(&next).map_err(error)?;
            File::open(&directory)
                .and_then(|f| f.sync_all())
                .map_err(error)?;
        }
        File::open(
            directory
                .parent()
                .ok_or("desired directory needs a parent")?,
        )
        .and_then(|f| f.sync_all())
        .map_err(error)?;
        Ok(Self {
            directory,
            limits,
            pid: std::process::id(),
            inner: Mutex::new(Inner {
                latest,
                failed: false,
            }),
            _owner: owner,
            #[cfg(test)]
            fail_stage: std::sync::atomic::AtomicU8::new(0),
        })
    }
    fn check_owner(&self, inner: &Inner) -> Result<(), String> {
        if self.pid != std::process::id() || inner.failed {
            Err("desired state owner failed or inherited across fork; reopen required".into())
        } else {
            Ok(())
        }
    }
    #[cfg(test)]
    fn checkpoint(&self, stage: u8) -> Result<(), String> {
        if self.fail_stage.load(std::sync::atomic::Ordering::Relaxed) == stage {
            Err(format!("injected storage failure at stage {stage}"))
        } else {
            Ok(())
        }
    }
    fn ensure_pid(&self) -> Result<(), String> {
        if self.pid != std::process::id() {
            Err("desired state inherited across fork; reopen required".into())
        } else {
            Ok(())
        }
    }
    fn publish(&self, bytes: &[u8]) -> Result<(), String> {
        let next = self.directory.join("desired.next");
        let mut file = regular(&next, Access::New)?;
        file.write_all(MAGIC).map_err(error)?;
        file.write_all(&Sha256::digest(bytes)).map_err(error)?;
        file.write_all(bytes).map_err(error)?;
        #[cfg(test)]
        self.checkpoint(1)?;
        file.sync_all().map_err(error)?;
        #[cfg(test)]
        self.checkpoint(2)?;
        drop(file);
        std::fs::rename(next, self.directory.join("desired.state")).map_err(error)?;
        #[cfg(test)]
        self.checkpoint(3)?;
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(error)
    }
}
impl DesiredStateStore for FileStateStore {
    fn commit(&self, expected: Option<u64>, state: &DesiredStateRecord) -> Result<(), String> {
        self.ensure_pid()?;
        state.validate(&self.limits)?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "desired state lock poisoned")?;
        self.check_owner(&inner)?;
        check_transition(inner.latest.as_ref(), expected, state)?;
        let bytes = encode(state)?;
        if let Err(error) = self.publish(&bytes) {
            inner.failed = true;
            return Err(error);
        }
        inner.latest = Some(state.clone());
        Ok(())
    }
    fn current_revision(&self) -> Result<Option<u64>, String> {
        self.ensure_pid()?;
        let inner = self
            .inner
            .lock()
            .map_err(|_| "desired state lock poisoned")?;
        self.check_owner(&inner)?;
        Ok(inner.latest.as_ref().map(|state| state.revision))
    }
    fn latest(&self) -> Result<Option<DesiredStateRecord>, String> {
        self.ensure_pid()?;
        let inner = self
            .inner
            .lock()
            .map_err(|_| "desired state lock poisoned")?;
        self.check_owner(&inner)?;
        Ok(inner.latest.clone())
    }
}
fn encode(state: &DesiredStateRecord) -> Result<Vec<u8>, String> {
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_RECORD.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("desired snapshot exceeds 32 MiB"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded(Vec::new());
    serde_json::to_writer(&mut writer, state).map_err(error)?;
    Ok(writer.0)
}
fn exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(error(e)),
    }
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
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(error)?;
    let metadata = file.metadata().map_err(error)?;
    if !metadata.is_file() {
        return Err("desired state path is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("desired state files must not have hard links".into());
        }
    }
    Ok(file)
}
fn error(error: impl std::fmt::Display) -> String {
    format!("desired storage: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArtifactRef, ComponentSpec, EngineKind, InstancePolicy};
    fn record(revision: u64) -> DesiredStateRecord {
        DesiredStateRecord {
            mirrors: Default::default(),
            revision,
            manifest: crate::AppManifest {
                name: "disk-boundaries".into(),
                version: format!("{revision}.0.0"),
                components: vec![ComponentSpec {
                    name: "a".into(),
                    engine: EngineKind::Parrot,
                    artifact: ArtifactRef::Props {
                        factory: "echo".into(),
                    },
                    alt_artifact: None,
                    instances: InstancePolicy::Singleton,
                    placement: Default::default(),
                    upgrade: Default::default(),
                    deps: vec![],
                    config: None,
                    hooks: Default::default(),
                }],
                wiring: vec![],
                mirrors: Vec::new(),
                config_overlay: None,
            },
            pending: Default::default(),
        }
    }
    #[test]
    fn failed_publication_before_and_after_rename_requires_recovery() {
        for stage in 1..=3 {
            let dir = tempfile::tempdir().unwrap();
            let store = FileStateStore::open(dir.path(), Default::default()).unwrap();
            store.commit(None, &record(1)).unwrap();
            store
                .fail_stage
                .store(stage, std::sync::atomic::Ordering::Relaxed);
            assert!(store.commit(Some(1), &record(2)).is_err());
            assert!(store.latest().is_err());
            assert!(store.current_revision().is_err());
            assert!(store.commit(Some(1), &record(2)).is_err());
            drop(store);
            let recovered = FileStateStore::open(dir.path(), Default::default()).unwrap();
            assert_eq!(
                recovered.latest().unwrap().unwrap().revision,
                if stage == 3 { 2 } else { 1 }
            );
            assert!(!dir.path().join("desired.next").exists());
        }
    }
    #[test]
    fn inherited_owner_rejects_before_accessing_possibly_inherited_mutex() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileStateStore::open(dir.path(), Default::default()).unwrap();
        store.pid = 0;
        let _held = store.inner.lock().unwrap();
        assert!(store.latest().is_err());
        assert!(store.current_revision().is_err());
        assert!(store.commit(None, &record(1)).is_err());
    }
}
