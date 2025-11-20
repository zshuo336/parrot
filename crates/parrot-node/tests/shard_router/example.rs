use super::*;
use std::{net::TcpListener, path::Path};

fn command(
    binary: &str,
    config: &Path,
    member: &NodeAddr,
    directory: &NodeAddr,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("PARROT_"))
    {
        command.env_remove(key);
    }
    command
        .env("PARROT_CONFIG", config)
        .env("PARROT_ACTORS", "")
        .env(
            "PARROT_SHARD_MEMBERS",
            format!("tcp://{}@{}", member.node_id, member.addr),
        )
        .env(
            "PARROT_SHARD_DIRECTORY",
            format!("tcp://{}@{}", directory.node_id, directory.addr),
        )
        .env("PARROT_SHARD_GROUP", "register-cli")
        .kill_on_drop(true);
    command
}
async fn serve(binary: &str, config: &Path, member: &NodeAddr, directory: &NodeAddr) -> Process {
    let child = command(binary, config, member, directory)
        .arg("serve")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    collect_process(child).await
}
async fn invoke(
    binary: &str,
    config: &Path,
    member: &NodeAddr,
    directory: &NodeAddr,
    args: &[&str],
) -> String {
    let output = tokio::time::timeout(
        WAIT,
        command(binary, config, member, directory)
            .args(args)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
async fn stop(mut process: Process) {
    let pid = process.child.id().expect("live example server");
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) }, 0);
    let status = tokio::time::timeout(WAIT, process.child.wait())
        .await
        .unwrap()
        .unwrap();
    let output = process.output.await.unwrap();
    assert!(
        status.success(),
        "example shutdown failed: {status}: {output}"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the explicitly built SHARD_REGISTER_EXAMPLE executable"]
async fn register_example_cli() {
    let binary = std::env::var("SHARD_REGISTER_EXAMPLE")
        .expect("build sharded_register and select its exact executable");
    let root = tempfile::tempdir().unwrap();
    let names = [
        "register-host.test",
        "register-dir.test",
        "register-client.test",
        "register-observer.test",
    ];
    certificates(root.path(), &names);
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let member = NodeAddr::tcp(names[0], socket.local_addr().unwrap());
    let mut configs = Vec::new();
    for (name, address) in [
        (names[0], member.addr),
        (names[2], "127.0.0.1:0".parse().unwrap()),
    ] {
        let path = root.path().join(format!("{name}.toml"));
        let text = format!("[remote.node]\nnode_id={name:?}\nbind={:?}\n[remote.security]\nmode='mtls'\ncert_path={:?}\nkey_path={:?}\nca_path={:?}\n", address.to_string(), root.path().join(format!("{name}.pem")), root.path().join(format!("{name}.key")), root.path().join("ca.pem"));
        std::fs::write(&path, text).unwrap();
        configs.push(path);
    }
    std::fs::create_dir(root.path().join("directory")).unwrap();
    let mut directory = NetworkDirectory::start_with_timing(
        root.path().join("directory"),
        "register-cli",
        RemoteConfig::tcp(names[1], Some("127.0.0.1:0".parse().unwrap())).with_mtls(
            tls(root.path(), names[1]),
            vec![names[0].into(), names[3].into()],
        ),
        vec![],
        Arc::new(Empty),
        DirectoryTiming {
            election_ms: 100,
            heartbeat_ms: 20,
            rpc_timeout_ms: 500,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(WAIT, async {
        while directory.handle().observation().role != Role::Leader {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let peer = NodeAddr::tcp(names[1], directory.remote().local_addr().unwrap());
    let observer = RemoteActorSystem::new(
        RemoteConfig::tcp(names[3], None).with_mtls(tls(root.path(), names[3]), vec![]),
        Arc::new(Empty),
    )
    .unwrap();
    observer.start().await.unwrap();
    let authority =
        OwnershipClusterClient::new(observer.clone(), "register-cli", vec![peer.clone()]).unwrap();
    drop(socket);
    let first = serve(&binary, &configs[0], &member, &peer).await;
    let args = [
        "write",
        names[0],
        "alice",
        "example-once",
        "none",
        "example-state",
    ];
    let reply = invoke(&binary, &configs[1], &member, &peer, &args).await;
    assert!(reply.contains("ValueChanged"), "{reply}");
    assert_eq!(
        invoke(&binary, &configs[1], &member, &peer, &args).await,
        reply,
        "an identical same-generation write did not retain its original result"
    );
    let old = owner(&authority, "/shards/register/alice").await.unwrap();
    stop(first).await;
    assert!(owner(&authority, "/shards/register/alice")
        .await
        .unwrap()
        .holder
        .is_none());
    let second = serve(&binary, &configs[0], &member, &peer).await;
    let read = invoke(
        &binary,
        &configs[1],
        &member,
        &peer,
        &["read", names[0], "alice"],
    )
    .await;
    assert!(
        read.contains("ValueObserved") && read.contains("example-state"),
        "{read}"
    );
    assert!(
        owner(&authority, "/shards/register/alice")
            .await
            .unwrap()
            .generation
            > old.generation
    );
    let conflict = invoke(&binary, &configs[1], &member, &peer, &args).await;
    assert!(
        conflict.contains("Conflict"),
        "cross-generation write identity was silently rewritten: {conflict}"
    );
    stop(second).await;
    observer.shutdown().await.unwrap();
    directory.close().await.unwrap();
}
