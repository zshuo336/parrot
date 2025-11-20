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
    roles::directory::{
        client::DirectoryClient,
        ownership::{
            OwnershipClient, OwnershipClusterClient, OwnershipCommand, OwnershipOutcome,
            OwnershipRejection, OwnershipStatus,
        },
        DirCmd, DirQuery, DirectoryRequestStatus,
    },
    tls::TlsConfig,
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    net::TcpListener,
    ops::RangeInclusive,
    path::Path,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

fn automatic_port_ranges() -> Vec<RangeInclusive<u16>> {
    #[cfg(target_os = "linux")]
    let values = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .expect("read the kernel automatic TCP port range");
    #[cfg(not(target_os = "linux"))]
    let values = {
        let output = std::process::Command::new("sysctl")
            .args([
                "-n",
                "net.inet.ip.portrange.first",
                "net.inet.ip.portrange.last",
                "net.inet.ip.portrange.hifirst",
                "net.inet.ip.portrange.hilast",
                "net.inet.ip.portrange.lowfirst",
                "net.inet.ip.portrange.lowlast",
            ])
            .output()
            .expect("query automatic TCP port ranges");
        assert!(
            output.status.success(),
            "query automatic TCP port ranges: {output:?}"
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let values: Vec<u16> = values
        .split_whitespace()
        .map(|value| value.parse().unwrap())
        .collect();
    let (pairs, remainder) = values.as_chunks::<2>();
    assert!(!pairs.is_empty() && remainder.is_empty());
    pairs
        .iter()
        .map(|pair| pair[0].min(pair[1])..=pair[0].max(pair[1]))
        .collect()
}

fn cluster_ports() -> Vec<Option<TcpListener>> {
    static NEXT: AtomicU32 = AtomicU32::new(1024);
    let automatic = automatic_port_ranges();
    (0..3)
        .map(|_| {
            loop {
                let port = u16::try_from(NEXT.fetch_add(1, Ordering::Relaxed))
                    .expect("no available cluster ports outside the kernel automatic ranges");
                if automatic.iter().any(|range| range.contains(&port)) {
                    continue;
                }
                // A released :0 reservation can immediately become another connection's
                // source port. Use distinct explicit ports outside the actual OS ranges.
                match TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], port))) {
                    Ok(listener) => break Some(listener),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                        ) => {}
                    Err(error) => panic!("reserve cluster TCP port: {error}"),
                }
            }
        })
        .collect()
}

#[test]
fn cluster_port_reservations_are_distinct_and_exclude_automatic_ranges() {
    let first = cluster_ports();
    let second = cluster_ports();
    let ranges = automatic_port_ranges();
    let mut selected = std::collections::HashSet::new();
    for listener in first.iter().chain(&second) {
        let port = listener.as_ref().unwrap().local_addr().unwrap().port();
        assert!(
            selected.insert(port),
            "two live cluster reservations share a port"
        );
        assert!(
            !ranges.iter().any(|range| range.contains(&port)),
            "released cluster reservation can be reused as an automatic source port"
        );
    }
}
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
    let mut reserved = cluster_ports();
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
        let text = format!(
            "[remote.node]\nnode_id={:?}\nbind={:?}\ntopology_role='directory'\n[remote.security]\nmode='mtls'\ncert_path={:?}\nkey_path={:?}\nca_path={:?}\n[remote.directory]\nstorage_dir={:?}\ngroup='standard-cluster'\npeers={:?}\nelection_ms={}\nheartbeat_ms=100\nrpc_timeout_ms=1500\n",
            names[i],
            addresses[i].to_string(),
            root.join(format!("{}.pem", names[i])).to_str().unwrap(),
            root.join(format!("{}.key", names[i])).to_str().unwrap(),
            root.join("ca.pem").to_str().unwrap(),
            data.to_str().unwrap(),
            peers,
            if i == 0 { 500 } else { 5000 }
        );
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
    let ownership = OwnershipClient::new(transport.clone(), "standard-cluster").unwrap();
    let leader_route = NodeAddr::tcp(names[0], addresses[0]);
    let claim = OwnershipCommand::Claim {
        resource: "/user/echo".into(),
        expected: None,
    };
    let original_claim = ownership
        .submit_and_wait(&leader_route, "echo-owner", &claim, Duration::from_secs(10))
        .await
        .unwrap();
    let generation = match &original_claim {
        OwnershipStatus::Applied {
            outcome: OwnershipOutcome::Changed(record),
            ..
        } => {
            assert_eq!(record.holder.as_deref(), Some(writer));
            record.generation
        }
        status => panic!("expected committed ownership: {status:?}"),
    };
    let write = OwnershipCommand::WriteValue {
        resource: "/user/echo".into(),
        generation,
        expected_revision: None,
        value: 42_u64.to_le_bytes().to_vec(),
    };
    let original_write = ownership
        .submit_and_wait(&leader_route, "echo-state", &write, Duration::from_secs(10))
        .await
        .unwrap();
    let state = match &original_write {
        OwnershipStatus::Applied {
            index,
            outcome: OwnershipOutcome::ValueChanged { ownership, value },
            ..
        } => {
            assert_eq!(ownership.holder.as_deref(), Some(writer));
            assert_eq!(ownership.generation, generation);
            assert_eq!(value.revision, *index);
            assert_eq!(value.value.as_ref(), 42_u64.to_le_bytes());
            value.clone()
        }
        status => panic!("expected committed fenced state: {status:?}"),
    };
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
            let index = handle.compact().await.unwrap();
            assert!(
                matches!(receipt, DirectoryRequestStatus::Applied { index: applied, .. } if index >= applied)
            );
            assert!(std::fs::read(root.join(names[0]).join("raft.state"))
                .unwrap()
                .starts_with(b"PRAFT03\0"));
            children[1] = start(&configs[2]);
            ready(&mut children[1]).await;
        }
        replicated(&parent, &names, &query, &expected).await;
        if revision == 2 {
            assert!(
                std::fs::read(root.join(names[2]).join("raft.state"))
                    .unwrap()
                    .starts_with(b"PRAFT03\0"),
                "the killed standard node must install its missing prefix as a persisted application snapshot"
            );
        }
        let read = client
            .query_linearizable(&leader_route, &query, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(read.entry, Some(expected.clone()));
        assert!(read.applied_index >= read.barrier_index);
        assert!(
            matches!(receipt, DirectoryRequestStatus::Applied { index, .. } if read.barrier_index >= index)
        );
        // The restarted real executable must recover the request identity, not
        // merely the business entry. Retrying on a follower returns its original
        // positive commit proof without a second mutation.
        let follower = NodeAddr::tcp(names[2], addresses[2]);
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let status = ownership
                    .status(
                        &follower,
                        "echo-owner",
                        claim.digest().unwrap(),
                        Duration::from_secs(2),
                    )
                    .await
                    .unwrap();
                if status == original_claim {
                    let state_status = ownership
                        .status(
                            &follower,
                            "echo-state",
                            write.digest().unwrap(),
                            Duration::from_secs(2),
                        )
                        .await
                        .unwrap();
                    if state_status == original_write {
                        break;
                    }
                    assert!(
                        matches!(
                            state_status,
                            OwnershipStatus::Unknown | OwnershipStatus::Pending { .. }
                        ),
                        "{state_status:?}"
                    );
                } else {
                    assert!(
                        matches!(
                            status,
                            OwnershipStatus::Unknown | OwnershipStatus::Pending { .. }
                        ),
                        "{status:?}"
                    );
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("ownership identity must replicate and survive process recovery");
        assert_eq!(
            ownership
                .submit(&follower, "echo-owner", &claim, Duration::from_secs(3))
                .await
                .unwrap(),
            original_claim
        );
        assert_eq!(
            ownership
                .submit(&follower, "echo-state", &write, Duration::from_secs(3))
                .await
                .unwrap(),
            original_write
        );
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
    let next = ownership
        .submit_and_wait(
            &leader_route,
            "next-owner",
            &OwnershipCommand::Claim {
                resource: "/user/echo".into(),
                expected: Some(generation),
            },
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let next_owner = match next {
        OwnershipStatus::Applied {
            outcome: OwnershipOutcome::Changed(record),
            ..
        } => record,
        status => panic!("expected replacement ownership: {status:?}"),
    };
    assert!(next_owner.generation > generation);
    let delayed = ownership
        .submit_and_wait(
            &leader_route,
            "delayed-state",
            &OwnershipCommand::WriteValue {
                resource: "/user/echo".into(),
                generation,
                expected_revision: Some(state.revision),
                value: b"must-not-apply".to_vec(),
            },
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(delayed, OwnershipStatus::Applied { outcome: OwnershipOutcome::Rejected { reason: OwnershipRejection::GenerationConflict, current: Some(record) }, .. } if record == next_owner)
    );
    assert!(
        matches!(ownership.read_value(&leader_route, "/user/echo", Duration::from_secs(10)).await.unwrap(), OwnershipStatus::Applied { outcome: OwnershipOutcome::ValueObserved { ownership: Some(record), value: Some(value) }, .. } if record == next_owner && value == state)
    );
    let released = ownership
        .submit_and_wait(
            &leader_route,
            "echo-release",
            &OwnershipCommand::Release {
                resource: "/user/echo".into(),
                expected: next_owner.generation,
            },
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let tombstone = match released {
        OwnershipStatus::Applied {
            outcome: OwnershipOutcome::Changed(record),
            ..
        } => {
            assert!(record.generation > next_owner.generation);
            assert_eq!(record.holder, None);
            record
        }
        status => panic!("expected committed release: {status:?}"),
    };
    assert!(
        matches!(ownership.observe(&leader_route, "/user/echo", Duration::from_secs(10)).await.unwrap(), OwnershipStatus::Applied { outcome: OwnershipOutcome::Observed(Some(record)), .. } if record == tombstone)
    );
    assert!(
        matches!(ownership.read_value(&leader_route, "/user/echo", Duration::from_secs(10)).await.unwrap(), OwnershipStatus::Applied { outcome: OwnershipOutcome::ValueObserved { ownership: Some(record), value: Some(value) }, .. } if record == tombstone && value == state)
    );
    assert_eq!(
        ownership
            .submit(&leader_route, "echo-owner", &claim, Duration::from_secs(3))
            .await
            .unwrap(),
        original_claim
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ownership_client_recovers_after_real_leader_sigkill_without_reapplying_state() {
    use std::os::unix::process::ExitStatusExt;
    let root = tempfile::tempdir().unwrap();
    let names = ["failover-a.test", "failover-b.test", "failover-c.test"];
    let writer = "failover-writer.test";
    certificates(root.path(), &[names[0], names[1], names[2], writer]);
    let reserved = cluster_ports();
    let addresses: Vec<_> = reserved
        .iter()
        .map(|socket| socket.as_ref().unwrap().local_addr().unwrap())
        .collect();
    let routes: Vec<_> = (0..3)
        .map(|i| NodeAddr::tcp(names[i], addresses[i]))
        .collect();
    let mut configs = Vec::new();
    for i in 0..3 {
        let data = root.path().join(names[i]);
        std::fs::create_dir(&data).unwrap();
        let peers: Vec<_> = (0..3)
            .filter(|j| *j != i)
            .map(|j| format!("tcp://{}@{}", names[j], addresses[j]))
            .collect();
        let path = root.path().join(format!("{i}.toml"));
        std::fs::write(&path, format!("[remote.node]\nnode_id={:?}\nbind={:?}\ntopology_role='directory'\n[remote.security]\nmode='mtls'\nadmin_nodes=[{writer:?}]\ncert_path={:?}\nkey_path={:?}\nca_path={:?}\n[remote.directory]\nstorage_dir={:?}\ngroup='failover-cluster'\npeers={peers:?}\nelection_ms={}\nheartbeat_ms=40\nrpc_timeout_ms=300\n", names[i], addresses[i].to_string(), root.path().join(format!("{}.pem", names[i])), root.path().join(format!("{}.key", names[i])), root.path().join("ca.pem"), data, if i == 0 { 200 } else { 800 })).unwrap();
        configs.push(path);
    }
    drop(reserved);
    let mut children: Vec<_> = configs.iter().map(|path| start(path)).collect();
    for child in &mut children {
        ready(child).await;
    }
    let transport = RemoteActorSystem::new(
        RemoteConfig::tcp(writer, None).with_mtls(
            TlsConfig {
                cert_pem: std::fs::read(root.path().join(format!("{writer}.pem"))).unwrap(),
                key_pem: std::fs::read(root.path().join(format!("{writer}.key"))).unwrap(),
                ca_pem: std::fs::read(root.path().join("ca.pem")).unwrap(),
            },
            [],
        ),
        Arc::new(Empty),
    )
    .unwrap();
    transport.start().await.unwrap();
    // This real TCP endpoint never completes TLS. It must not consume the total
    // request deadline and prevent healthy authenticated members being tried.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut candidates = vec![NodeAddr::tcp("silent.test", silent.local_addr().unwrap())];
    candidates.extend(routes.clone());
    let client =
        OwnershipClusterClient::new(transport.clone(), "failover-cluster", candidates).unwrap();
    let direct = OwnershipClient::new(transport.clone(), "failover-cluster").unwrap();
    let timeout = Duration::from_secs(15);
    let claim = OwnershipCommand::Claim {
        resource: "entity".into(),
        expected: None,
    };
    let original_claim = client
        .submit_and_wait("owner", &claim, timeout)
        .await
        .unwrap();
    let old_owner = match &original_claim {
        OwnershipStatus::Applied {
            outcome: OwnershipOutcome::Changed(record),
            ..
        } => record.clone(),
        other => panic!("{other:?}"),
    };
    let first = OwnershipCommand::WriteValue {
        resource: "entity".into(),
        generation: old_owner.generation,
        expected_revision: None,
        value: b"before-kill".to_vec(),
    };
    let original = client
        .submit_and_wait("state", &first, timeout)
        .await
        .unwrap();
    let (old_term, value) = match &original {
        OwnershipStatus::Applied {
            term,
            outcome: OwnershipOutcome::ValueChanged { value, .. },
            ..
        } => (*term, value.clone()),
        other => panic!("{other:?}"),
    };
    // A fresh committed linear read identifies the actual live leader. Node
    // order and startup timing do not substitute for an authoritative result.
    let directory = DirectoryClient::new(transport.clone(), "failover-cluster").unwrap();
    let (leader, leader_term) = tokio::time::timeout(timeout, async {
        'find: loop {
            for (index, route) in routes.iter().enumerate() {
                if let Ok(read) = directory
                    .query_linearizable(
                        route,
                        &DirQuery::Resolve {
                            prefix_or_node: "absent".into(),
                        },
                        Duration::from_millis(300),
                    )
                    .await
                {
                    break 'find (index, read.term);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(leader_term >= old_term);
    children[leader].start_kill().unwrap();
    let killed = children[leader].wait().await.unwrap();
    assert_eq!(killed.signal(), Some(9), "must kill the observed leader");
    let recovered = client.read_value("entity", timeout).await.unwrap();
    assert!(
        matches!(recovered, OwnershipStatus::Applied { term, outcome: OwnershipOutcome::ValueObserved { ownership: Some(record), value: Some(current) }, .. } if term > leader_term && record == old_owner && current == value)
    );
    assert_eq!(
        client
            .submit_and_wait("state", &first, timeout)
            .await
            .unwrap(),
        original
    );
    assert_eq!(
        client
            .submit_and_wait("owner", &claim, timeout)
            .await
            .unwrap(),
        original_claim
    );
    let next = client
        .submit_and_wait(
            "next-owner",
            &OwnershipCommand::Claim {
                resource: "entity".into(),
                expected: Some(old_owner.generation),
            },
            timeout,
        )
        .await
        .unwrap();
    let next_owner = match next {
        OwnershipStatus::Applied {
            outcome: OwnershipOutcome::Changed(record),
            ..
        } => record,
        other => panic!("{other:?}"),
    };
    assert!(next_owner.generation > old_owner.generation);
    let stale = client
        .submit_and_wait(
            "stale",
            &OwnershipCommand::WriteValue {
                resource: "entity".into(),
                generation: old_owner.generation,
                expected_revision: Some(value.revision),
                value: b"stale".to_vec(),
            },
            timeout,
        )
        .await
        .unwrap();
    assert!(
        matches!(stale, OwnershipStatus::Applied { outcome: OwnershipOutcome::Rejected { reason: OwnershipRejection::GenerationConflict, current: Some(record) }, .. } if record == next_owner)
    );
    let updated = client
        .submit_and_wait(
            "new-state",
            &OwnershipCommand::WriteValue {
                resource: "entity".into(),
                generation: next_owner.generation,
                expected_revision: Some(value.revision),
                value: b"after-kill".to_vec(),
            },
            timeout,
        )
        .await
        .unwrap();
    let final_value = match &updated {
        OwnershipStatus::Applied {
            outcome: OwnershipOutcome::ValueChanged { value, .. },
            ..
        } => value.clone(),
        other => panic!("{other:?}"),
    };
    assert!(final_value.revision > value.revision);
    assert_eq!(
        client
            .submit_and_wait("state", &first, timeout)
            .await
            .unwrap(),
        original
    );
    assert_eq!(
        client
            .submit_and_wait("state", &claim, timeout)
            .await
            .unwrap(),
        OwnershipStatus::Conflict
    );
    assert!(
        matches!(client.read_value("entity", timeout).await.unwrap(), OwnershipStatus::Applied { outcome: OwnershipOutcome::ValueObserved { ownership: Some(record), value: Some(current) }, .. } if record == next_owner && current == final_value)
    );
    children[leader] = start(&configs[leader]);
    ready(&mut children[leader]).await;
    let update_command = OwnershipCommand::WriteValue {
        resource: "entity".into(),
        generation: next_owner.generation,
        expected_revision: Some(value.revision),
        value: b"after-kill".to_vec(),
    };
    let digest = update_command.digest().unwrap();
    tokio::time::timeout(timeout, async {
        loop {
            if let Ok(status) = direct
                .status(&routes[leader], "new-state", digest, Duration::from_secs(1))
                .await
            {
                if status == updated {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("restarted former leader must apply the exact new committed result");
    transport.shutdown().await.unwrap();
    assert_eq!(transport.callbacks.len(), 0);
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
}
