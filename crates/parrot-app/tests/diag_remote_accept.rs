//! A4 辅助诊断：RemoteActorSystem 接受真实 erl 网关注册（隔离 AssemblingContext）。
#![cfg(feature = "host")]

use std::sync::Arc;

use parrot::thread::system::ThreadActorSystem;
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};

use parrot_app::host::HostLookup;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_accepts_erl_registration() {
    let ts = ThreadActorSystem::shared(Default::default());
    let sa: std::net::SocketAddr = "127.0.0.1:19960".parse().unwrap();
    let r = RemoteActorSystem::new(
        RemoteConfig::tcp("t-app", Some(sa)),
        Arc::new(HostLookup { ts }),
    )
    .unwrap();
    r.start().await.unwrap();

    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .current_dir(&repo_root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .arg("cd interop/erlang && exec erl -noshell -pa . -eval 'parrot_gw:main([0, \"parrot=127.0.0.1:19960\"])'");
    let mut child = cmd.spawn().expect("spawn erl");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        if r.nodes.get("erl-gw-1").is_some() {
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let _ = child.start_kill();
    let _ = r.shutdown().await;
    assert!(ok, "erl-gw-1 should register within 30s");
}
