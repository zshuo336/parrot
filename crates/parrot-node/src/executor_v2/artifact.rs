use parrot_remote::admin_v2::{v2_err, AdminArtifactRef};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

/// Legacy local artifact inspection. No download or cache population occurs.
/// Execution must use verified bytes, never reopen the returned path as proof
/// of integrity. The Wasm executor uses the app host's same-byte admission.
#[derive(Debug, Clone, Default)]
pub struct ArtifactChannel;
impl ArtifactChannel {
    pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
    /// Configured directory name retained for callers; this channel does not
    /// populate it or treat its contents as trusted.
    pub fn cache_dir() -> PathBuf {
        std::env::var_os("PARROT_ARTIFACT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/parrot-artifacts"))
    }
    /// Inspect a local path and digest. The returned path is mutable and is NOT
    /// a verified execution handle. Use [`Self::fetch_bytes`] when loading code.
    pub fn fetch(&self, artifact: &AdminArtifactRef) -> Result<PathBuf, (u16, String)> {
        if matches!(artifact, AdminArtifactRef::Props { .. }) {
            return Ok(PathBuf::new());
        }
        let (path, digest) = Self::reference(artifact)?;
        Self::read_verified(&path, digest)?;
        Ok(path)
    }
    /// Own the exact bytes whose mandatory digest was verified under the input
    /// limit. Props has no external artifact and returns an empty buffer.
    pub fn fetch_bytes(&self, artifact: &AdminArtifactRef) -> Result<Vec<u8>, (u16, String)> {
        if matches!(artifact, AdminArtifactRef::Props { .. }) {
            return Ok(Vec::new());
        }
        let (path, digest) = Self::reference(artifact)?;
        Self::read_verified(&path, digest)
    }
    fn reference(artifact: &AdminArtifactRef) -> Result<(PathBuf, &str), (u16, String)> {
        match artifact {
            AdminArtifactRef::Wasm { digest, uri }
            | AdminArtifactRef::Dylib { digest, uri, .. } => Self::materialize(uri)
                .map(|path| (path, digest.as_str()))
                .map_err(|e| (v2_err::ARTIFACT_FETCH, e)),
            _ => Err((
                v2_err::DIALECT_MISMATCH,
                "unsupported artifact dialect".into(),
            )),
        }
    }
    fn materialize(uri: &str) -> Result<PathBuf, String> {
        if uri.starts_with('/') {
            return Ok(PathBuf::from(uri));
        }
        let url = url::Url::parse(uri).map_err(|e| format!("unsupported artifact URI: {e}"))?;
        if !uri.starts_with("file:///")
            || url.scheme() != "file"
            || url.host_str().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("unsupported artifact URI: expected absolute local file:/// without authority, query or fragment".into());
        }
        url.to_file_path()
            .map_err(|_| "unsupported file URI".into())
    }
    /// Verify a bounded regular file. Empty/malformed digests are errors.
    pub fn verify(path: &Path, digest: &str) -> Result<(), String> {
        Self::read_verified(path, digest)
            .map(|_| ())
            .map_err(|(_, detail)| detail)
    }
    fn read_verified(path: &Path, digest: &str) -> Result<Vec<u8>, (u16, String)> {
        let fetch = |e: std::io::Error| (v2_err::ARTIFACT_FETCH, e.to_string());
        let mut open = std::fs::OpenOptions::new();
        open.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.custom_flags(libc::O_NONBLOCK);
        }
        let file = open.open(path).map_err(fetch)?;
        let metadata = file.metadata().map_err(fetch)?;
        if !metadata.is_file() {
            return Err((
                v2_err::ARTIFACT_FETCH,
                "expected a regular artifact file".into(),
            ));
        }
        if metadata.len() > Self::MAX_INPUT_BYTES as u64 {
            return Err((
                v2_err::ARTIFACT_FETCH,
                "artifact exceeds input byte limit".into(),
            ));
        }
        let expected = digest.strip_prefix("sha256:").unwrap_or(digest);
        if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err((
                v2_err::ARTIFACT_DIGEST,
                "mandatory SHA-256 requires 64 hex digits".into(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(Self::MAX_INPUT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(fetch)?;
        if bytes.len() > Self::MAX_INPUT_BYTES {
            return Err((
                v2_err::ARTIFACT_FETCH,
                "artifact exceeds input byte limit".into(),
            ));
        }
        if hex::encode(Sha256::digest(&bytes)) != expected.to_ascii_lowercase() {
            return Err((v2_err::ARTIFACT_DIGEST, "SHA-256 digest mismatch".into()));
        }
        Ok(bytes)
    }
}
