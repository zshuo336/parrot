use std::process::Command;
use std::sync::Arc;

struct NoActors;
#[async_trait::async_trait]
impl parrot_remote::LocalLookup for NoActors {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_reads_measured_transport_without_a_component_executor() {
    let server = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("measured-test", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(NoActors),
    )
    .unwrap();
    server.start().await.unwrap();
    let node = format!("erl@measured-test={}", server.local_addr().unwrap());
    let status = Command::new(env!("CARGO_BIN_EXE_parrot-obs"))
        .args(["status", &node, "--insecure-development"])
        .output()
        .unwrap();
    let metrics = Command::new(env!("CARGO_BIN_EXE_parrot-obs"))
        .args(["metrics", &node, "--insecure-development"])
        .output()
        .unwrap();
    server.shutdown().await.unwrap();
    for result in [&status, &metrics] {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert!(String::from_utf8_lossy(&status.stdout).contains("conn=1"));
    assert!(String::from_utf8_lossy(&metrics.stdout).contains("n/a"));
}
#[test]
fn actual_help_exits_successfully_without_starting_a_network_client() {
    for args in [vec![], vec!["help"], vec!["--help"], vec!["load", "--help"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_parrot-obs"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("parrot-obs <command>"));
    }
}
#[test]
fn invalid_command_and_bad_limits_fail_before_connecting() {
    for args in [
        vec!["wat"],
        vec!["load", "ray=127.0.0.1:1", "--conc=0"],
        vec!["web", "ray=127.0.0.1:1", "--web-port=65536"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_parrot-obs"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("连接失败"));
    }
}
