#![cfg(unix)]
//! One embedded standard node plus two actual parrot-node executables. All use
//! RunningNode assembly, real mTLS TCP, real actors and independent disk owners.
use parrot_api::address::ActorRef;
use parrot_node::{
    configuration::NodeSettings,
    directory::{LocalEntry, QueryLocal},
    runtime::RunningNode,
};
use parrot_remote::{
    raft::Role,
    roles::directory::{client::DirectoryClient, DirCmd, DirQuery, DirectoryRequestStatus},
    tls::TlsConfig,
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{net::TcpListener, path::Path, sync::Arc, time::Duration};
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
fn certificates(root: &Path, names: &[&str]) {
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    let ca = params.self_signed(&key).unwrap().pem();
    std::fs::write(root.join("ca.pem"), ca).unwrap();
    let issuer = rcgen::Issuer::new(params, key);
    for name in names {
        let mut params = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, *name);
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.signed_by(&key, &issuer).unwrap();
        std::fs::write(root.join(format!("{name}.pem")), certificate.pem()).unwrap();
        std::fs::write(root.join(format!("{name}.key")), key.serialize_pem()).unwrap();
    }
}
fn start(config: &Path) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_parrot-node"));
    for (key, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("PARROT_"))
    {
        cmd.env_remove(key);
    }
    cmd.env("PARROT_CONFIG", config)
        .env("PARROT_ACTORS", "echo")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}
async fn ready(child: &mut Child) {
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            assert!(
                reader.read_line(&mut line).await.unwrap() > 0,
                "process exited before readiness: {line}"
            );
            assert!(line.len() < 65536);
            if line.starts_with("PARROT_NODE_READY=") {
                break;
            }
            line.clear();
        }
    })
    .await
    .expect("process launch deadline");
}
async fn replicated(
    parent: &RunningNode,
    names: &[&str],
    query: &DirQuery,
    expected: &parrot_remote::roles::directory::DirEntry,
) {
    for name in &names[1..] {
        let actor = parent
            .remote
            .remote_ref(&format!("parrot://{name}/system/directory"))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(reply) = ActorRef::send_with_timeout(
                    &actor,
                    Box::new(QueryLocal(query.clone())),
                    Some(Duration::from_secs(2)),
                )
                .await
                {
                    if *reply.downcast::<LocalEntry>().unwrap()
                        == LocalEntry(Some(expected.clone()))
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("{name} did not expose the exact applied entry through its real actor")
        });
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_standard_assemblies_replicate_through_real_actors_and_recover_a_killed_process() {
    let root = std::env::temp_dir().join(format!("parrot-standard-cluster-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let names = ["standard-a.test", "standard-b.test", "standard-c.test"];
    let writer = "standard-writer.test";
    certificates(&root, &[names[0], names[1], names[2], writer]);
    let mut reserved: Vec<_> = (0..3)
        .map(|_| Some(TcpListener::bind("127.0.0.1:0").unwrap()))
        .collect();
    let addresses: Vec<_> = reserved
        .iter()
        .map(|s| s.as_ref().unwrap().local_addr().unwrap())
        .collect();
    let mut configs = Vec::new();
    for i in 0..3 {
        let data = root.join(names[i]);
        std::fs::create_dir(&data).unwrap();
        let peers: Vec<_> = (0..3)
            .filter(|j| *j != i)
            .map(|j| format!("tcp://{}@localhost:{}", names[j], addresses[j].port()))
            .collect();
        let text=format!("[remote.node]\nnode_id={:?}\nbind={:?}\ntopology_role='directory'\n[remote.security]\nmode='mtls'\ncert_path={:?}\nkey_path={:?}\nca_path={:?}\n[remote.directory]\nstorage_dir={:?}\ngroup='standard-cluster'\npeers={:?}\nelection_ms={}\nheartbeat_ms=100\nrpc_timeout_ms=1500\n",names[i],addresses[i].to_string(),root.join(format!("{}.pem",names[i])).to_str().unwrap(),root.join(format!("{}.key",names[i])).to_str().unwrap(),root.join("ca.pem").to_str().unwrap(),data.to_str().unwrap(),peers,if i==0{500}else{5000});
        let path = root.join(format!("{i}.toml"));
        let text = text.replace(
            "mode='mtls'",
            &format!("mode='mtls'\nadmin_nodes=[{writer:?}]"),
        );
        std::fs::write(&path, text).unwrap();
        configs.push(path);
    }
    let settings = NodeSettings::from_environment(|key| match key {
        "PARROT_CONFIG" => Some(configs[0].clone().into_os_string()),
        "PARROT_ACTORS" => Some("echo".into()),
        _ => None,
    })
    .unwrap();
    drop(reserved[0].take());
    let parent = RunningNode::start(&settings).await.unwrap();
    let handle = parent.directory().unwrap();
    let mut children = Vec::new();
    for i in 1..3 {
        drop(reserved[i].take());
        children.push(start(&configs[i]));
    }
    for child in &mut children {
        ready(child).await;
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        while handle.observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let transport = RemoteActorSystem::new(
        RemoteConfig::tcp(writer, None).with_mtls(
            TlsConfig {
                cert_pem: std::fs::read(root.join(format!("{writer}.pem"))).unwrap(),
                key_pem: std::fs::read(root.join(format!("{writer}.key"))).unwrap(),
                ca_pem: std::fs::read(root.join("ca.pem")).unwrap(),
            },
            [],
        ),
        Arc::new(Empty),
    )
    .unwrap();
    transport.start().await.unwrap();
    let client = DirectoryClient::new(transport.clone(), "standard-cluster").unwrap();
    let leader_route = NodeAddr::tcp(names[0], addresses[0]);
    let query = DirQuery::Resolve {
        prefix_or_node: "worker".into(),
    };
    let mut pre_kill = None;
    for revision in 1..=2 {
        let command = DirCmd::Upsert {
            node: "worker".into(),
            endpoints: vec![format!("tcp://worker:{revision}")],
        };
        let id = format!("revision-{revision}");
        let receipt = client
            .submit_and_wait(&leader_route, &id, &command, Duration::from_secs(10))
            .await
            .unwrap();
        assert!(
            matches!(receipt, DirectoryRequestStatus::Applied { revision: actual, .. } if actual == revision)
        );
        assert_eq!(
            client
                .submit(&leader_route, &id, &command, Duration::from_secs(2))
                .await
                .unwrap(),
            receipt
        );
        let expected = handle.query_local(&query).await.unwrap().unwrap();
        assert_eq!(expected.version, revision);
        if revision == 2 {
            children[1] = start(&configs[2]);
            ready(&mut children[1]).await;
        }
        replicated(&parent, &names, &query, &expected).await;
        let read = client
            .query_linearizable(&leader_route, &query, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(read.entry, Some(expected.clone()));
        assert!(read.applied_index >= read.barrier_index);
        assert!(read.barrier_index > revision);
        // The restarted real executable must recover the request identity, not
        // merely the business entry. Retrying on a follower returns its original
        // positive commit proof without a second mutation.
        let follower = NodeAddr::tcp(names[2], addresses[2]);
        assert_eq!(
            client
                .submit(&follower, &id, &command, Duration::from_secs(3))
                .await
                .unwrap(),
            receipt
        );
        if let Some((old_command, old_receipt)) = &pre_kill {
            assert_eq!(
                client
                    .submit(&follower, "revision-1", old_command, Duration::from_secs(3))
                    .await
                    .unwrap(),
                *old_receipt
            );
            // Replaying the pre-crash request must not overwrite revision2.
            replicated(&parent, &names, &query, &expected).await;
        }
        if revision == 1 {
            pre_kill = Some((command, receipt));
            children[1].kill().await.unwrap();
            assert!(!children[1].wait().await.unwrap().success());
        }
    }
    for name in &names[1..] {
        let actor = parent
            .remote
            .remote_ref(&format!("parrot://{name}/user/echo"))
            .unwrap();
        let reply = ActorRef::send_with_timeout(
            &actor,
            Box::new(parrot_node::NEcho(99)),
            Some(Duration::from_secs(2)),
        )
        .await
        .unwrap();
        assert_eq!(
            *reply.downcast::<parrot_node::NEchoed>().unwrap(),
            parrot_node::NEchoed(99)
        );
    }
    transport.shutdown().await.unwrap();
    parent.shutdown().await.unwrap();
    for child in &mut children {
        assert!(Command::new("kill")
            .arg("-TERM")
            .arg(child.id().unwrap().to_string())
            .status()
            .await
            .unwrap()
            .success());
    }
    for child in &mut children {
        assert!(tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
    }
    drop(parent);
    std::fs::remove_dir_all(root).unwrap();
}
