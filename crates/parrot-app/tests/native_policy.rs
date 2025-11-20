#![cfg(feature = "host")]

#[cfg(all(unix, feature = "dylib"))]
#[path = "support/native_path.rs"]
mod native_path;
#[cfg(all(unix, feature = "dylib"))]
#[path = "support/policy_fifo.rs"]
mod policy_fifo;

#[tokio::test]
async fn invalid_native_policy_arguments_never_create_recording_or_readiness() {
    let temp = tempfile::tempdir().unwrap();
    let policy = temp.path().join("policy.toml");
    std::fs::write(&policy, "grants = []\nmisspelled = true\n").unwrap();
    let ready = temp.path().join("ready");
    let recording = temp.path().join("capture");
    let flag = std::ffi::OsString::from("--native-messaging-policy");
    for values in [
        vec![flag.clone(), policy.clone().into_os_string()],
        vec![flag.clone(), temp.path().join("missing").into_os_string()],
        vec![flag.clone(), "".into()],
        vec![flag.clone()],
        vec![
            flag.clone(),
            policy.clone().into_os_string(),
            flag,
            policy.into_os_string(),
        ],
    ] {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"))
                .current_dir(temp.path())
                .env("PARROT_APP_READY_FILE", &ready)
                .args(["run", "--manifest", "missing.toml", "--record"])
                .arg(&recording)
                .args(values)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if cfg!(feature = "dylib") {
                "policy"
            } else {
                "requires the dylib feature"
            }),
            "{error}"
        );
        assert!(!recording.exists());
        assert!(!ready.exists());
    }
}

#[cfg(feature = "dylib")]
mod enabled {
    use parrot_app::host::dylib::{
        NativeMessageGrant, NativeMessageOperation, NativeMessagingPolicy,
    };
    fn grant() -> NativeMessageGrant {
        NativeMessageGrant {
            source_path: "/user/guest".into(),
            artifact_digest: "A".repeat(64),
            destination: "/user/backend".into(),
            request_type_key: "bin:policy.Request#v1".into(),
            operation: NativeMessageOperation::Ask {
                reply_type_key: "bin:policy.Reply#v1".into(),
            },
        }
    }
    fn document(grants: &[NativeMessageGrant]) -> String {
        #[derive(serde::Serialize)]
        struct Document<'a> {
            grants: &'a [NativeMessageGrant],
        }
        toml::to_string(&Document { grants }).unwrap()
    }
    #[test]
    fn strict_policy_rejects_unknown_fields_and_requires_explicit_grants() {
        let valid = document(&[grant()]);
        assert_eq!(
            NativeMessagingPolicy::from_toml(&valid).unwrap().grants()[0].artifact_digest,
            "a".repeat(64)
        );
        for invalid in [
            String::new(),
            "enabled=true".into(),
            "grants=[]\nextra=true".into(),
            valid.replace("source_path =", "extra = true\nsource_path ="),
            valid.replace("source_path =", "typo ="),
            valid.replace("kind = \"ask\"", "kind = \"ask\"\nextra = true"),
            valid.replace("kind = \"ask\"", "kind = \"tell\""),
            valid.replace("kind = \"ask\"", "kind = \"unknown\""),
            valid.replace("reply_type_key =", "reply_typo ="),
        ] {
            assert!(
                NativeMessagingPolicy::from_toml(&invalid).is_err(),
                "accepted {invalid}"
            );
        }
        assert_eq!(
            NativeMessagingPolicy::from_toml("grants=[]").unwrap(),
            NativeMessagingPolicy::default()
        );
    }
    #[test]
    fn normalized_authority_order_is_stable_and_limits_are_exact() {
        let first = grant();
        let second = NativeMessageGrant {
            operation: NativeMessageOperation::Tell,
            ..grant()
        };
        let a = NativeMessagingPolicy::new(vec![first.clone(), second.clone()]).unwrap();
        let b = NativeMessagingPolicy::new(vec![
            second,
            NativeMessageGrant {
                artifact_digest: format!("sha256:{}", "a".repeat(64)),
                ..first.clone()
            },
        ])
        .unwrap();
        assert_eq!(a, b);
        assert!(NativeMessagingPolicy::new(vec![first.clone(), first.clone()]).is_err());
        let mut grants: Vec<_> = (0..256)
            .map(|i| NativeMessageGrant {
                source_path: format!("/user/guest-{i}"),
                ..first.clone()
            })
            .collect();
        NativeMessagingPolicy::new(grants.clone()).unwrap();
        grants.push(NativeMessageGrant {
            source_path: "/user/extra".into(),
            ..first
        });
        assert!(NativeMessagingPolicy::new(grants).is_err());
        let mut exact = "grants=[]\n#".to_string();
        exact.extend(std::iter::repeat_n('x', 1_048_576 - exact.len()));
        assert_eq!(
            NativeMessagingPolicy::from_toml(&exact).unwrap(),
            NativeMessagingPolicy::default()
        );
        exact.push('x');
        assert!(NativeMessagingPolicy::from_toml(&exact).is_err());
    }
    #[tokio::test]
    async fn file_policy_is_bounded_validated_and_immutable_after_load() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("operator.toml");
        std::fs::write(&path, document(&[grant()])).unwrap();
        let policy = NativeMessagingPolicy::load_file(&path).await.unwrap();
        std::fs::write(&path, "grants=[]").unwrap();
        assert_eq!(policy.grants().len(), 1);
        assert_eq!(
            NativeMessagingPolicy::load_file(&path).await.unwrap(),
            NativeMessagingPolicy::default()
        );
        for bytes in [vec![255], vec![b' '; 1_048_577]] {
            std::fs::write(&path, bytes).unwrap();
            assert!(NativeMessagingPolicy::load_file(&path).await.is_err());
        }
        assert!(NativeMessagingPolicy::load_file(temp.path()).await.is_err());
        assert!(
            NativeMessagingPolicy::load_file(temp.path().join("missing"))
                .await
                .is_err()
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn native_paths_and_regular_symlinks_preserve_policy() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(super::native_path::file_name());
        std::fs::write(&path, "grants=[]").unwrap();
        assert_eq!(
            NativeMessagingPolicy::load_file(&path).await.unwrap(),
            NativeMessagingPolicy::default()
        );
        let link = temp.path().join("link");
        symlink(path, &link).unwrap();
        assert_eq!(
            NativeMessagingPolicy::load_file(link).await.unwrap(),
            NativeMessagingPolicy::default()
        );
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_rejected_without_waiting_for_a_writer() {
        super::policy_fifo::rejects_without_writer(
            "enabled::fifo_is_rejected_without_waiting_for_a_writer",
            NativeMessagingPolicy::load_file,
        );
    }
}

#[cfg(all(feature = "dylib", unix))]
#[path = "support/messaging.rs"]
mod messaging;
#[cfg(all(feature = "dylib", unix))]
#[path = "native_policy/process.rs"]
mod process;
