use parrot_node::configuration::{native_messaging_policy_request, native_service_policy_request};

#[test]
fn service_selector_is_explicit_and_feature_checked() {
    assert!(native_service_policy_request(None).unwrap().is_none());
    assert!(native_service_policy_request(Some("".into())).is_err());
    let result = native_service_policy_request(Some("operator services.toml".into()));
    if cfg!(feature = "dylib") {
        assert_eq!(
            result.unwrap().unwrap(),
            std::path::PathBuf::from("operator services.toml")
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let path = std::ffi::OsString::from_vec(vec![b's', 255]);
            assert_eq!(
                native_service_policy_request(Some(path.clone()))
                    .unwrap()
                    .unwrap(),
                std::path::PathBuf::from(path)
            );
        }
    } else {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requires the dylib feature"));
    }
}

#[tokio::test]
async fn invalid_service_policy_fails_node_before_readiness() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bad.toml");
    std::fs::write(&path, "grants=[]\nunknown=true").unwrap();
    for value in [
        path.into_os_string(),
        "".into(),
        temp.path().join("missing").into_os_string(),
    ] {
        let ready = temp.path().join("ready");
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-node"));
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("PARROT_"))
        {
            command.env_remove(key);
        }
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            command
                .current_dir(temp.path())
                .env("PARROT_NATIVE_SERVICE_POLICY", value)
                .env("PARROT_READY_FILE", &ready)
                .env("PARROT_BIND", "127.0.0.1:0")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr).to_ascii_lowercase();
        assert!(
            error.contains(if cfg!(feature = "dylib") {
                "policy"
            } else {
                "requires the dylib feature"
            }),
            "{error}"
        );
        assert!(!ready.exists());
        assert!(!String::from_utf8_lossy(&result.stdout).contains("PARROT_NODE_READY="));
    }
}

#[test]
fn policy_selector_is_explicit_feature_checked_and_preserves_native_paths() {
    assert!(native_messaging_policy_request(None).unwrap().is_none());
    assert!(native_messaging_policy_request(Some("".into())).is_err());
    let request = native_messaging_policy_request(Some("operator policy.toml".into()));
    if cfg!(feature = "dylib") {
        assert_eq!(
            request.unwrap().unwrap(),
            std::path::PathBuf::from("operator policy.toml")
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let path = std::ffi::OsString::from_vec(vec![b'p', 255]);
            assert_eq!(
                native_messaging_policy_request(Some(path.clone()))
                    .unwrap()
                    .unwrap(),
                std::path::PathBuf::from(path)
            );
        }
    } else {
        assert!(request
            .unwrap_err()
            .to_string()
            .contains("requires the dylib feature"));
    }
}

#[tokio::test]
async fn invalid_or_unavailable_node_policy_fails_before_readiness() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bad.toml");
    std::fs::write(&path, "grants=[]\nunknown=true").unwrap();
    let ready = temp.path().join("ready");
    for value in [
        path.into_os_string(),
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
                .env("PARROT_NATIVE_MESSAGING_POLICY", value)
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
                .contains(if cfg!(feature = "dylib") {
                    "policy"
                } else {
                    "requires the dylib feature"
                }),
            "{error}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("PARROT_NODE_READY="));
        assert!(!ready.exists());
    }
}

#[cfg(all(feature = "dylib", unix))]
#[path = "native_policy/enabled.rs"]
mod enabled;
#[cfg(all(feature = "dylib", unix))]
#[path = "../../parrot-app/tests/support/messaging.rs"]
mod messaging;
