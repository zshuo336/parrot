use parrot_node::configuration::{native_digest_requests, MAX_NATIVE_TRUST_DIGESTS};

#[test]
fn native_operator_metadata_is_bounded_and_strict() {
    assert!(native_digest_requests(None).unwrap().is_empty());
    assert!(native_digest_requests(Some(" \t ".into()))
        .unwrap()
        .is_empty());
    for value in [
        "bad".to_owned(),
        "g".repeat(64),
        format!("{},", "0".repeat(64)),
        format!(",{}", "0".repeat(64)),
        "0".repeat(MAX_NATIVE_TRUST_DIGESTS * 72 + 1),
        vec!["0".repeat(64); MAX_NATIVE_TRUST_DIGESTS + 1].join(","),
    ] {
        assert!(native_digest_requests(Some(value.into())).is_err());
    }
    let input = format!("sha256:{}, {}", "A".repeat(64), "a".repeat(64));
    let result = native_digest_requests(Some(input.into()));
    if cfg!(feature = "dylib") {
        assert_eq!(result.unwrap(), vec!["a".repeat(64)]);
    } else {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requires the dylib feature"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        assert!(native_digest_requests(Some(std::ffi::OsString::from_vec(vec![255]))).is_err());
    }
}

#[tokio::test]
async fn actual_node_rejects_invalid_or_unavailable_native_authorization_before_readiness() {
    let mut values = vec!["not-a-digest".to_owned(), format!("{},", "0".repeat(64))];
    if !cfg!(feature = "dylib") {
        values.push("0".repeat(64));
    }
    for value in values {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-node"));
        for (key, _) in
            std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("PARROT_"))
        {
            command.env_remove(key);
        }
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            command
                .current_dir(temp.path())
                .env("PARROT_TRUST_NATIVE_SHA256", value)
                .env("PARROT_BIND", "127.0.0.1:0")
                .env("PARROT_READY_FILE", &ready)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("PARROT_TRUST_NATIVE_SHA256"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("PARROT_NODE_READY="));
        assert!(!ready.exists());
    }
}

#[cfg(not(feature = "dylib"))]
#[tokio::test]
async fn default_node_executor_rejects_native_even_if_app_native_feature_is_unified() {
    use parrot_remote::admin_v2::*;
    let engine = parrot::thread::system::ThreadActorSystem::shared(Default::default());
    let executor = parrot_node::NodeComponentExecutor::new(engine.clone());
    let reply = executor
        .deploy(
            1,
            &ComponentDeploy {
                name: "disabled".into(),
                version: "1.0.0".into(),
                artifact: AdminArtifactRef::Dylib {
                    digest: "0".repeat(64),
                    uri: "file:///missing".into(),
                    abi: 1,
                },
                instances: AdminInstancePolicy::Singleton,
                config: None,
            },
        )
        .await;
    assert!(
        matches!(
            reply,
            AdminReplyV2::Failed {
                code: v2_err::DIALECT_MISMATCH,
                ..
            }
        ),
        "{reply:?}"
    );
    assert!(engine.get_actor_ref("/user/disabled").is_none());
    engine.shutdown_internal().await.unwrap();
}
