//! Durable application namespace and fresh per-invocation component addresses.
//! The caller must hold the data directory's .crawl.lock throughout use.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    namespace: String,
    roles: BTreeMap<String, String>,
    #[serde(default)]
    index_data: Option<String>,
    checksum: [u8; 32],
}

pub(crate) struct Identity {
    namespace: String,
    run: String,
    directory: std::path::PathBuf,
    roles: BTreeMap<String, String>,
    index_data: Option<String>,
}

fn checksum(namespace: &str, roles: &BTreeMap<String, String>) -> [u8; 32] {
    Sha256::digest(serde_json::to_vec(&(1u32, namespace, roles)).expect("string maps serialize"))
        .into()
}

fn checksum_v2(
    namespace: &str,
    roles: &BTreeMap<String, String>,
    index_data: &Option<String>,
) -> [u8; 32] {
    Sha256::digest(
        serde_json::to_vec(&(2u32, namespace, roles, index_data)).expect("string maps serialize"),
    )
    .into()
}

fn random_hex() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("component identity entropy: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl Identity {
    pub fn open(directory: &Path) -> Result<Self, String> {
        let path = directory.join("remote.identity");
        let run = random_hex()?;
        let namespace = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.len() != 65 {
                    return Err(
                        "invalid remote identity file; preserve it and restore its matching backup"
                            .into(),
                    );
                }
                let mut bytes = Vec::new();
                File::open(&path)
                    .map_err(|e| e.to_string())?
                    .take(66)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                if bytes.len() != 65
                    || bytes[64] != b'\n'
                    || !bytes[..64]
                        .iter()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
                {
                    return Err("invalid remote identity encoding".into());
                }
                String::from_utf8(bytes[..64].to_vec()).unwrap()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(directory.join("remote.components")).is_ok() {
                    return Err("remote identity missing while component intents exist; restore matching identity".into());
                }
                let namespace = random_hex()?;
                let temporary = directory.join(format!(".remote-identity-{run}.tmp"));
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                    .map_err(|e| e.to_string())?;
                writeln!(output, "{namespace}")
                    .and_then(|_| output.sync_all())
                    .map_err(|e| format!("persist remote identity: {e}"))?;
                // Atomic publication without overwriting an unexpected owner.
                std::fs::hard_link(&temporary, &path)
                    .map_err(|e| format!("publish remote identity: {e}"))?;
                File::open(directory)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| format!("sync remote identity directory: {e}"))?;
                std::fs::remove_file(&temporary).map_err(|e| e.to_string())?;
                namespace
            }
            Err(error) => return Err(format!("read remote identity: {error}")),
        };
        let manifest = directory.join("remote.components");
        let (roles, index_data) = match std::fs::symlink_metadata(&manifest) {
            Ok(meta) => {
                if !meta.is_file() || meta.len() > 32768 {
                    return Err("invalid remote component intents file".into());
                }
                let mut bytes = Vec::new();
                File::open(&manifest)
                    .map_err(|e| e.to_string())?
                    .take(32769)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                let value: Manifest = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("invalid remote component intents: {e}"))?;
                let expected = match value.version {
                    1 if value.index_data.is_none() => checksum(&value.namespace, &value.roles),
                    2 => checksum_v2(&value.namespace, &value.roles, &value.index_data),
                    _ => return Err("unsupported remote component intent version".into()),
                };
                if value.checksum != expected
                    || value
                        .index_data
                        .as_ref()
                        .is_some_and(|p| super::cli::validate_index_data(p).is_err())
                    || value.namespace != namespace
                    || value.roles.len() > 3
                    || value.roles.iter().any(|(role, run)| {
                        !matches!(role.as_str(), "frontier" | "tokenizer" | "search")
                            || run.len() != 64
                            || !run
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                {
                    return Err("invalid remote component identity or role".into());
                }
                (value.roles, value.index_data)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (BTreeMap::new(), None),
            Err(e) => return Err(e.to_string()),
        };
        Ok(Self {
            namespace,
            run,
            directory: directory.to_owned(),
            roles,
            index_data,
        })
    }

    pub fn prefix(&self, role: &str) -> String {
        format!("ws-{}-{role}", self.namespace)
    }
    pub fn component(&self, role: &str) -> String {
        format!("{}-{}", self.prefix(role), self.run)
    }

    pub fn previous(&self, role: &str) -> String {
        format!(
            "{}-{}",
            self.prefix(role),
            self.roles.get(role).unwrap_or(&self.run)
        )
    }

    /// After the previous address is confirmed retired, persist the new intent
    /// before sending Deploy. A lost reply then still has an exact cleanup path.
    pub fn deploying(&mut self, role: &str) -> Result<(), String> {
        if !matches!(role, "frontier" | "tokenizer" | "search") {
            return Err("invalid component role".into());
        }
        let mut roles = self.roles.clone();
        roles.insert(role.to_owned(), self.run.clone());
        self.persist(&roles, &self.index_data)?;
        self.roles = roles;
        Ok(())
    }

    /// An omitted option reuses the durable selection. Keep/serve-only cannot
    /// redirect a previously bound journal to a different index by accident.
    pub fn select_index(
        &mut self,
        requested: Option<String>,
        preserve: bool,
    ) -> Result<Option<String>, String> {
        if let Some(path) = requested {
            super::cli::validate_index_data(&path)?;
            if self.index_data.as_ref() != Some(&path) {
                if preserve && self.index_data.is_some() {
                    return Err("requested --index-data differs from persisted remote index; restore the matching selection or start an explicit fresh crawl".into());
                }
                let selected = Some(path);
                self.persist(&self.roles, &selected)?;
                self.index_data = selected;
            }
        }
        Ok(self.index_data.clone())
    }

    fn persist(
        &self,
        roles: &BTreeMap<String, String>,
        index_data: &Option<String>,
    ) -> Result<(), String> {
        let bytes = serde_json::to_vec(&Manifest {
            version: 2,
            namespace: self.namespace.clone(),
            roles: roles.clone(),
            index_data: index_data.clone(),
            checksum: checksum_v2(&self.namespace, roles, index_data),
        })
        .map_err(|e| e.to_string())?;
        let temporary = self
            .directory
            .join(format!(".remote-components-{}.tmp", random_hex()?));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("persist component intent: {e}"))?;
        std::fs::rename(&temporary, self.directory.join("remote.components"))
            .map_err(|e| e.to_string())?;
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("sync component intent directory: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_selection_survives_restart_and_cannot_redirect_a_preserved_journal() {
        let d = Directory::new();
        let path =
            d.0.join("remote \" quote \\ 中文\n")
                .to_str()
                .unwrap()
                .to_owned();
        let mut first = Identity::open(&d.0).unwrap();
        first.select_index(Some(path.clone()), false).unwrap();
        first.deploying("search").unwrap();
        let bytes = std::fs::read(d.0.join("remote.components")).unwrap();
        let mut next = Identity::open(&d.0).unwrap();
        assert_eq!(next.select_index(None, true).unwrap(), Some(path.clone()));
        assert_eq!(next.previous("search"), first.component("search"));
        assert!(next
            .select_index(Some("/different/index".into()), true)
            .is_err());
        assert_eq!(std::fs::read(d.0.join("remote.components")).unwrap(), bytes);
        assert_eq!(
            next.select_index(Some(path.clone()), true).unwrap(),
            Some(path)
        );
        next.select_index(Some("/different/index".into()), false)
            .unwrap();
        assert_eq!(
            Identity::open(&d.0)
                .unwrap()
                .select_index(None, true)
                .unwrap(),
            Some("/different/index".into())
        );
    }

    #[test]
    fn old_intents_upgrade_without_losing_any_role_or_assuming_an_index() {
        let d = Directory::new();
        let first = Identity::open(&d.0).unwrap();
        let roles: BTreeMap<_, _> = ["frontier", "tokenizer", "search"]
            .into_iter()
            .map(|role| (role.to_owned(), "f".repeat(64)))
            .collect();
        let old = serde_json::json!({"version":1,"namespace":first.namespace,"roles":roles,
                                    "checksum":checksum(&first.namespace,&roles)});
        std::fs::write(
            d.0.join("remote.components"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        let mut next = Identity::open(&d.0).unwrap();
        assert_eq!(next.select_index(None, true).unwrap(), None);
        next.select_index(Some("/explicit/migration".into()), true)
            .unwrap();
        let upgraded = Identity::open(&d.0).unwrap();
        assert_eq!(upgraded.roles, roles);
        assert_eq!(upgraded.index_data.as_deref(), Some("/explicit/migration"));
    }

    #[test]
    fn index_tampering_and_failed_publication_preserve_prior_selection() {
        let d = Directory::new();
        let mut first = Identity::open(&d.0).unwrap();
        first.select_index(Some("/original".into()), false).unwrap();
        let path = d.0.join("remote.components");
        let bytes = std::fs::read(&path).unwrap();
        let mut manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
        manifest.index_data = Some("/foreign".into());
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(Identity::open(&d.0).is_err());
        manifest.index_data = Some("relative".into());
        manifest.checksum = checksum_v2(&manifest.namespace, &manifest.roles, &manifest.index_data);
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(Identity::open(&d.0).is_err());
        std::fs::rename(&path, d.0.join("invalid.saved")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(first.select_index(Some("/other".into()), false).is_err());
        assert_eq!(
            first.select_index(None, true).unwrap(),
            Some("/original".into())
        );
    }
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("ws-owner-{}", random_hex().unwrap()));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn identity_survives_restart_but_paths_change_each_run() {
        let d = Directory::new();
        let a = Identity::open(&d.0).unwrap();
        let b = Identity::open(&d.0).unwrap();
        assert_eq!(a.prefix("search"), b.prefix("search"));
        assert_ne!(a.component("search"), b.component("search"));
        for role in ["search", "frontier", "tokenizer"] {
            assert!(a.component(role).starts_with(&(a.prefix(role) + "-")));
            assert!(a.component(role).len() < 256);
        }
        let other = Directory::new();
        assert_ne!(
            a.prefix("search"),
            Identity::open(&other.0).unwrap().prefix("search")
        );
    }
    #[test]
    fn invalid_identity_never_generates_a_new_namespace_or_modifies_evidence() {
        let d = Directory::new();
        for bytes in [
            vec![],
            vec![b'a'; 64],
            [vec![b'a'; 64], vec![b'\n', b'x']].concat(),
            [vec![b'G'; 64], vec![b'\n']].concat(),
        ] {
            std::fs::write(d.0.join("remote.identity"), &bytes).unwrap();
            assert!(Identity::open(&d.0).is_err());
            assert_eq!(std::fs::read(d.0.join("remote.identity")).unwrap(), bytes);
        }
    }
    #[test]
    fn torn_unpublished_identity_is_not_used_and_remains_available_for_inspection() {
        let d = Directory::new();
        std::fs::write(d.0.join(".remote-identity-old.tmp"), b"torn").unwrap();
        let a = Identity::open(&d.0).unwrap();
        assert_eq!(
            a.prefix("search"),
            Identity::open(&d.0).unwrap().prefix("search")
        );
        assert_eq!(
            std::fs::read(d.0.join(".remote-identity-old.tmp")).unwrap(),
            b"torn"
        );
    }
    #[cfg(unix)]
    #[test]
    fn symlink_identity_is_rejected() {
        let d = Directory::new();
        std::fs::write(d.0.join("other"), [vec![b'a'; 64], vec![b'\n']].concat()).unwrap();
        std::os::unix::fs::symlink(d.0.join("other"), d.0.join("remote.identity")).unwrap();
        assert!(Identity::open(&d.0).is_err());
    }
    #[test]
    fn deployment_intents_survive_missing_ack_and_serve_only_preserves_other_roles() {
        let d = Directory::new();
        let mut first = Identity::open(&d.0).unwrap();
        for role in ["frontier", "tokenizer", "search"] {
            first.deploying(role).unwrap();
        }
        let mut reader = Identity::open(&d.0).unwrap();
        for role in ["frontier", "tokenizer", "search"] {
            assert_eq!(reader.previous(role), first.component(role));
        }
        reader.deploying("search").unwrap();
        let next = Identity::open(&d.0).unwrap();
        assert_eq!(next.previous("search"), reader.component("search"));
        for role in ["frontier", "tokenizer"] {
            assert_eq!(next.previous(role), first.component(role));
        }
        assert_ne!(next.component("search"), next.previous("search"));
    }
    #[test]
    fn corrupt_or_foreign_intents_cannot_authorize_remote_cleanup() {
        let d = Directory::new();
        let mut id = Identity::open(&d.0).unwrap();
        id.deploying("search").unwrap();
        let path = d.0.join("remote.components");
        let original = std::fs::read(&path).unwrap();
        for n in 0..original.len() {
            std::fs::write(&path, &original[..n]).unwrap();
            assert!(Identity::open(&d.0).is_err(), "prefix {n}");
        }
        let mut changed: Manifest = serde_json::from_slice(&original).unwrap();
        changed.roles.insert("search".into(), "f".repeat(64));
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(Identity::open(&d.0).is_err());
        let other = Directory::new();
        Identity::open(&other.0).unwrap();
        std::fs::write(other.0.join("remote.components"), &original).unwrap();
        assert!(Identity::open(&other.0).is_err());
        std::fs::write(&path, &original).unwrap();
        std::fs::rename(d.0.join("remote.identity"), d.0.join("identity.saved")).unwrap();
        assert!(Identity::open(&d.0).is_err());
        assert!(!d.0.join("remote.identity").exists());
    }
    #[test]
    fn failed_intent_publication_keeps_previous_role_addresses() {
        let d = Directory::new();
        let mut first = Identity::open(&d.0).unwrap();
        first.deploying("search").unwrap();
        let mut next = Identity::open(&d.0).unwrap();
        std::fs::rename(d.0.join("remote.components"), d.0.join("intents.saved")).unwrap();
        std::fs::create_dir(d.0.join("remote.components")).unwrap();
        assert!(next.deploying("search").is_err());
        assert_eq!(next.previous("search"), first.component("search"));
    }
}
