use parrot_node::configuration::{wasm_messaging_policy_request, wasm_service_policy_request};

#[test]
fn operator_policy_path_is_explicit_and_preserves_native_paths() {
    for select in [wasm_messaging_policy_request, wasm_service_policy_request] {
        assert!(select(None).unwrap().is_none());
        assert!(select(Some("".into())).is_err());
        let request = select(Some("operator policy.toml".into()));
        if cfg!(feature = "wasm") {
            assert_eq!(
                request.unwrap().unwrap(),
                std::path::PathBuf::from("operator policy.toml")
            );
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                let path = std::ffi::OsString::from_vec(vec![b'p', 255]);
                assert_eq!(
                    select(Some(path.clone())).unwrap().unwrap(),
                    std::path::PathBuf::from(path)
                );
            }
        } else {
            assert!(request
                .unwrap_err()
                .to_string()
                .contains("requires the wasm feature"));
        }
    }
}

#[tokio::test]
async fn stock_node_rejects_bad_or_unavailable_policy_before_readiness() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bad.toml");
    std::fs::write(&path, "grants = []\nunknown = true").unwrap();
    let ready = temp.path().join("ready");
    for variable in ["PARROT_WASM_MESSAGING_POLICY", "PARROT_WASM_SERVICE_POLICY"] {
        for value in [
            path.clone().into_os_string(),
            "".into(),
            temp.path().join("missing").into_os_string(),
        ] {
            let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-node"));
            for (key, _) in
                std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("PARROT_"))
            {
                command.env_remove(key);
            }
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                command
                    .current_dir(temp.path())
                    .env(variable, value)
                    .env("PARROT_READY_FILE", &ready)
                    .env("PARROT_BIND", "127.0.0.1:0")
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(!output.status.success());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error
                    .to_ascii_lowercase()
                    .contains(if cfg!(feature = "wasm") {
                        "policy"
                    } else {
                        "requires the wasm feature"
                    }),
                "{error}"
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("PARROT_NODE_READY="));
            assert!(!ready.exists());
        }
    }
}

#[cfg(feature = "wasm")]
#[path = "wasm_policy/enabled.rs"]
mod enabled;
