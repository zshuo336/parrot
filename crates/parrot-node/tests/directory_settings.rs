use parrot_node::configuration::NodeSettings;
use std::{collections::HashMap, ffi::OsString, path::PathBuf};
struct File(PathBuf);
impl File {
    fn new(text: &str) -> Self {
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "parrot-directory-settings-{}-{}.toml",
            std::process::id(),
            SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&p, text).unwrap();
        Self(p)
    }
    fn settings(
        &self,
        overrides: &[(&str, &str)],
    ) -> Result<NodeSettings, parrot_config::ConfigError> {
        let mut env: HashMap<String, OsString> = overrides
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        env.insert("PARROT_CONFIG".into(), self.0.clone().into_os_string());
        NodeSettings::from_environment(|key| env.get(key).cloned())
    }
}
impl Drop for File {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).unwrap();
    }
}
const CONFIG:&str="[remote.node]\nnode_id='node-a'\nscheme='mem'\ntopology_role='directory'\n[remote.directory]\nstorage_dir='/private/old'\ngroup='old'\npeers=['mem://node-b']\nelection_ms=1500\n";
#[test]
fn environment_overrides_directory_fields_without_inventing_voters() {
    let f = File::new(CONFIG);
    let settings = f
        .settings(&[
            ("PARROT_DIRECTORY_PATH", "/private/new"),
            ("PARROT_DIRECTORY_GROUP", "new"),
            ("PARROT_DIRECTORY_PEERS", ""),
            ("PARROT_DIRECTORY_HEARTBEAT_MS", "90"),
        ])
        .unwrap();
    let d = settings.resolved.remote.directory.unwrap();
    assert_eq!(d.storage_dir, "/private/new");
    assert_eq!(d.group, "new");
    assert!(d.peers.is_empty());
    assert_eq!(d.election_ms, 1500);
    assert_eq!(d.heartbeat_ms, 90);
}
#[test]
fn standard_node_rejects_role_service_mismatch_and_invalid_overrides() {
    let f = File::new(CONFIG);
    for overrides in [
        vec![("PARROT_TOPOLOGY_ROLE", "normal")],
        vec![("PARROT_DIRECTORY_GROUP", "")],
        vec![("PARROT_DIRECTORY_ELECTION_MS", "bad")],
        vec![("PARROT_DIRECTORY_PEERS", "mem://node-a")],
        vec![("PARROT_DIRECTORY_RPC_TIMEOUT_MS", "0")],
    ] {
        assert!(f.settings(&overrides).is_err());
    }
    let f = File::new("[remote.node]\ntopology_role='directory'\n");
    assert!(f.settings(&[]).is_err());
    let f = File::new("");
    assert!(f
        .settings(&[("PARROT_TOPOLOGY_ROLE", "directory")])
        .is_err());
    assert!(f.settings(&[("PARROT_DIRECTORY_GROUP", "g")]).is_err());
}

#[test]
fn persistent_members_do_not_inherit_ordinary_node_identity_or_listener_defaults() {
    let without_identity = File::new(&CONFIG.replace("node_id='node-a'\n", ""));
    assert!(without_identity.settings(&[]).is_err());
    assert!(without_identity
        .settings(&[("PARROT_NODE_ID", "chosen-node")])
        .is_ok());
    let tcp = File::new(
        "[remote.node]\nnode_id='a.test'\nscheme='tcp'\ntopology_role='directory'\n\
         [remote.security]\nmode='mtls'\ncert_path='a.pem'\nkey_path='a.key'\nca_path='ca.pem'\n\
         [remote.directory]\nstorage_dir='/private/a'\ngroup='g'\npeers=[]\n",
    );
    assert!(tcp.settings(&[]).is_err());
    assert!(tcp.settings(&[("PARROT_BIND", "127.0.0.1:9801")]).is_ok());
    let ordinary = File::new("");
    let settings = ordinary.settings(&[]).unwrap();
    assert_eq!(
        settings.resolved.remote.node.node_id.as_deref(),
        Some("node-1")
    );
    assert_eq!(
        settings.resolved.remote.node.bind.as_deref(),
        Some("127.0.0.1:9801")
    );
}

#[test]
fn wasm_epoch_configuration_is_explicit_bounded_and_feature_checked() {
    let f = File::new("");
    assert_eq!(f.settings(&[]).unwrap().wasm_epoch_ticks, None);
    for invalid in ["0", "4294967296", "-1", "bad"] {
        let error = f
            .settings(&[("PARROT_WASM_EPOCH_TICKS", invalid)])
            .unwrap_err();
        assert!(error.to_string().contains("PARROT_WASM_EPOCH_TICKS"));
    }
    for valid in ["1", "1000", "4294967295"] {
        let result = f.settings(&[("PARROT_WASM_EPOCH_TICKS", valid)]);
        if cfg!(feature = "wasm") {
            let mut settings = result.unwrap();
            assert_eq!(settings.wasm_epoch_ticks, Some(valid.parse().unwrap()));
            settings.wasm_epoch_ticks = Some(0);
            assert!(settings.validate().is_err());
        } else {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("requires the wasm feature"));
        }
    }
}
