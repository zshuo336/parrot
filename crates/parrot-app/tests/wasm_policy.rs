#![cfg(feature = "host")]

#[cfg(all(unix, feature = "wasm"))]
#[path = "support/native_path.rs"]
mod native_path;
#[cfg(all(unix, feature = "wasm"))]
#[path = "support/policy_fifo.rs"]
mod policy_fifo;

#[tokio::test]
async fn invalid_operator_policy_never_creates_recording_or_readiness() {
    let temp = tempfile::tempdir().unwrap();
    let policy = temp.path().join("policy.toml");
    std::fs::write(&policy, "grants = []\nmisspelled = true\n").unwrap();
    let recording = temp.path().join("capture");
    let ready = temp.path().join("ready");
    for flag in ["--wasm-messaging-policy", "--wasm-service-policy"] {
        for path in [
            policy.clone(),
            temp.path().join("missing"),
            std::path::PathBuf::new(),
        ] {
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"))
                    .current_dir(temp.path())
                    .env("PARROT_APP_READY_FILE", &ready)
                    .args(["run", "--manifest", "missing.app.toml", flag])
                    .arg(path)
                    .arg("--record")
                    .arg(&recording)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(if cfg!(feature = "wasm") {
                    "policy"
                } else {
                    "requires the wasm feature"
                })
            );
            assert!(!recording.exists());
            assert!(!ready.exists());
        }
    }
}

#[cfg(feature = "wasm")]
mod enabled {
    use parrot_app::host::wasm::{WasmMessageGrant, WasmMessageOperation, WasmMessagingPolicy};

    fn grant() -> WasmMessageGrant {
        WasmMessageGrant {
            source_path: "/user/guest".into(),
            artifact_digest: "A".repeat(64),
            destination: "/user/native".into(),
            request_type_key: "bin:policy.Request#v1".into(),
            operation: WasmMessageOperation::Ask {
                reply_type_key: "bin:policy.Reply#v1".into(),
            },
        }
    }
    fn document(grants: &[WasmMessageGrant]) -> String {
        #[derive(serde::Serialize)]
        struct Document<'a> {
            grants: &'a [WasmMessageGrant],
        }
        toml::to_string(&Document { grants }).unwrap()
    }

    #[test]
    fn policy_is_strict_at_every_level_and_requires_explicit_grants() {
        let valid = document(&[grant()]);
        assert_eq!(
            WasmMessagingPolicy::from_toml(&valid).unwrap().grants()[0].artifact_digest,
            "a".repeat(64)
        );
        for invalid in [
            String::new(),
            "enabled = true".into(),
            "grants = []\nextra = true".into(),
            valid.replace("source_path =", "typo ="),
            valid.replace("source_path =", "extra = true\nsource_path ="),
            valid.replace("kind = \"ask\"", "kind = \"ask\"\nextra = true"),
            valid.replace("kind = \"ask\"", "kind = \"tell\""),
            valid.replace("kind = \"ask\"", "kind = \"unknown\""),
            valid.replace("reply_type_key =", "reply_typo ="),
        ] {
            assert!(
                WasmMessagingPolicy::from_toml(&invalid).is_err(),
                "accepted: {invalid}"
            );
        }
        assert_eq!(
            WasmMessagingPolicy::from_toml("grants = []").unwrap(),
            WasmMessagingPolicy::default()
        );
    }

    #[test]
    fn policies_compare_by_authority_and_reject_overlaps_and_size() {
        let first = grant();
        let second = WasmMessageGrant {
            operation: WasmMessageOperation::Tell,
            ..grant()
        };
        let a = WasmMessagingPolicy::new(vec![first.clone(), second.clone()]).unwrap();
        let b = WasmMessagingPolicy::new(vec![
            second,
            WasmMessageGrant {
                artifact_digest: format!("sha256:{}", "a".repeat(64)),
                ..first.clone()
            },
        ])
        .unwrap();
        assert_eq!(a, b);
        assert!(WasmMessagingPolicy::new(vec![first.clone(), first.clone()]).is_err());
        let grants: Vec<_> = (0..256)
            .map(|index| WasmMessageGrant {
                source_path: format!("/user/guest-{index}"),
                ..first.clone()
            })
            .collect();
        WasmMessagingPolicy::new(grants.clone()).unwrap();
        let mut over = grants;
        over.push(WasmMessageGrant {
            source_path: "/user/guest-256".into(),
            ..first
        });
        assert!(WasmMessagingPolicy::new(over).is_err());
        let mut exact = "grants = []\n#".to_string();
        exact.extend(std::iter::repeat_n('x', 1_048_576 - exact.len()));
        assert_eq!(
            WasmMessagingPolicy::from_toml(&exact).unwrap(),
            WasmMessagingPolicy::default()
        );
        exact.push('x');
        assert!(WasmMessagingPolicy::from_toml(&exact).is_err());
    }

    #[tokio::test]
    async fn file_load_is_bounded_validated_and_immutable_after_return() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("operator policy.toml");
        std::fs::write(&path, document(&[grant()])).unwrap();
        let policy = WasmMessagingPolicy::load_file(&path).await.unwrap();
        std::fs::write(&path, "grants = []").unwrap();
        assert_eq!(policy.grants().len(), 1);
        assert_eq!(
            WasmMessagingPolicy::load_file(&path).await.unwrap(),
            WasmMessagingPolicy::default()
        );
        let mut oversized = b"grants = []\n#".to_vec();
        oversized.resize(1_048_577, b'x');
        for bytes in [vec![255], oversized] {
            std::fs::write(&path, bytes).unwrap();
            assert!(WasmMessagingPolicy::load_file(&path).await.is_err());
        }
        assert!(WasmMessagingPolicy::load_file(temp.path()).await.is_err());
        assert!(WasmMessagingPolicy::load_file(temp.path().join("missing"))
            .await
            .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_paths_and_regular_symlinks_preserve_policy() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(super::native_path::file_name());
        std::fs::write(&path, "grants = []").unwrap();
        assert_eq!(
            WasmMessagingPolicy::load_file(&path).await.unwrap(),
            WasmMessagingPolicy::default()
        );
        let link = temp.path().join("link");
        symlink(&path, &link).unwrap();
        assert_eq!(
            WasmMessagingPolicy::load_file(link).await.unwrap(),
            WasmMessagingPolicy::default()
        );
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_rejected_without_waiting_for_a_writer() {
        super::policy_fifo::rejects_without_writer(
            "enabled::fifo_is_rejected_without_waiting_for_a_writer",
            WasmMessagingPolicy::load_file,
        );
    }
}
