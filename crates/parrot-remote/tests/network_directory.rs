use bytes::Bytes;

use parrot_remote::{
    raft::{RaftRpc, Role},
    roles::directory::{
        network::{NetworkDirectory, NetworkDirectoryError, RAFT_ACK_KEY, RAFT_KEY, RAFT_PATH},
        DirCmd, DirQuery, DurableDirectoryStatus,
    },
    tls::TlsConfig,
    ErrCode, Frame, LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig, ReplyPayload,
};
use std::{net::TcpListener, path::PathBuf, sync::Arc, time::Duration};
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-network-directory-{}-{}",
            std::process::id(),
            ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn credentials(names: &[&str]) -> Vec<TlsConfig> {
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    let ca = params.self_signed(&key).unwrap().pem().into_bytes();
    let issuer = rcgen::Issuer::new(params, key);
    names
        .iter()
        .map(|name| {
            let mut params = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap();
            params.distinguished_name = rcgen::DistinguishedName::new();
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, *name);
            let key = rcgen::KeyPair::generate().unwrap();
            TlsConfig {
                cert_pem: params.signed_by(&key, &issuer).unwrap().pem().into_bytes(),
                key_pem: key.serialize_pem().into_bytes(),
                ca_pem: ca.clone(),
            }
        })
        .collect()
}
fn put(node: &str) -> DirCmd {
    DirCmd::Upsert {
        node: node.into(),
        endpoints: vec![format!("tcp://{node}:7")],
    }
}
fn query(node: &str) -> DirQuery {
    DirQuery::Resolve {
        prefix_or_node: node.into(),
    }
}
async fn leader(nodes: &[Option<NetworkDirectory>]) -> usize {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let leaders: Vec<_> = nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| {
                    n.as_ref()
                        .is_some_and(|n| n.handle().observation().role == Role::Leader)
                })
                .map(|(i, _)| i)
                .collect();
            if leaders.len() == 1 {
                return leaders[0];
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "actual network election must progress: {:?}",
            nodes
                .iter()
                .flatten()
                .map(|n| n.handle().observation())
                .collect::<Vec<_>>()
        )
    })
}
async fn applied(nodes: &[Option<NetworkDirectory>], index: u64) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if nodes
                .iter()
                .flatten()
                .all(|n| n.handle().observation().applied_index == index)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "replicas did not apply {index}: {:?}",
            nodes
                .iter()
                .flatten()
                .map(|n| n.handle().observation())
                .collect::<Vec<_>>()
        )
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_mtls_tcp_replicas_commit_catch_up_elect_and_recover_exact_history() {
    let names = ["dir-a.test", "dir-b.test", "dir-c.test"];
    let certs = credentials(&names);
    let dirs: Vec<_> = (0..3).map(|_| Scratch::new()).collect();
    // Reserve all addresses together. Release each listener immediately before
    // starting its owner, keeping the remaining future endpoints reserved.
    let mut reserves: Vec<_> = (0..3)
        .map(|_| Some(TcpListener::bind("127.0.0.1:0").unwrap()))
        .collect();
    let routes: Vec<_> = names
        .iter()
        .zip(&reserves)
        .map(|(name, r)| NodeAddr::tcp(*name, r.as_ref().unwrap().local_addr().unwrap()))
        .collect();
    let start = |i: usize| {
        NetworkDirectory::start(
            dirs[i].0.clone(),
            "network-contract",
            RemoteConfig::tcp(names[i], Some(routes[i].addr)).with_mtls(certs[i].clone(), []),
            routes
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, p)| p.clone())
                .collect(),
            Arc::new(Empty),
        )
    };
    let mut nodes = Vec::new();
    for (i, reserved) in reserves.iter_mut().enumerate() {
        drop(reserved.take());
        nodes.push(Some(start(i).await.unwrap()));
    }
    eprintln!("initial election");
    let first = leader(&nodes).await;
    let handle = nodes[first].as_ref().unwrap().handle();
    let receipt = handle.propose(&put("initial")).await.unwrap();
    applied(&nodes, 1).await;
    assert_eq!(
        handle.proposal_status(&receipt).await.unwrap(),
        DurableDirectoryStatus::Applied
    );
    for n in nodes.iter().flatten() {
        assert_eq!(
            n.handle()
                .query_local(&query("initial"))
                .await
                .unwrap()
                .unwrap()
                .version,
            1
        );
    }

    let lagging = (first + 1) % 3;
    let stale = nodes[lagging].as_ref().unwrap().handle();
    let remote = nodes[lagging].as_ref().unwrap().remote().clone();
    nodes[lagging].take().unwrap().close().await.unwrap();
    assert!(matches!(
        stale.propose(&put("closed")).await,
        Err(NetworkDirectoryError::Closed)
    ));
    assert_eq!(remote.callbacks.len(), 0);
    // > 4 MiB history forces several bounded AppendEntries frames when the
    // stopped replica returns. Compare complete endpoint data, not just counts.
    for i in 0..6 {
        let cmd = DirCmd::Upsert {
            node: format!("large-{i}"),
            endpoints: vec!["x".repeat(3800); 250],
        };
        handle.propose(&cmd).await.unwrap();
        applied(&nodes, i + 2).await;
    }
    nodes[lagging] = Some(start(lagging).await.unwrap());
    applied(&nodes, 7).await;
    for n in nodes.iter().flatten() {
        for i in 0..6 {
            let actual = n
                .handle()
                .query_local(&query(&format!("large-{i}")))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(actual.endpoints, vec!["x".repeat(3800); 250]);
            assert_eq!(actual.version, i + 2);
        }
    }
    // Lose the leader. Two real TCP replicas must elect and commit a new term.
    nodes[first].take().unwrap().close().await.unwrap();
    eprintln!("replacement election");
    let second = leader(&nodes).await;
    nodes[second]
        .as_ref()
        .unwrap()
        .handle()
        .propose(&put("after-election"))
        .await
        .unwrap();
    applied(&nodes, 8).await;
    nodes[first] = Some(start(first).await.unwrap());
    applied(&nodes, 8).await;
    // Stop every disk owner and recover without manually calling tick/step.
    for node in &mut nodes {
        node.take().unwrap().close().await.unwrap();
    }
    for (i, node) in nodes.iter_mut().enumerate() {
        *node = Some(start(i).await.unwrap());
    }
    applied(&nodes, 8).await;
    eprintln!("full restart election");
    let third = leader(&nodes).await;
    for n in nodes.iter().flatten() {
        assert_eq!(
            n.handle()
                .query_local(&query("after-election"))
                .await
                .unwrap()
                .unwrap()
                .version,
            8
        );
    }
    nodes[third]
        .as_ref()
        .unwrap()
        .handle()
        .propose(&put("after-restart"))
        .await
        .unwrap();
    applied(&nodes, 9).await;
    // A sole surviving leader can accept locally but cannot apply without quorum.
    for (i, node) in nodes.iter_mut().enumerate() {
        if i != third {
            node.take().unwrap().close().await.unwrap();
        }
    }
    let h = nodes[third].as_ref().unwrap().handle();
    let pending = h.propose(&put("minority")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        h.proposal_status(&pending).await.unwrap(),
        DurableDirectoryStatus::Pending
    );
    assert_eq!(h.query_local(&query("minority")).await.unwrap(), None);
    nodes[third].take().unwrap().close().await.unwrap();
}

#[derive(serde::Serialize)]
struct Wire<'a> {
    group: &'a str,
    rpc: RaftRpc,
}
fn wire(group: &str, rpc: RaftRpc) -> Bytes {
    bincode::serde::encode_to_vec(Wire { group, rpc }, bincode::config::standard())
        .unwrap()
        .into()
}
fn vote(candidate: &str, term: u64) -> RaftRpc {
    RaftRpc::RequestVote {
        term,
        candidate: candidate.into(),
        last_log_index: 0,
        last_log_term: 0,
    }
}
async fn raw(
    source: &Arc<RemoteActorSystem>,
    dest: &str,
    payload: Bytes,
    path: &str,
    hop: u8,
) -> ReplyPayload {
    raw_key(source, dest, payload, path, hop, RAFT_KEY).await
}
async fn raw_key(
    source: &Arc<RemoteActorSystem>,
    dest: &str,
    payload: Bytes,
    path: &str,
    hop: u8,
    key: &str,
) -> ReplyPayload {
    let sender = source
        .links_snapshot()
        .await
        .into_iter()
        .find(|p| p.0 == dest)
        .unwrap()
        .1;
    assert_eq!(
        sender.authentication(),
        parrot_remote::PeerAuthentication::MutualTls
    );
    let cid = source.callbacks.next_cid();
    let (tx, rx) = tokio::sync::oneshot::channel();
    source.callbacks.insert_on(cid, dest, tx, &sender).unwrap();

    let mut frame = Frame::ask(cid, path, key, payload, None);
    frame.header.hop_count = hop;
    sender.send(frame).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), rx).await;
    source.callbacks.remove(cid);
    result.unwrap().unwrap()
}
#[tokio::test]
async fn truncated_proposal_reports_conflict_through_the_mtls_owner() {
    use parrot_remote::roles::directory::network::DirectoryTiming;
    let names = ["receipt-owner.test", "receipt-peer.test"];
    let certs = credentials(&names);
    let dir = Scratch::new();
    let mut host = NetworkDirectory::start_with_timing(
        &dir.0,
        "receipt",
        RemoteConfig::tcp(names[0], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[0].clone(), []),
        vec![NodeAddr::tcp(names[1], "127.0.0.1:9".parse().unwrap())],
        Arc::new(Empty),
        DirectoryTiming {
            election_ms: 1000,
            heartbeat_ms: 150,
            rpc_timeout_ms: 1000,
        },
    )
    .await
    .unwrap();
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp(names[1], None).with_mtls(certs[1].clone(), []),
        Arc::new(Empty),
    )
    .unwrap();
    source.start().await.unwrap();
    source
        .connect(&NodeAddr::tcp(
            names[0],
            host.remote().local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let path = format!("parrot://{}{RAFT_PATH}", names[0]);
    let handle = host.handle();
    let term = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = handle.observation();
            if status.role == Role::Candidate {
                break status.term;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let response = raw(
        &source,
        names[0],
        wire(
            "receipt",
            RaftRpc::RequestVoteResp {
                term,
                vote_granted: true,
            },
        ),
        &path,
        0,
    )
    .await;
    assert!(matches!(response, ReplyPayload::Ok(_, _)), "{response:?}");
    handle.propose(&put("discarded-first")).await.unwrap();
    let receipt = handle.propose(&put("discarded-tail")).await.unwrap();
    assert_eq!(receipt.index(), 2);
    assert_eq!(
        handle.proposal_status(&receipt).await.unwrap(),
        DurableDirectoryStatus::Pending
    );
    let response = raw(
        &source,
        names[0],
        wire(
            "receipt",
            RaftRpc::AppendEntries {
                term: term + 1,
                leader: names[1].into(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: vec![parrot_remote::raft::LogEntry {
                    index: 1,
                    term: term + 1,
                    cmd: put("replacement").encode().unwrap(),
                }],
                leader_commit: 1,
            },
        ),
        &path,
        0,
    )
    .await;
    assert!(matches!(response, ReplyPayload::Ok(_, _)), "{response:?}");
    assert_eq!(
        handle.proposal_status(&receipt).await.unwrap(),
        DurableDirectoryStatus::Conflict
    );
    assert!(handle
        .query_local(&query("discarded-tail"))
        .await
        .unwrap()
        .is_none());
    assert!(handle
        .query_local(&query("replacement"))
        .await
        .unwrap()
        .is_some());
    host.close().await.unwrap();
    source.shutdown().await.unwrap();
    let recovered = parrot_remote::roles::directory::DurableDirectory::open(
        &dir.0,
        "receipt",
        names[0],
        vec![names[1].into()],
        Arc::new(parrot_remote::raft::ManualClock::new()),
    )
    .unwrap();
    assert_eq!(recovered.applied_index(), 1);
    assert!(recovered
        .query_local(&query("discarded-tail"))
        .unwrap()
        .is_none());
    assert!(recovered
        .query_local(&query("replacement"))
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn malformed_raft_metadata_is_rejected_without_closing_the_mtls_owner() {
    use parrot_remote::roles::directory::network::DirectoryTiming;
    let names = ["shape-target.test", "shape-member.test"];
    let certs = credentials(&names);
    let dir = Scratch::new();
    let mut host = NetworkDirectory::start_with_timing(
        &dir.0,
        "shape",
        RemoteConfig::tcp(names[0], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[0].clone(), []),
        vec![NodeAddr::tcp(names[1], "127.0.0.1:9".parse().unwrap())],
        Arc::new(Empty),
        DirectoryTiming {
            election_ms: 60_000,
            heartbeat_ms: 150,
            rpc_timeout_ms: 1000,
        },
    )
    .await
    .unwrap();
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp(names[1], None).with_mtls(certs[1].clone(), []),
        Arc::new(Empty),
    )
    .unwrap();
    source.start().await.unwrap();
    source
        .connect(&NodeAddr::tcp(
            names[0],
            host.remote().local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let path = format!("parrot://{}{RAFT_PATH}", names[0]);
    let connection = source
        .links_snapshot()
        .await
        .into_iter()
        .find(|(id, _, _)| id == names[0])
        .unwrap()
        .1;
    let before = std::fs::read(dir.0.join("raft.state")).unwrap();
    for rpc in [
        vote(names[1], 0),
        RaftRpc::AppendEntries {
            term: 0,
            leader: names[1].into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![],
            leader_commit: 0,
        },
        RaftRpc::AppendEntries {
            term: 50,
            leader: names[1].into(),
            prev_log_index: 0,
            prev_log_term: 1,
            entries: vec![],
            leader_commit: 0,
        },
        RaftRpc::AppendEntries {
            term: 50,
            leader: names[1].into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![parrot_remote::raft::LogEntry {
                index: 2,
                term: 50,
                cmd: put("valid-command").encode().unwrap(),
            }],
            leader_commit: 0,
        },
    ] {
        let reply = raw(&source, names[0], wire("shape", rpc), &path, 0).await;
        assert!(
            matches!(reply, ReplyPayload::Err(ErrCode::ProtocolViolation, _)),
            "{reply:?}"
        );
        let status = host.handle().observation();
        assert!(!status.closed);
        assert_eq!(status.term, 0);
        assert_eq!(std::fs::read(dir.0.join("raft.state")).unwrap(), before);
    }
    // No reconnect: the same source immediately submits a valid election.
    let reply = raw(
        &source,
        names[0],
        wire("shape", vote(names[1], 1)),
        &path,
        0,
    )
    .await;
    assert!(
        matches!(reply, ReplyPayload::Ok(bytes, key) if bytes.is_empty() && key == RAFT_ACK_KEY)
    );
    let after = source
        .links_snapshot()
        .await
        .into_iter()
        .find(|(id, _, _)| id == names[0])
        .unwrap()
        .1;
    assert!(
        connection.same_connection(&after),
        "invalid input must not require reconnecting"
    );
    host.close().await.unwrap();
    source.shutdown().await.unwrap();
    let recovered = parrot_remote::roles::directory::DurableDirectory::open(
        &dir.0,
        "shape",
        names[0],
        vec![names[1].into()],
        Arc::new(parrot_remote::raft::ManualClock::new()),
    )
    .unwrap();
    assert_eq!(recovered.current_term(), 1);
}

#[tokio::test]
async fn mtls_membership_group_identity_destination_and_strict_decode_are_enforced() {
    let names = ["target.test", "member.test", "outsider.test"];
    let certs = credentials(&names);
    let dir = Scratch::new();
    let mut host = NetworkDirectory::start(
        &dir.0,
        "secure",
        RemoteConfig::tcp(names[0], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[0].clone(), []),
        vec![NodeAddr::tcp(names[1], "127.0.0.1:9".parse().unwrap())],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let route = NodeAddr::tcp(names[0], host.remote().local_addr().unwrap());
    let path = format!("parrot://{}{RAFT_PATH}", names[0]);
    let mut sources = Vec::new();
    for i in 1..3 {
        let remote = RemoteActorSystem::new(
            RemoteConfig::tcp(names[i], Some("127.0.0.1:0".parse().unwrap()))
                .with_mtls(certs[i].clone(), []),
            Arc::new(Empty),
        )
        .unwrap();
        remote.start().await.unwrap();
        remote.connect(&route).await.unwrap();
        sources.push(remote);
    }
    let forbidden = raw(
        &sources[1],
        names[0],
        wire("secure", vote(names[1], 500)),
        &path,
        0,
    )
    .await;
    assert!(
        matches!(forbidden, ReplyPayload::Err(ErrCode::Forbidden, _)),
        "{forbidden:?}"
    );
    for (payload, target, hop) in [
        (wire("other", vote(names[1], 500)), path.clone(), 0),
        (wire("secure", vote(names[2], 500)), path.clone(), 0),
        (
            wire("secure", vote(names[1], 500)),
            format!("parrot://other.test{RAFT_PATH}"),
            0,
        ),
        (wire("secure", vote(names[1], 500)), path.clone(), 1),
        (Bytes::from_static(&[255; 20]), path.clone(), 0),
        (Bytes::from(vec![0; 4 * 1024 * 1024 + 1]), path.clone(), 0),
        (
            {
                let mut bytes = wire("secure", vote(names[1], 500)).to_vec();
                bytes.push(0);
                bytes.into()
            },
            path.clone(),
            0,
        ),
        (
            wire(
                "secure",
                RaftRpc::AppendEntries {
                    term: 500,
                    leader: names[1].into(),
                    prev_log_index: 0,
                    prev_log_term: 0,
                    entries: vec![parrot_remote::raft::LogEntry {
                        term: 500,
                        index: 1,
                        cmd: vec![255],
                    }],
                    leader_commit: 1,
                },
            ),
            path.clone(),
            0,
        ),
    ] {
        let reply = raw(&sources[0], names[0], payload, &target, hop).await;
        assert!(matches!(reply, ReplyPayload::Err(_, _)), "{reply:?}");
        assert!(host.handle().observation().term < 500);
        assert_eq!(host.handle().observation().applied_index, 0);
    }
    let accepted = raw(
        &sources[0],
        names[0],
        wire("secure", vote(names[1], 501)),
        &path,
        0,
    )
    .await;
    assert!(
        matches!(accepted, ReplyPayload::Ok(bytes, key) if bytes.is_empty() && key == RAFT_ACK_KEY)
    );
    // Network ACK follows the durable term/vote save; opening again observes it.
    host.close().await.unwrap();
    let saved = parrot_remote::roles::directory::DurableDirectory::open(
        &dir.0,
        "secure",
        names[0],
        vec![names[1].into()],
        Arc::new(parrot_remote::raft::ManualClock::new()),
    )
    .unwrap();
    assert_eq!(saved.current_term(), 501);
    drop(saved);
    for source in sources {
        source.shutdown().await.unwrap();
        assert_eq!(source.callbacks.len(), 0);
    }
}

#[tokio::test]
async fn insecure_hosts_invalid_membership_and_failed_start_do_not_leave_a_disk_owner() {
    let dir = Scratch::new();
    assert!(NetworkDirectory::start(
        &dir.0,
        "g",
        RemoteConfig::tcp("plain", Some("127.0.0.1:0".parse().unwrap())),
        vec![],
        Arc::new(Empty)
    )
    .await
    .is_err());
    let cert = credentials(&["solo.test"]).remove(0);
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = RemoteConfig::tcp("solo.test", Some(reserved.local_addr().unwrap()))
        .with_mtls(cert.clone(), []);
    assert!(
        NetworkDirectory::start(&dir.0, "g", config, vec![], Arc::new(Empty))
            .await
            .is_err()
    );
    let mut host = NetworkDirectory::start(
        &dir.0,
        "g",
        RemoteConfig::tcp("solo.test", Some("127.0.0.1:0".parse().unwrap())).with_mtls(cert, []),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let handle = host.handle();
    // External shutdown also fences the disk thread, even if handles survive.
    host.remote().shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !handle.observation().closed {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.close().await.unwrap();
    assert!(matches!(
        handle.query_local(&query("x")).await,
        Err(NetworkDirectoryError::Closed)
    ));
    drop(host);
    let reopened = parrot_remote::roles::directory::DurableDirectory::open(
        &dir.0,
        "g",
        "solo.test",
        vec![],
        Arc::new(parrot_remote::raft::ManualClock::new()),
    )
    .unwrap();
    drop(reopened);
}

#[tokio::test]
async fn drop_closes_retained_remote_and_handles_and_releases_storage() {
    let dir = Scratch::new();
    let host = NetworkDirectory::start(
        &dir.0,
        "g",
        RemoteConfig::mem("directory-drop-test"),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let handle = host.handle();
    let remote = host.remote().clone();
    drop(host);
    assert!(handle.observation().closed);
    assert!(matches!(
        handle.propose(&put("x")).await,
        Err(NetworkDirectoryError::Closed)
    ));
    assert!(remote.start().await.is_err());
    remote.shutdown().await.unwrap();
    let mut second = NetworkDirectory::start(
        &dir.0,
        "g",
        RemoteConfig::mem("directory-drop-test"),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    second.close().await.unwrap();
}

#[tokio::test]
async fn invalid_peer_routes_are_rejected_before_storage_is_opened() {
    use parrot_remote::roles::directory::network::DirectoryTiming;
    for timing in [
        DirectoryTiming {
            election_ms: 0,
            ..Default::default()
        },
        DirectoryTiming {
            heartbeat_ms: 2000,
            ..Default::default()
        },
        DirectoryTiming {
            rpc_timeout_ms: 0,
            ..Default::default()
        },
    ] {
        let dir = Scratch::new();
        assert!(NetworkDirectory::start_with_timing(
            &dir.0,
            "g",
            RemoteConfig::mem("timing-validation"),
            vec![],
            Arc::new(Empty),
            timing
        )
        .await
        .is_err());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    for peers in [
        vec![NodeAddr::mem("route-validation")],
        vec![NodeAddr::mem("peer"), NodeAddr::mem("peer")],
        vec![NodeAddr::tcp("peer", "127.0.0.1:9".parse().unwrap())],
        {
            let mut peer = NodeAddr::mem("peer");
            peer.identity_is_explicit = false;
            vec![peer]
        },
        (0..17)
            .map(|i| NodeAddr::mem(format!("peer-{i}")))
            .collect(),
    ] {
        let dir = Scratch::new();
        assert!(NetworkDirectory::start(
            &dir.0,
            "g",
            RemoteConfig::mem("route-validation"),
            peers,
            Arc::new(Empty)
        )
        .await
        .is_err());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn admission_is_bounded_and_cancelled_callers_do_not_leak_capacity() {
    let dir = Scratch::new();
    let mut host = NetworkDirectory::start(
        &dir.0,
        "g",
        RemoteConfig::mem("directory-admission"),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let handle = host.handle();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    // Poll a large simultaneous burst without yielding between admissions. The
    // serial disk writer cannot admit arbitrarily many syncs into its queue.
    let command = put("bounded");
    let results = futures::future::join_all((0..1000).map(|_| handle.propose(&command))).await;
    let accepted = results.iter().filter(|r| r.is_ok()).count();
    assert!(accepted > 0 && accepted < results.len());
    assert!(results
        .iter()
        .filter_map(|r| r.as_ref().err())
        .all(|e| matches!(e, NetworkDirectoryError::Capacity)));
    assert_eq!(
        handle
            .query_local(&query("bounded"))
            .await
            .unwrap()
            .unwrap()
            .version,
        accepted as u64
    );
    // Drop submitted futures after their first poll, then prove the owner still
    // accepts and applies a later request. No fabricated success on cancellation.
    for _ in 0..100 {
        let mut request = Box::pin(handle.propose(&command));
        let _ = futures::poll!(&mut request);
        drop(request);
    }
    let receipt = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match handle.propose(&put("after-cancel")).await {
                Ok(receipt) => break receipt,
                Err(NetworkDirectoryError::Capacity) => tokio::task::yield_now().await,
                Err(e) => panic!("{e}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        handle.proposal_status(&receipt).await.unwrap(),
        DurableDirectoryStatus::Applied
    );
    host.close().await.unwrap();
}

#[tokio::test]
async fn prepared_directory_defers_network_until_explicit_start_and_keeps_one_owner() {
    use parrot_remote::roles::directory::network::DirectoryTiming;
    let dir = Scratch::new();
    let mut host = NetworkDirectory::prepare_with_timing(
        &dir.0,
        "prepare",
        RemoteConfig::mem("prepared-directory"),
        vec![],
        Arc::new(Empty),
        DirectoryTiming::default(),
    )
    .await
    .unwrap();
    let client =
        RemoteActorSystem::new(RemoteConfig::mem("prepared-client"), Arc::new(Empty)).unwrap();
    client.start().await.unwrap();
    // Preparing storage must not allow a completed handshake. An immediate
    // unavailable error and a pending offer are both valid before start.
    assert!(!matches!(
        tokio::time::timeout(
            Duration::from_millis(50),
            client.connect(&NodeAddr::mem("prepared-directory"))
        )
        .await,
        Ok(Ok(()))
    ));
    host.start_transport().await.unwrap();
    host.start_transport().await.unwrap();
    client
        .connect(&NodeAddr::mem("prepared-directory"))
        .await
        .unwrap();
    host.close().await.unwrap();
    assert!(host.start_transport().await.is_err());
    assert!(host.handle().wait_closed().await.closed);
    client.shutdown().await.unwrap();
    let mut reopened = NetworkDirectory::start(
        &dir.0,
        "prepare",
        RemoteConfig::mem("prepared-directory"),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    reopened.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alternate_addresses_do_not_add_votes_and_unreachable_first_route_is_retried() {
    let names = ["route-a.test", "route-b.test"];
    let certs = credentials(&names);
    let a = Scratch::new();
    let b = Scratch::new();
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reserved.local_addr().unwrap();
    // B has no usable configured return route to A. Only A's second candidate
    // can establish their actual TLS connection; B replies on that connection.
    let b_host = NetworkDirectory::start(
        &b.0,
        "routes",
        RemoteConfig::tcp(names[1], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[1].clone(), []),
        vec![NodeAddr::tcp(names[0], "127.0.0.1:9".parse().unwrap())],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    drop(reserved);
    let a_host = NetworkDirectory::start(
        &a.0,
        "routes",
        RemoteConfig::tcp(names[0], Some(address)).with_mtls(certs[0].clone(), []),
        vec![
            NodeAddr::tcp(names[1], "127.0.0.1:9".parse().unwrap()),
            NodeAddr::tcp(names[1], b_host.remote().local_addr().unwrap()),
        ],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let mut nodes = vec![Some(a_host), Some(b_host)];
    let chosen = leader(&nodes).await;
    nodes[chosen]
        .as_ref()
        .unwrap()
        .handle()
        .propose(&put("route-entry"))
        .await
        .unwrap();
    applied(&nodes, 1).await;
    for n in nodes.iter().flatten() {
        assert_eq!(
            n.handle()
                .query_local(&query("route-entry"))
                .await
                .unwrap()
                .unwrap()
                .version,
            1
        );
    }
    for n in &mut nodes {
        n.take().unwrap().close().await.unwrap();
    }
    // The multi-address node's disk binding contains one peer, not two votes.
    let mut recovered = NetworkDirectory::start(
        &a.0,
        "routes",
        RemoteConfig::tcp(names[0], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[0].clone(), []),
        vec![NodeAddr::tcp(names[1], "127.0.0.1:9".parse().unwrap())],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    assert_eq!(recovered.handle().observation().applied_index, 1);
    recovered.close().await.unwrap();
}

#[derive(serde::Serialize)]
enum ClientOperation {
    Submit { id: String, command: DirCmd },
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_client_wire_write_is_persisted_and_retries_do_not_reapply() {
    let names = ["writer-target.test", "writer.test"];
    let certs = credentials(&names);
    let dir = Scratch::new();
    let mut host = NetworkDirectory::start(
        &dir.0,
        "client-wire",
        RemoteConfig::tcp(names[0], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[0].clone(), [names[1].to_owned()]),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp(names[1], None).with_mtls(certs[1].clone(), []),
        Arc::new(Empty),
    )
    .unwrap();
    source.start().await.unwrap();
    source
        .connect(&NodeAddr::tcp(
            names[0],
            host.remote().local_addr().unwrap(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        while host.handle().observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let bytes = bincode::serde::encode_to_vec(
        (
            "client-wire",
            ClientOperation::Submit {
                id: "same-request".into(),
                command: put("registered"),
            },
        ),
        bincode::config::standard(),
    )
    .unwrap();
    for _ in 0..2 {
        let result = raw_key(
            &source,
            names[0],
            bytes.clone().into(),
            &format!("parrot://{}/_directory/client/v1", names[0]),
            0,
            "parrot:directory:client#v1",
        )
        .await;
        assert!(
            matches!(&result,ReplyPayload::Ok(_,key) if key=="parrot:directory:client-result#v1"),
            "{result:?}"
        );
        let entry = host
            .handle()
            .query_local(&query("registered"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.version, 1);
        assert_eq!(entry.endpoints, ["tcp://registered:7"]);
    }
    assert_eq!(host.handle().observation().applied_index, 1);
    source.shutdown().await.unwrap();
    host.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn snapshot_capacity_over_mtls_keeps_reads_deduplication_and_smaller_writes_available() {
    use parrot_remote::roles::directory::{
        client::DirectoryClient, directory_command_digest, DirectoryRequestStatus as Status,
    };
    let names = ["capacity-owner.test", "capacity-client.test"];
    let certs = credentials(&names);
    let dir = Scratch::new();
    let mut host = NetworkDirectory::start(
        &dir.0,
        "capacity",
        RemoteConfig::tcp(names[0], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[0].clone(), [names[1].to_owned()]),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp(names[1], None).with_mtls(certs[1].clone(), []),
        Arc::new(Empty),
    )
    .unwrap();
    source.start().await.unwrap();
    let route = NodeAddr::tcp(names[0], host.remote().local_addr().unwrap());
    source.connect(&route).await.unwrap();
    let connection = source
        .links_snapshot()
        .await
        .into_iter()
        .find(|x| x.0 == names[0])
        .unwrap()
        .1;
    let client = DirectoryClient::new(source.clone(), "capacity").unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        while host.handle().observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let limit = Duration::from_secs(10);
    let large = DirCmd::Upsert {
        node: "large".into(),
        endpoints: vec!["x".repeat(4096); 240],
    };
    let digest = directory_command_digest(&large).unwrap();
    let mut accepted = 0;
    let mut rejected = None;
    for index in 0..20 {
        let id = format!("large-{index}");
        let before = std::fs::read(dir.0.join("raft.state")).unwrap();
        match client.submit(&route, &id, &large, limit).await {
            Ok(Status::Applied { revision, .. }) => {
                accepted += 1;
                assert_eq!(revision, accepted);
            }
            Err(error) => {
                assert!(
                    matches!(error, NetworkDirectoryError::Capacity),
                    "{error:?}"
                );
                assert!(!host.handle().observation().closed);
                assert_eq!(std::fs::read(dir.0.join("raft.state")).unwrap(), before);
                rejected = Some(id);
                break;
            }
            result => panic!("unexpected result: {result:?}"),
        }
    }
    let rejected = rejected.expect("cumulative snapshot bound must be enforced");
    assert!(accepted > 0);
    assert_eq!(
        client
            .status(&route, &rejected, digest, limit)
            .await
            .unwrap(),
        Status::Unknown
    );
    assert!(matches!(
        client
            .submit(&route, "large-0", &large, limit)
            .await
            .unwrap(),
        Status::Applied { revision: 1, .. }
    ));
    // A rejected request ID was never admitted and can carry a smaller command.
    assert!(
        matches!(client.submit(&route, &rejected, &put("small"), limit).await.unwrap(), Status::Applied { revision, .. } if revision == accepted + 1)
    );
    let read = client
        .query_linearizable(&route, &query("small"), limit)
        .await
        .unwrap();
    assert!(read.entry.is_some());
    let after = source
        .links_snapshot()
        .await
        .into_iter()
        .find(|x| x.0 == names[0])
        .unwrap()
        .1;
    assert!(connection.same_connection(&after));
    host.close().await.unwrap();
    source.shutdown().await.unwrap();
    let recovered = parrot_remote::roles::directory::DurableDirectory::open(
        &dir.0,
        "capacity",
        names[0],
        vec![],
        Arc::new(parrot_remote::raft::ManualClock::new()),
    )
    .unwrap();
    assert_eq!(recovered.state_local().unwrap().version, accepted + 1);
    assert!(recovered.query_local(&query("small")).unwrap().is_some());
    assert!(
        matches!(recovered.request_status(names[1], &rejected, directory_command_digest(&put("small")).unwrap()).unwrap(), Status::Applied { revision, .. } if revision == accepted + 1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn directory_client_enforces_admin_identity_namespace_group_and_direct_wire_boundaries() {
    use parrot_remote::roles::directory::{
        client::{DirectoryClient, CLIENT_KEY, CLIENT_PATH, CLIENT_REPLY_KEY},
        directory_command_digest, DirectoryRequestStatus as Status,
    };
    let names = [
        "client-target.test",
        "admin-a.test",
        "admin-b.test",
        "guest.test",
    ];
    let certs = credentials(&names);
    let dir = Scratch::new();
    let mut host = NetworkDirectory::start(
        &dir.0,
        "client-auth",
        RemoteConfig::tcp(names[0], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[0].clone(), [names[1].to_owned(), names[2].to_owned()]),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let route = NodeAddr::tcp(names[0], host.remote().local_addr().unwrap());
    let path = format!("parrot://{}{CLIENT_PATH}", names[0]);
    let mut sources = vec![];
    for i in 1..4 {
        let source = RemoteActorSystem::new(
            RemoteConfig::tcp(names[i], None).with_mtls(certs[i].clone(), []),
            Arc::new(Empty),
        )
        .unwrap();
        source.start().await.unwrap();
        source.connect(&route).await.unwrap();
        sources.push(source);
    }
    let clients: Vec<_> = sources
        .iter()
        .map(|s| DirectoryClient::new(s.clone(), "client-auth").unwrap())
        .collect();
    tokio::time::timeout(Duration::from_secs(8), async {
        while host.handle().observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let limit = Duration::from_secs(3);
    let command = put("same");
    let digest = directory_command_digest(&command).unwrap();
    assert!(matches!(
        clients[2].submit(&route, "id", &command, limit).await,
        Err(NetworkDirectoryError::Forbidden)
    ));
    assert!(matches!(
        clients[2].status(&route, "id", digest, limit).await,
        Err(NetworkDirectoryError::Forbidden)
    ));
    let first = clients[0]
        .submit_and_wait(&route, "id", &command, limit)
        .await
        .unwrap();
    assert!(matches!(
        first,
        Status::Applied {
            revision: 1,
            index: 1,
            ..
        }
    ));
    assert_eq!(
        clients[0]
            .submit(&route, "id", &command, limit)
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        clients[0]
            .submit(&route, "id", &put("conflict"), limit)
            .await
            .unwrap(),
        Status::Conflict
    );
    assert_eq!(
        clients[1]
            .status(&route, "id", digest, limit)
            .await
            .unwrap(),
        Status::Unknown
    );
    assert!(matches!(
        clients[1]
            .submit_and_wait(&route, "id", &put("other-client"), limit)
            .await
            .unwrap(),
        Status::Applied { revision: 2, .. }
    ));
    let valid = bincode::serde::encode_to_vec(
        (
            "client-auth",
            ClientOperation::Submit {
                id: "bad-wire".into(),
                command: put("must-not-apply"),
            },
        ),
        bincode::config::standard(),
    )
    .unwrap();
    for (payload, target, hop) in [
        (
            bincode::serde::encode_to_vec(
                (
                    "wrong-group",
                    ClientOperation::Submit {
                        id: "bad-wire".into(),
                        command: put("must-not-apply"),
                    },
                ),
                bincode::config::standard(),
            )
            .unwrap(),
            path.clone(),
            0,
        ),
        (valid.clone(), format!("parrot://other{CLIENT_PATH}"), 0),
        (valid.clone(), path.clone(), 1),
        (
            {
                let mut b = valid.clone();
                b.push(0);
                b
            },
            path.clone(),
            0,
        ),
        (vec![255; 64], path.clone(), 0),
        (vec![0; 1024 * 1024 + 2049], path.clone(), 0),
    ] {
        let reply = raw_key(
            &sources[0],
            names[0],
            payload.into(),
            &target,
            hop,
            CLIENT_KEY,
        )
        .await;
        match reply {
            ReplyPayload::Ok(bytes, key) => {
                assert_eq!(key, CLIENT_REPLY_KEY);
                let (result, used): (Result<Status, NetworkDirectoryError>, usize) =
                    bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
                assert_eq!(used, bytes.len());
                assert!(result.is_err());
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(host.handle().observation().applied_index, 2);
    assert!(host
        .handle()
        .query_local(&query("must-not-apply"))
        .await
        .unwrap()
        .is_none());
    assert_eq!(sources[0].callbacks.len(), 0);
    for source in sources {
        source.shutdown().await.unwrap();
    }
    host.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_deadline_zero_is_rejected_before_sending() {
    assert_client_deadline(Duration::ZERO).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_deadline_overflow_is_rejected_before_sending() {
    assert_client_deadline(Duration::MAX).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_deadline_exhausted_before_wire_admission_cannot_send() {
    assert_client_deadline(Duration::from_nanos(1)).await;
}

async fn assert_client_deadline(timeout: Duration) {
    use parrot_remote::roles::directory::{client::DirectoryClient, directory_command_digest};
    let names = ["deadline-owner.test", "deadline-client.test"];
    let certs = credentials(&names);
    let dir = Scratch::new();
    let mut host = NetworkDirectory::start(
        &dir.0,
        "deadlines",
        RemoteConfig::tcp(names[0], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[0].clone(), [names[1].to_owned()]),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp(names[1], None).with_mtls(certs[1].clone(), []),
        Arc::new(Empty),
    )
    .unwrap();
    source.start().await.unwrap();
    let route = NodeAddr::tcp(names[0], host.remote().local_addr().unwrap());
    source.connect(&route).await.unwrap();
    let client = DirectoryClient::new(source.clone(), "deadlines").unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        while host.handle().observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let command = DirCmd::Upsert {
        node: "must-not-apply".into(),
        endpoints: vec!["x".repeat(4096); 240],
    };
    let digest = directory_command_digest(&command).unwrap();
    for operation in 0..3 {
        let before = source.callbacks.next_cid();
        let outcome = match operation {
            0 => client.submit(&route, "expired", &command, timeout).await,
            1 => {
                client
                    .submit_and_wait(&route, "expired", &command, timeout)
                    .await
            }
            _ => client.status(&route, "expired", digest, timeout).await,
        };
        if timeout == Duration::from_nanos(1) {
            assert!(
                matches!(outcome, Err(NetworkDirectoryError::Timeout)),
                "{outcome:?}"
            );
        } else {
            assert!(
                matches!(outcome, Err(NetworkDirectoryError::Invalid(_))),
                "{outcome:?}"
            );
        }
        assert_eq!(
            source.callbacks.next_cid(),
            before + 1,
            "deadline failure allocated a wire callback for operation {operation}"
        );
        assert_eq!(source.callbacks.len(), 0);
    }
    assert_eq!(host.handle().observation().applied_index, 0);
    assert!(host
        .handle()
        .query_local(&query("must-not-apply"))
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        client
            .submit_and_wait(&route, "valid", &put("valid"), Duration::from_secs(5))
            .await
            .unwrap(),
        parrot_remote::roles::directory::DirectoryRequestStatus::Applied { revision: 1, .. }
    ));
    host.close().await.unwrap();
    source.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_timeout_keeps_one_pending_request_and_recovery_returns_its_original_result() {
    use parrot_remote::roles::directory::{
        client::DirectoryClient, directory_command_digest, DirectoryRequestStatus as Status,
    };
    let names = [
        "client-recovery-a",
        "client-recovery-b",
        "client-recovery-c",
    ];
    let dirs: Vec<_> = (0..3).map(|_| Scratch::new()).collect();
    let start = |i: usize| {
        NetworkDirectory::start_with_timing(
            &dirs[i].0,
            "client-recovery",
            RemoteConfig::mem(names[i]),
            names
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, n)| NodeAddr::mem(*n))
                .collect(),
            Arc::new(Empty),
            parrot_remote::roles::directory::network::DirectoryTiming {
                election_ms: 500,
                heartbeat_ms: 50,
                rpc_timeout_ms: 500,
            },
        )
    };
    let mut nodes = vec![];
    for i in 0..3 {
        nodes.push(Some(start(i).await.unwrap()));
    }
    let lead = leader(&nodes).await;
    let source =
        RemoteActorSystem::new(RemoteConfig::mem("client-recovery-writer"), Arc::new(Empty))
            .unwrap();
    source.start().await.unwrap();
    let client = DirectoryClient::new(source.clone(), "client-recovery").unwrap();
    let route = NodeAddr::mem(names[lead]);
    let command = put("pending");
    for (i, n) in nodes.iter_mut().enumerate() {
        if i != lead {
            n.take().unwrap().close().await.unwrap();
        }
    }
    assert!(matches!(
        client
            .submit(&route, "stable-id", &command, Duration::from_secs(2))
            .await
            .unwrap(),
        Status::Pending { index: 1, .. }
    ));
    assert!(matches!(
        client
            .submit_and_wait(&route, "stable-id", &command, Duration::from_millis(100))
            .await,
        Err(NetworkDirectoryError::Timeout)
    ));
    assert_eq!(source.callbacks.len(), 0);
    assert!(nodes[lead]
        .as_ref()
        .unwrap()
        .handle()
        .query_local(&query("pending"))
        .await
        .unwrap()
        .is_none());
    for (i, n) in nodes.iter_mut().enumerate() {
        if i != lead {
            *n = Some(start(i).await.unwrap());
        }
    }
    // Quorum restoration can also elect a new leader. An uncommitted request
    // is allowed to be replaced; retry the same ID on the elected member and
    // require one business application, not a particular uncommitted log slot.
    let recovered_leader = leader(&nodes).await;
    let route = NodeAddr::mem(names[recovered_leader]);
    let committed = client
        .submit_and_wait(&route, "stable-id", &command, Duration::from_secs(10))
        .await
        .unwrap();
    let committed_index = nodes[recovered_leader]
        .as_ref()
        .unwrap()
        .handle()
        .observation()
        .applied_index;
    applied(&nodes, committed_index).await;
    let original = client
        .status(
            &route,
            "stable-id",
            directory_command_digest(&command).unwrap(),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(matches!(original, Status::Applied { revision: 1, .. }));
    assert_eq!(original, committed);
    for n in &mut nodes {
        n.take().unwrap().close().await.unwrap();
    }
    for (i, n) in nodes.iter_mut().enumerate() {
        *n = Some(start(i).await.unwrap());
    }
    let new_lead = leader(&nodes).await;
    let route = NodeAddr::mem(names[new_lead]);
    source.connect(&route).await.unwrap();
    assert_eq!(
        client
            .submit_and_wait(&route, "stable-id", &command, Duration::from_secs(2))
            .await
            .unwrap(),
        original
    );
    for n in nodes.iter().flatten() {
        assert_eq!(
            n.handle()
                .query_local(&query("pending"))
                .await
                .unwrap()
                .unwrap()
                .version,
            1
        );
        assert_eq!(n.handle().observation().applied_index, committed_index);
    }
    for n in nodes.iter_mut().flatten() {
        n.close().await.unwrap();
    }
    source.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn linear_read_wire_requires_fresh_quorum_without_mutating_business_revision() {
    let names = ["read-target.test", "read-writer.test"];
    let certs = credentials(&names);
    let dir = Scratch::new();
    let mut host = NetworkDirectory::start(
        &dir.0,
        "read-wire",
        RemoteConfig::tcp(names[0], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[0].clone(), [names[1].to_owned()]),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp(names[1], None).with_mtls(certs[1].clone(), []),
        Arc::new(Empty),
    )
    .unwrap();
    source.start().await.unwrap();
    source
        .connect(&NodeAddr::tcp(
            names[0],
            host.remote().local_addr().unwrap(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        while host.handle().observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    host.handle().propose(&put("present")).await.unwrap();
    let payload = bincode::serde::encode_to_vec(
        ("read-wire", query("present"), 2000u64),
        bincode::config::standard(),
    )
    .unwrap();
    for index in [2, 3] {
        let result = raw_key(
            &source,
            names[0],
            payload.clone().into(),
            &format!("parrot://{}/_directory/read/v1", names[0]),
            0,
            "parrot:directory:read#v1",
        )
        .await;
        let ReplyPayload::Ok(bytes, key) = result else {
            panic!("{result:?}")
        };
        assert_eq!(key, "parrot:directory:read-result#v1");
        // Independent structural decoder: exact entry, fresh log barrier, applied
        // index and term. No production read type needed by the old-binary negative.
        type View = (
            Option<parrot_remote::roles::directory::DirEntry>,
            u64,
            u64,
            u64,
        );
        let (result, used): (Result<View, NetworkDirectoryError>, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(used, bytes.len());
        let (entry, barrier, applied, term) = result.unwrap();
        assert_eq!(barrier, index);
        assert_eq!(applied, index);
        assert!(term > 0);
        assert_eq!(entry.unwrap().version, 1);
    }
    source.shutdown().await.unwrap();
    host.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn minority_linear_reads_timeout_and_bounded_cancelled_waiters_release_admission() {
    use parrot_remote::roles::directory::client::DirectoryClient;
    let names = ["read-a", "read-b", "read-c"];
    let dirs: Vec<_> = (0..3).map(|_| Scratch::new()).collect();
    let start = |i: usize| {
        NetworkDirectory::start_with_timing(
            &dirs[i].0,
            "read-minority",
            RemoteConfig::mem(names[i]),
            names
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, n)| NodeAddr::mem(*n))
                .collect(),
            Arc::new(Empty),
            parrot_remote::roles::directory::network::DirectoryTiming {
                election_ms: 500,
                heartbeat_ms: 50,
                rpc_timeout_ms: 500,
            },
        )
    };
    let mut nodes = Vec::new();
    for i in 0..3 {
        nodes.push(Some(start(i).await.unwrap()));
    }
    let selected = leader(&nodes).await;
    let source = RemoteActorSystem::new(RemoteConfig::mem("read-client"), Arc::new(Empty)).unwrap();
    source.start().await.unwrap();
    let client = DirectoryClient::new(source.clone(), "read-minority").unwrap();
    let route = NodeAddr::mem(names[selected]);
    let handle = nodes[selected].as_ref().unwrap().handle();
    client
        .submit_and_wait(
            &route,
            "before-read",
            &put("worker"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    let present = client
        .query_linearizable(&route, &query("worker"), Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(present.entry.unwrap().version, 1);
    assert!(present.barrier_index >= 2);
    let follower = (selected + 1) % 3;
    assert!(matches!(
        client
            .query_linearizable(
                &NodeAddr::mem(names[follower]),
                &query("worker"),
                Duration::from_secs(2)
            )
            .await,
        Err(NetworkDirectoryError::NotLeader(_))
    ));
    for (i, n) in nodes.iter_mut().enumerate() {
        if i != selected {
            n.take().unwrap().close().await.unwrap();
        }
    }
    assert_eq!(handle.observation().role, Role::Leader);
    assert!(handle
        .query_local(&query("worker"))
        .await
        .unwrap()
        .is_some());
    assert!(matches!(
        client
            .query_linearizable(&route, &query("worker"), Duration::from_millis(80))
            .await,
        Err(NetworkDirectoryError::Timeout)
    ));
    assert_eq!(source.callbacks.len(), 0);
    // Polling creates actual in-flight reads without creating uncontrolled tasks.
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle.pending_linear_reads() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let query = query("worker");
    let mut waiters = Vec::new();
    for _ in 0..64 {
        let mut future = Box::pin(handle.query_linearizable(&query, Duration::from_secs(10)));
        assert!(futures::poll!(&mut future).is_pending());
        waiters.push(future);
    }
    assert!(matches!(
        handle
            .query_linearizable(&query, Duration::from_millis(50))
            .await,
        Err(NetworkDirectoryError::Capacity)
    ));
    assert_eq!(handle.pending_linear_reads(), 64);
    drop(waiters);
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.pending_linear_reads() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match handle
                .query_linearizable(&query, Duration::from_millis(40))
                .await
            {
                Err(NetworkDirectoryError::Capacity) => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(NetworkDirectoryError::Timeout) => break,
                other => panic!("minority must not succeed: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    for (i, n) in nodes.iter_mut().enumerate() {
        if i != selected {
            *n = Some(start(i).await.unwrap());
        }
    }
    // A completed read after recovery must contain the committed write, regardless
    // of the legitimate current leader or the number of cancelled barrier entries.
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let i = leader(&nodes).await;
            match client
                .query_linearizable(&NodeAddr::mem(names[i]), &query, Duration::from_secs(2))
                .await
            {
                Ok(read) => {
                    assert_eq!(read.entry.unwrap().version, 1);
                    break;
                }
                Err(NetworkDirectoryError::NotLeader(_) | NetworkDirectoryError::Timeout) => {}
                other => panic!("{other:?}"),
            }
        }
    })
    .await
    .unwrap();
    for n in nodes.iter_mut().flatten() {
        n.close().await.unwrap();
    }
    source.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_linear_read_rejects_unauthorized_or_malformed_calls_and_decodes_large_results() {
    use parrot_remote::roles::directory::client::{
        DirectoryClient, READ_KEY, READ_PATH, READ_REPLY_KEY,
    };
    let names = ["read-auth.test", "read-admin.test", "read-guest.test"];
    let certs = credentials(&names);
    let dir = Scratch::new();
    let mut host = NetworkDirectory::start(
        &dir.0,
        "read-auth",
        RemoteConfig::tcp(names[0], Some("127.0.0.1:0".parse().unwrap()))
            .with_mtls(certs[0].clone(), [names[1].to_owned()]),
        vec![],
        Arc::new(Empty),
    )
    .await
    .unwrap();
    let route = NodeAddr::tcp(names[0], host.remote().local_addr().unwrap());
    let path = format!("parrot://{}{READ_PATH}", names[0]);
    let mut sources = Vec::new();
    for i in 1..3 {
        let s = RemoteActorSystem::new(
            RemoteConfig::tcp(names[i], None).with_mtls(certs[i].clone(), []),
            Arc::new(Empty),
        )
        .unwrap();
        s.start().await.unwrap();
        s.connect(&route).await.unwrap();
        sources.push(s);
    }
    tokio::time::timeout(Duration::from_secs(8), async {
        while host.handle().observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let client = DirectoryClient::new(sources[0].clone(), "read-auth").unwrap();
    let guest = DirectoryClient::new(sources[1].clone(), "read-auth").unwrap();
    assert!(matches!(
        guest
            .query_linearizable(&route, &query("worker"), Duration::from_secs(2))
            .await,
        Err(NetworkDirectoryError::Forbidden)
    ));
    let value = DirCmd::Upsert {
        node: "worker".into(),
        endpoints: vec!["x".repeat(3000); 128],
    };
    client
        .submit_and_wait(&route, "large", &value, Duration::from_secs(5))
        .await
        .unwrap();
    let expected = host.handle().query_local(&query("worker")).await.unwrap();
    let read = client
        .query_linearizable(&route, &query("worker"), Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(read.entry, expected);
    assert_eq!(read.barrier_index, 2);
    let bytes = |group: &str, q: DirQuery, ms: u64| {
        bincode::serde::encode_to_vec((group, q, ms), bincode::config::standard()).unwrap()
    };
    for (payload, target, hop) in [
        (bytes("wrong", query("worker"), 500), path.clone(), 0),
        (
            bytes("read-auth", query("worker"), 500),
            format!("parrot://wrong{READ_PATH}"),
            0,
        ),
        (bytes("read-auth", query("worker"), 500), path.clone(), 1),
        (bytes("read-auth", query("worker"), 0), path.clone(), 0),
        (bytes("read-auth", query("worker"), 60001), path.clone(), 0),
        (
            bytes("read-auth", query(&"x".repeat(4097)), 500),
            path.clone(),
            0,
        ),
        (
            {
                let mut v = bytes("read-auth", query("worker"), 500);
                v.push(0);
                v
            },
            path.clone(),
            0,
        ),
        (vec![0; 8193], path.clone(), 0),
    ] {
        let response = raw_key(
            &sources[0],
            names[0],
            payload.into(),
            &target,
            hop,
            READ_KEY,
        )
        .await;
        let ReplyPayload::Ok(bytes, key) = response else {
            panic!("{response:?}")
        };
        assert_eq!(key, READ_REPLY_KEY);
        let (result, used): (
            Result<parrot_remote::roles::directory::DirectoryRead, NetworkDirectoryError>,
            usize,
        ) = bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(used, bytes.len());
        assert!(result.is_err());
    }
    assert_eq!(host.handle().observation().applied_index, 2);
    assert_eq!(
        host.handle().query_local(&query("worker")).await.unwrap(),
        expected
    );
    assert!(client
        .query_linearizable(&route, &query("missing"), Duration::from_secs(2))
        .await
        .unwrap()
        .entry
        .is_none());
    for s in sources {
        s.shutdown().await.unwrap();
    }
    host.close().await.unwrap();
}
