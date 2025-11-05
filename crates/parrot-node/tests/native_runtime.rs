#![cfg(feature = "dylib")]
#[path = "support/native.rs"]
mod native;
use parrot_api::address::ActorRefExt;
use parrot_remote::admin_v2::{AdminReplyV2 as R, ComponentExecutor};

// NODE is intentionally process scoped. Run each startup policy in fresh
// processes, and require a completion marker so an empty child cannot pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_runtime_authority_and_process_owner_policy_are_explicit_and_immutable() {
    let case = std::env::var("PARROT_NATIVE_RUNTIME_CASE").ok();
    if let Some(case) = case {
        check_policy(&case).await;
        println!("NATIVE_RUNTIME_POLICY_VERIFIED={case}");
    } else {
        for case in ["safe-default", "trusted", "trusted-custom-epoch"] {
            if case == "trusted-custom-epoch" && !cfg!(feature = "wasm") {
                continue;
            }
            let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "native_runtime_authority_and_process_owner_policy_are_explicit_and_immutable",
                    "--nocapture",
                ])
                .env("PARROT_NATIVE_RUNTIME_CASE", case)
                .kill_on_drop(true);
            let output =
                tokio::time::timeout(std::time::Duration::from_secs(120), command.output())
                    .await
                    .unwrap()
                    .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout)
                .contains(&format!("NATIVE_RUNTIME_POLICY_VERIFIED={case}")));
        }
    }
}
async fn check_policy(case: &str) {
    let fixture = native::NativeFixture::new();
    let mut settings =
        parrot_node::configuration::NodeSettings::from_environment(|key| match key {
            "PARROT_NODE_ID" => Some("native-runtime".into()),
            "PARROT_BIND" => Some("127.0.0.1:0".into()),
            "PARROT_ACTORS" => Some("".into()),
            "PARROT_TRUST_NATIVE_SHA256" => Some(fixture.digest.clone().into()),
            _ => None,
        })
        .unwrap();
    if case == "trusted-custom-epoch" {
        settings.wasm_epoch_ticks = Some(1000);
    }
    let trust = fixture.trust();
    let node = if case.starts_with("trusted") {
        parrot_node::runtime::RunningNode::start_with_native_trust(&settings, &trust)
            .await
            .unwrap()
    } else {
        parrot_node::runtime::RunningNode::start(&settings)
            .await
            .unwrap()
    };
    let executor = parrot_node::NodeComponentExecutor::new(node.thread.clone());
    let command = fixture.component("runtime", 1);
    if case.starts_with("trusted") {
        assert_ne!(
            node.remote.config.extra_caps & parrot_remote::handshake::caps::DYLIB,
            0
        );
        assert!(matches!(
            executor.deploy(1, &command).await,
            R::Deployed { .. }
        ));
        let actor = node.thread.get_actor_ref("/user/runtime-0").unwrap();
        actor
            .ask(parrot_node::DylibTell {
                type_key: "set".into(),
                payload: 23u64.to_le_bytes().to_vec(),
            })
            .await
            .unwrap();
        let configured =
            parrot_node::NodeComponentExecutor::with_native_trust(node.thread.clone(), &trust);
        let same = if case == "trusted-custom-epoch" {
            assert!(
                configured.is_err(),
                "default budget must not replace custom policy"
            );
            parrot_node::NodeComponentExecutor::new(node.thread.clone())
        } else {
            configured.unwrap()
        };
        assert!(matches!(same.deploy(2, &command).await, R::Deployed { .. }));
        assert!(parrot_node::NodeComponentExecutor::with_native_trust(
            node.thread.clone(),
            &Default::default()
        )
        .is_err());
        let another = unsafe { parrot_node::NativeTrust::authorize(&["0".repeat(64)]) }.unwrap();
        assert!(parrot_node::NodeComponentExecutor::with_native_trust(
            node.thread.clone(),
            &another
        )
        .is_err());
        assert_eq!(
            actor
                .ask(parrot_node::DylibAsk {
                    type_key: "value".into(),
                    payload: vec![]
                })
                .await
                .unwrap()
                .0,
            23u64.to_le_bytes()
        );
        assert_eq!(
            parrot_node::exec_deploy_v2(&node.thread, command.clone())
                .await
                .unwrap(),
            ["/user/runtime-0"]
        );
        assert!(matches!(
            same.stop(4, "/user/runtime").await,
            R::Stopped { .. }
        ));
    } else {
        assert_eq!(
            node.remote.config.extra_caps & parrot_remote::handshake::caps::DYLIB,
            0
        );
        let reply = executor.deploy(1, &command).await;
        assert!(
            matches!(reply,R::Failed{ref detail,..} if detail.contains("trust authorization")),
            "{reply:?}"
        );
        assert!(
            parrot_node::NodeComponentExecutor::with_native_trust(node.thread.clone(), &trust)
                .is_err()
        );
        assert!(fixture.events().is_empty());
    }
    node.shutdown().await.unwrap();
}
