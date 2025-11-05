//! Temporary compiler files shared by their caller and every process using them.
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::sync::Arc;

pub(super) struct PrivateArtifacts {
    directory: Option<tempfile::TempDir>,
    preserve: AtomicBool,
}

impl PrivateArtifacts {
    pub(super) fn new(directory: tempfile::TempDir) -> Self {
        Self {
            directory: Some(directory),
            preserve: AtomicBool::new(false),
        }
    }

    pub(super) fn path(&self) -> &std::path::Path {
        self.directory.as_ref().expect("live artifact owner").path()
    }

    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    fn preserve(&self) {
        // Sticky across all users: a later successful compiler or gateway must
        // not erase another owner's uncertain retirement.
        self.preserve.store(true, Ordering::Release);
    }
}

impl Drop for PrivateArtifacts {
    fn drop(&mut self) {
        if self.preserve.load(Ordering::Acquire) {
            if let Some(directory) = self.directory.take() {
                let _ = directory.keep();
            }
        }
    }
}

// Created only after a process starts. Drop also covers an owner unwind and
// thread-spawn failure; neither proves that the process group has retired.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(super) struct RetirementArtifacts {
    directory: Arc<PrivateArtifacts>,
    retired: AtomicBool,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl RetirementArtifacts {
    pub(super) fn new(directory: Arc<PrivateArtifacts>) -> Self {
        Self {
            directory,
            retired: AtomicBool::new(false),
        }
    }

    pub(super) fn retired(&self) {
        self.retired.store(true, Ordering::Release);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl Drop for RetirementArtifacts {
    fn drop(&mut self) {
        if !self.retired.load(Ordering::Acquire) {
            self.directory.preserve();
            tracing::error!(path = %self.directory.path().display(), "gateway retirement unproven; private artifacts retained");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unstarted_or_confirmed_owners_release_files() {
        for started in [false, true] {
            let artifacts = Arc::new(PrivateArtifacts::new(tempfile::tempdir().unwrap()));
            let path = artifacts.path().to_owned();
            if started {
                let owner = RetirementArtifacts::new(artifacts.clone());
                owner.retired();
                drop(owner);
                assert!(path.is_dir(), "caller still owns these files");
            }
            drop(artifacts);
            assert!(!path.exists());
        }
    }

    #[test]
    fn owner_unwind_preserves_files_but_releases_shared_metadata() {
        let artifacts = Arc::new(PrivateArtifacts::new(tempfile::tempdir().unwrap()));
        let path = artifacts.path().to_owned();
        std::fs::write(path.join("component.beam"), b"artifact").unwrap();
        let weak = Arc::downgrade(&artifacts);
        let result = std::panic::catch_unwind(move || {
            let _owner = RetirementArtifacts::new(artifacts);
            panic!("owner failure before retirement confirmation");
        });
        assert!(result.is_err());
        assert!(weak.upgrade().is_none(), "retention leaked owner metadata");
        assert_eq!(
            std::fs::read(path.join("component.beam")).unwrap(),
            b"artifact"
        );
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn later_success_does_not_erase_another_owners_unknown_retirement() {
        let artifacts = Arc::new(PrivateArtifacts::new(tempfile::tempdir().unwrap()));
        let path = artifacts.path().to_owned();
        let weak = Arc::downgrade(&artifacts);
        let failed = RetirementArtifacts::new(artifacts.clone());
        let success = RetirementArtifacts::new(artifacts.clone());
        drop(failed);
        success.retired();
        drop(success);
        drop(artifacts);
        assert!(weak.upgrade().is_none());
        assert!(path.is_dir());
        std::fs::remove_dir_all(path).unwrap();
    }
}
