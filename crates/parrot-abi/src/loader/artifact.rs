//! Immutable, private staging of verified native code. Preparation does not
//! execute code; loading still requires the caller's explicit trust decision.
use super::{read_artifact, scan_bytes, DylibHandle, DylibLoader, LoadError, NativeArtifactLimits};
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path, sync::Arc};

#[derive(Debug)]
pub(super) struct StagedFile {
    pub path: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

/// A bounded, SHA-256-verified native artifact in a private temporary directory.
/// Clones and loaded handles share its lifetime. The original input may be
/// changed or removed after preparation. Dependencies are not copied or verified;
/// relative library dependencies resolve relative to the staging directory.
/// This is integrity protection, not a sandbox or a signature/trust authority.
#[derive(Clone, Debug)]
pub struct NativeArtifact {
    staged: Arc<StagedFile>,
}
impl NativeArtifact {
    /// Read a regular file once, enforce budgets and scan, then stage those exact
    /// bytes. A nonempty SHA-256 (hex64 or `sha256:hex64`) is mandatory.
    /// This synchronous operation belongs on a blocking worker in async hosts.
    pub fn prepare(
        path: &Path,
        digest: &str,
        limits: NativeArtifactLimits,
    ) -> Result<Self, LoadError> {
        Self::validate_digest(digest)?;
        let bytes = read_artifact(path, limits)?;
        Self::prepare_bytes(&bytes, digest, limits)
    }

    /// Stage a verified copy of the supplied bytes without executing native code.
    pub fn prepare_bytes(
        bytes: &[u8],
        digest: &str,
        limits: NativeArtifactLimits,
    ) -> Result<Self, LoadError> {
        let want = Self::validate_digest(digest)?;
        limits.validate()?;
        if bytes.len() > limits.max_file_bytes {
            return Err(limits.exceeded());
        }
        let got = hex::encode(Sha256::digest(bytes));
        if got != want {
            return Err(LoadError::Digest { want, got });
        }
        let violations = scan_bytes(bytes, limits)?;
        if !violations.is_empty() {
            return Err(LoadError::Forbidden(violations));
        }
        let stage = || -> std::io::Result<StagedFile> {
            let mut builder = tempfile::Builder::new();
            builder.prefix("parrot-native-");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Set at creation, not via a later chmod with an exposure window.
                builder.permissions(std::fs::Permissions::from_mode(0o700));
            }
            let directory = builder.tempdir()?;
            let path = directory
                .path()
                .join(format!("component.{}", std::env::consts::DLL_EXTENSION));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&path)?;
            file.write_all(bytes)?;
            // The writer is closed before dlopen, including on platforms that
            // deny mapping an executable file while it is open for writing.
            drop(file);
            let mut permissions = std::fs::metadata(&path)?.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&path, permissions)?;
            Ok(StagedFile {
                path,
                _directory: directory,
            })
        };
        Ok(Self {
            staged: Arc::new(stage().map_err(|e| LoadError::Open(format!("native staging: {e}")))?),
        })
    }

    fn validate_digest(digest: &str) -> Result<String, LoadError> {
        let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(LoadError::InvalidConfig(
                "native artifact requires a SHA-256 digest (64 hex digits)".into(),
            ));
        }
        Ok(digest.to_ascii_lowercase())
    }

    /// Load from the owned snapshot, retaining it until native cleanup completes.
    ///
    /// # Safety
    /// All code/ABI/hook/dependency trust obligations of [`DylibLoader::load`]
    /// apply. The caller need not preserve the original input path, but must not
    /// interfere with the private staging directory (including via other code
    /// running with this process's credentials). Native initializers execute.
    pub unsafe fn load(&self) -> Result<DylibHandle, LoadError> {
        unsafe { DylibLoader::open(&self.staged.path, Some(self.staged.clone())) }
    }
}

#[cfg(test)]
#[path = "../../tests/common/mod.rs"]
mod fixtures;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn staging_outlives_cancelled_cleanup_wait_and_is_removed_after_native_completion() {
        let _lock = fixtures::load_mutex();
        let bytes = std::fs::read(fixtures::testcomp()).unwrap();
        let artifact = NativeArtifact::prepare_bytes(
            &bytes,
            &format!("{:x}", Sha256::digest(&bytes)),
            Default::default(),
        )
        .unwrap();
        let path = artifact.staged.path.clone();
        let directory = path.parent().unwrap().to_owned();
        // Keep an independent library reference while observing native counters;
        // it protects these test function pointers until all observations finish.
        let observer = unsafe { libloading::Library::new(&path) }.unwrap();
        let reset = unsafe { observer.get::<unsafe extern "C" fn()>(b"lifecycle_reset") }.unwrap();
        let observe =
            unsafe { observer.get::<unsafe extern "C" fn(u32) -> u64>(b"lifecycle_observe") }
                .unwrap();
        let release =
            unsafe { observer.get::<unsafe extern "C" fn()>(b"lifecycle_release") }.unwrap();
        unsafe {
            reset();
        }
        let handle = unsafe { artifact.load() }.unwrap();
        DylibLoader.construct(&handle, "gate").unwrap();
        drop(artifact);
        let pending = DylibLoader.unload(handle, Duration::from_secs(5));
        let until = Instant::now() + Duration::from_secs(10);
        while unsafe { observe(0) } == 0 {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(pending);
        assert!(path.is_file());
        assert_eq!(unsafe { observe(1) }, 0);
        unsafe {
            release();
        }
        while directory.exists() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(unsafe { observe(0) }, 1);
        assert_eq!(unsafe { observe(1) }, 1);
        assert_eq!(unsafe { observe(2) }, 0);
    }

    #[test]
    fn private_staging_permissions_and_last_owner_removal() {
        let _lock = fixtures::load_mutex();
        use std::os::unix::fs::PermissionsExt;
        let bytes = std::fs::read(fixtures::testcomp()).unwrap();
        let artifact = NativeArtifact::prepare_bytes(
            &bytes,
            &format!("{:x}", Sha256::digest(&bytes)),
            Default::default(),
        )
        .unwrap();
        let path = artifact.staged.path.clone();
        let directory = path.parent().unwrap().to_owned();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o222,
            0
        );
        let clone = artifact.clone();
        drop(artifact);
        assert!(path.is_file());
        drop(clone);
        assert!(!directory.exists());
    }
}
