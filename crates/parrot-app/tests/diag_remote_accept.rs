//! A4 辅助诊断：RemoteActorSystem 接受真实 erl 网关注册（隔离 AssemblingContext）。
#![cfg(feature = "host")]

use std::sync::Arc;

use parrot::thread::system::ThreadActorSystem;
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};

use parrot_app::host::HostLookup;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_accepts_erl_registration() {
    let ts = ThreadActorSystem::shared(Default::default());
    let sa: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let r = RemoteActorSystem::new(
        RemoteConfig::tcp("t-app", Some(sa)),
        Arc::new(HostLookup { ts: ts.clone() }),
    )
    .unwrap();
    r.start().await.unwrap();

    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let gateway_dir = repo_root.join("interop/erlang");
    let status = tokio::process::Command::new("erlc")
        .current_dir(&gateway_dir)
        .args([
            "parrot_gw.erl",
            "parrot_trace.erl",
            "parrot_component_runtime.erl",
            "parrot_beam_snapshot.erl",
            "parrot_beam_guest.erl",
            "parrot_beam_host.erl",
        ])
        .kill_on_drop(true)
        .status()
        .await
        .expect("Erlang compiler is required");
    assert!(status.success(), "gateway compilation failed: {status}");
    let mut cmd = tokio::process::Command::new("erl");
    cmd.args(["-noshell", "-pa"])
        .arg(&gateway_dir)
        .arg("-eval")
        .arg(format!(
            "parrot_gw:main([0, \"parrot={}\"])",
            r.local_addr().unwrap()
        ))
        .env("PARROT_NODE_ID", "erl-gw-1")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    let mut child = cmd.spawn().expect("spawn erl");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("inspect gateway process") {
            panic!("Erlang gateway exited before registration: {status}");
        }
        if r.links_snapshot().await.iter().any(|(node, _, status)| {
            node == "erl-gw-1" && status.get() == parrot_remote::node::NodeState::Connected
        }) {
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    child.kill().await.expect("kill and reap gateway");
    r.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();
    assert!(ok, "erl-gw-1 should register within 30s");
}
