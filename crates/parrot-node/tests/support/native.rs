#![allow(dead_code)] // Shared by process and embedded-runtime contract binaries.
use parrot_remote::admin_v2::{AdminArtifactRef, AdminInstancePolicy, ComponentDeploy};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::OnceLock, time::Duration};

pub fn fixture() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../parrot-abi/tests/fixtures");
        let output = std::process::Command::new("cargo")
            .args(["build", "--locked", "--release", "--target-dir"])
            .arg(root.join("target"))
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture build: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        root.join("target/release")
            .join(format!(
                "libparrot_abi_hostcomp.{}",
                std::env::consts::DLL_EXTENSION
            ))
            .canonicalize()
            .unwrap()
    })
}
pub struct NativeFixture {
    pub temp: tempfile::TempDir,
    pub path: PathBuf,
    pub digest: String,
    pub config: toml::Table,
}
impl NativeFixture {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .join(format!("native 中文.{}", std::env::consts::DLL_EXTENSION));
        std::fs::copy(fixture(), &path).unwrap();
        let digest = format!("sha256:{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
        let mut config: toml::Table =
            toml::from_str("label='远端配置'\nitems=[1,2]\n[nested]\nlimit=7").unwrap();
        config.insert(
            "trace".into(),
            toml::Value::String(temp.path().join("events").to_str().unwrap().into()),
        );
        Self {
            temp,
            path,
            digest,
            config,
        }
    }
    pub fn trust(&self) -> parrot_node::NativeTrust {
        // SAFETY: this test builds/owns the fixture, whose hooks obey the ABI
        // contract, and trusts its fixed build dependencies.
        unsafe { parrot_node::NativeTrust::authorize(std::slice::from_ref(&self.digest)) }.unwrap()
    }
    pub fn component(&self, name: &str, count: usize) -> ComponentDeploy {
        ComponentDeploy {
            name: name.into(),
            version: "2.3.4".into(),
            artifact: AdminArtifactRef::Dylib {
                digest: self.digest.clone(),
                uri: url::Url::from_file_path(&self.path).unwrap().into(),
                abi: 1,
            },
            instances: AdminInstancePolicy::Pool { count },
            config: Some(toml::to_string(&self.config).unwrap().into_bytes()),
        }
    }
    pub fn gate(&mut self, key: &str) -> PathBuf {
        let path = self.temp.path().join(key);
        self.config.insert(
            key.into(),
            toml::Value::String(path.to_str().unwrap().into()),
        );
        path
    }
    pub fn events(&self) -> String {
        std::fs::read_to_string(self.temp.path().join("events")).unwrap_or_default()
    }
    pub async fn wait_for(&self, event: &str) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !self.events().lines().any(|line| line == event) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("missing event {event}: {}", self.events()));
    }
}
