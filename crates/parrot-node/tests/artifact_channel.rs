use parrot_node::ArtifactChannel;
use parrot_remote::admin_v2::{v2_err, AdminArtifactRef};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-artifact-channel-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn artifact(&self, path: &std::path::Path, bytes: &[u8]) -> AdminArtifactRef {
        AdminArtifactRef::Wasm {
            uri: url::Url::from_file_path(path).unwrap().into(),
            digest: format!("sha256:{:x}", Sha256::digest(bytes)),
        }
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn verified_bytes_survive_path_replacement_and_percent_encoded_paths() {
    let temp = Temp::new();
    let path = temp.0.join("with space.wasm");
    std::fs::write(&path, b"verified input").unwrap();
    let artifact = temp.artifact(&path, b"verified input");
    let bytes = ArtifactChannel.fetch_bytes(&artifact).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    assert_eq!(bytes, b"verified input");
    assert_eq!(
        ArtifactChannel.fetch_bytes(&artifact).unwrap_err().0,
        v2_err::ARTIFACT_DIGEST
    );
}
#[test]
fn artifact_limits_and_special_file_types_fail_before_reading_contents() {
    let temp = Temp::new();
    let path = temp.0.join("oversized.wasm");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(ArtifactChannel::MAX_INPUT_BYTES as u64 + 1)
        .unwrap();
    let error = ArtifactChannel
        .fetch_bytes(&temp.artifact(&path, b""))
        .unwrap_err();
    assert_eq!(error.0, v2_err::ARTIFACT_FETCH);
    assert!(error.1.contains("byte limit"));
    let error = ArtifactChannel
        .fetch_bytes(&temp.artifact(&temp.0, b""))
        .unwrap_err();
    assert!(error.1.contains("regular"));
}
#[test]
fn mandatory_digest_and_uri_authority_are_not_test_bypasses() {
    let temp = Temp::new();
    let path = temp.0.join("input.wasm");
    std::fs::write(&path, b"input").unwrap();
    for digest in ["", "sha256:", "ab", "sha512:abcd"] {
        assert!(ArtifactChannel::verify(&path, digest).is_err());
    }
    for uri in [
        format!("file://untrusted-host{}", path.display()),
        format!("file://{}?query", path.display()),
        format!("file://{}#fragment", path.display()),
    ] {
        let artifact = AdminArtifactRef::Wasm {
            uri,
            digest: format!("{:x}", Sha256::digest(b"input")),
        };
        assert_eq!(
            ArtifactChannel.fetch_bytes(&artifact).unwrap_err().0,
            v2_err::ARTIFACT_FETCH
        );
    }
}
#[cfg(unix)]
#[test]
fn fifo_does_not_wait_for_a_writer() {
    if let Some(path) = std::env::var_os("PARROT_ARTIFACT_FIFO_PROBE") {
        assert!(
            ArtifactChannel::verify(std::path::Path::new(&path), &"0".repeat(64))
                .unwrap_err()
                .contains("regular")
        );
        return;
    }
    let temp = Temp::new();
    let path = temp.0.join("fifo");
    assert!(std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap()
        .success());
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fifo_does_not_wait_for_a_writer"])
        .env("PARROT_ARTIFACT_FIFO_PROBE", path)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("artifact read blocked on FIFO");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
