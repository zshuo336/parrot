use parrot_remote::{
    raft::{LogEntry, ManualClock, RaftRpc, Role},
    roles::directory::{
        network::{DirectoryTiming, NetworkDirectory, NetworkDirectoryError},
        ownership::{
            OwnershipClient, OwnershipCommand as Command, OwnershipOutcome as Outcome,
            OwnershipRecord as Record, OwnershipRejection as Rejection, OwnershipStatus as Status,
        },
        DirCmd, DurableDirectory,
    },
    tls::TlsConfig,
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{net::TcpListener, sync::Arc, time::Duration};

fn claim(expected: Option<u64>) -> Command {
    Command::Claim {
        resource: "/user/entity-alice".into(),
        expected,
    }
}
fn release(expected: u64) -> Command {
    Command::Release {
        resource: "/user/entity-alice".into(),
        expected,
    }
}
fn observe() -> Command {
    Command::Observe {
        resource: "/user/entity-alice".into(),
    }
}
fn changed(status: &Status, holder: Option<&str>) -> Record {
    let Status::Applied {
        index,
        outcome: Outcome::Changed(record),
        ..
    } = status
    else {
        panic!("expected committed change, got {status:?}");
    };
    assert_eq!(record.resource, "/user/entity-alice");
    assert_eq!(record.holder.as_deref(), holder);
    assert_eq!(record.generation, *index);
    record.clone()
}
fn rejected(status: &Status, reason: Rejection, expected: &Record) {
    assert!(
        matches!(status, Status::Applied { outcome: Outcome::Rejected { reason: actual, current: Some(record) }, .. } if *actual == reason && record == expected),
        "{status:?}"
    );
}
fn open(path: &std::path::Path, clock: Arc<ManualClock>) -> DurableDirectory {
    DurableDirectory::open(path, "ownership", "a", vec![], clock).unwrap()
}
fn elect(owner: &mut DurableDirectory, clock: &ManualClock) {
    clock.advance(3000);
    owner.tick().unwrap();
    assert_eq!(owner.role(), Role::Leader);
}

#[test]
fn conditional_changes_tombstones_and_original_results_survive_reopen() {
    let path = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut owner = open(path.path(), clock.clone());
    elect(&mut owner, &clock);
    let first = owner
        .submit_ownership("alice", "first", &claim(None))
        .unwrap();
    let alice = changed(&first, Some("alice"));
    assert_eq!(alice.generation, 1);
    let original_rejection = owner
        .submit_ownership("bob", "compete", &claim(None))
        .unwrap();
    rejected(&original_rejection, Rejection::GenerationConflict, &alice);
    let bob = changed(
        &owner
            .submit_ownership("bob", "replace", &claim(Some(alice.generation)))
            .unwrap(),
        Some("bob"),
    );
    assert!(bob.generation > alice.generation);
    rejected(
        &owner
            .submit_ownership("alice", "stale-release", &release(alice.generation))
            .unwrap(),
        Rejection::GenerationConflict,
        &bob,
    );
    rejected(
        &owner
            .submit_ownership("alice", "foreign-release", &release(bob.generation))
            .unwrap(),
        Rejection::NotHolder,
        &bob,
    );
    let retired = changed(
        &owner
            .submit_ownership("bob", "release", &release(bob.generation))
            .unwrap(),
        None,
    );
    assert!(retired.generation > bob.generation);
    rejected(
        &owner
            .submit_ownership("alice", "aba", &claim(None))
            .unwrap(),
        Rejection::GenerationConflict,
        &retired,
    );
    let last = changed(
        &owner
            .submit_ownership("alice", "new", &claim(Some(retired.generation)))
            .unwrap(),
        Some("alice"),
    );
    assert!(last.generation > retired.generation);
    assert_eq!(
        owner
            .submit_ownership("alice", "first", &claim(None))
            .unwrap(),
        first
    );
    assert_eq!(
        owner
            .submit_ownership("alice", "first", &claim(Some(last.generation)))
            .unwrap(),
        Status::Conflict
    );
    assert_eq!(owner.state_local().unwrap().version, 0);
    let before = std::fs::read(path.path().join("raft.state")).unwrap();
    drop(owner);
    let mut owner = open(path.path(), clock.clone());
    assert_eq!(owner.role(), Role::Follower);
    assert_eq!(
        owner
            .submit_ownership("bob", "compete", &claim(None))
            .unwrap(),
        original_rejection
    );
    assert_eq!(
        owner
            .ownership_status("alice", "first", claim(None).digest().unwrap())
            .unwrap(),
        first
    );
    assert!(owner
        .submit_ownership("alice", "fresh", &observe())
        .is_err());
    assert_eq!(
        std::fs::read(path.path().join("raft.state")).unwrap(),
        before
    );
    elect(&mut owner, &clock);
    let observed = owner
        .submit_ownership("alice", "fresh", &observe())
        .unwrap();
    assert!(
        matches!(&observed, Status::Applied { outcome: Outcome::Observed(Some(record)), .. } if record == &last)
    );
    let next = changed(
        &owner
            .submit_ownership("bob", "later", &claim(Some(last.generation)))
            .unwrap(),
        Some("bob"),
    );
    assert!(next.generation > last.generation);
    assert_eq!(
        owner
            .submit_ownership("alice", "fresh", &observe())
            .unwrap(),
        observed
    );
    assert!(
        matches!(owner.submit_ownership("alice", "fresh-again", &observe()).unwrap(), Status::Applied { outcome: Outcome::Observed(Some(record)), .. } if record == next)
    );
}

#[test]
fn directory_and_ownership_id_namespaces_and_revisions_are_separate() {
    let path = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut owner = open(path.path(), clock.clone());
    elect(&mut owner, &clock);
    let directory = owner
        .submit_request(
            "alice",
            "same",
            &DirCmd::Upsert {
                node: "worker".into(),
                endpoints: vec!["mem://worker".into()],
            },
        )
        .unwrap();
    let authority = owner
        .submit_ownership("alice", "same", &claim(None))
        .unwrap();
    changed(&authority, Some("alice"));
    assert_eq!(owner.state_local().unwrap().version, 1);
    assert_eq!(owner.applied_index(), 2);
    drop(owner);
    let owner = open(path.path(), clock);
    assert_eq!(owner.state_local().unwrap().version, 1);
    assert_eq!(
        owner
            .ownership_status("alice", "same", claim(None).digest().unwrap())
            .unwrap(),
        authority
    );
    assert_eq!(
        owner
            .request_status(
                "alice",
                "same",
                parrot_remote::roles::directory::directory_command_digest(&DirCmd::Upsert {
                    node: "worker".into(),
                    endpoints: vec!["mem://worker".into()]
                })
                .unwrap()
            )
            .unwrap(),
        directory
    );
}

fn encoded(client: &str, id: &str, command: &Command) -> Vec<u8> {
    let mut bytes = b"\xffOWN\x01".to_vec();
    bincode::serde::encode_into_std_write(
        (client, id, command),
        &mut bytes,
        bincode::config::standard(),
    )
    .unwrap();
    bytes
}
fn append(entries: Vec<LogEntry>, commit: u64) -> RaftRpc {
    RaftRpc::AppendEntries {
        term: 2,
        leader: "a".into(),
        prev_log_index: 0,
        prev_log_term: 0,
        entries,
        leader_commit: commit,
    }
}
#[test]
fn replicated_duplicate_and_conflicting_ids_keep_first_committed_outcome() {
    let path = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let open = || {
        DurableDirectory::open(
            path.path(),
            "ownership",
            "b",
            vec!["a".into()],
            clock.clone(),
        )
        .unwrap()
    };
    let mut owner = open();
    let entries = [claim(None), claim(None), release(1)]
        .into_iter()
        .enumerate()
        .map(|(index, cmd)| LogEntry {
            index: index as u64 + 1,
            term: 2,
            cmd: encoded("alice", "same", &cmd),
        })
        .collect();
    owner.step("a".into(), append(entries, 3)).unwrap();
    let first = owner
        .ownership_status("alice", "same", claim(None).digest().unwrap())
        .unwrap();
    assert_eq!(changed(&first, Some("alice")).generation, 1);
    assert_eq!(owner.applied_index(), 3);
    assert_eq!(
        owner
            .ownership_status("alice", "same", release(1).digest().unwrap())
            .unwrap(),
        Status::Conflict
    );
    drop(owner);
    let owner = open();
    assert_eq!(
        owner
            .ownership_status("alice", "same", claim(None).digest().unwrap())
            .unwrap(),
        first
    );
}
#[test]
fn quorum_and_fresh_current_term_commit_are_required_for_changes_and_absence() {
    let path = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut owner = DurableDirectory::open(
        path.path(),
        "ownership",
        "a",
        vec!["b".into(), "c".into()],
        clock.clone(),
    )
    .unwrap();
    clock.advance(3000);
    owner.tick().unwrap();
    let term = owner.current_term();
    owner
        .step(
            "b".into(),
            RaftRpc::RequestVoteResp {
                term,
                vote_granted: true,
            },
        )
        .unwrap();
    assert_eq!(owner.role(), Role::Leader);
    assert_eq!(
        owner
            .submit_ownership("alice", "claim", &claim(None))
            .unwrap(),
        Status::Pending { index: 1, term }
    );
    assert_eq!(
        owner
            .submit_ownership(
                "alice",
                "absent",
                &Command::Observe {
                    resource: "absent".into()
                }
            )
            .unwrap(),
        Status::Pending { index: 2, term }
    );
    owner
        .step(
            "b".into(),
            RaftRpc::AppendEntriesResp {
                term,
                success: true,
                match_index: 1,
            },
        )
        .unwrap();
    assert_eq!(
        owner
            .ownership_status(
                "alice",
                "absent",
                Command::Observe {
                    resource: "absent".into()
                }
                .digest()
                .unwrap()
            )
            .unwrap(),
        Status::Pending { index: 2, term }
    );
    assert_eq!(owner.applied_index(), 1);
    owner
        .step(
            "b".into(),
            RaftRpc::AppendEntriesResp {
                term,
                success: true,
                match_index: 2,
            },
        )
        .unwrap();
    assert!(matches!(
        owner
            .ownership_status(
                "alice",
                "absent",
                Command::Observe {
                    resource: "absent".into()
                }
                .digest()
                .unwrap()
            )
            .unwrap(),
        Status::Applied {
            outcome: Outcome::Observed(None),
            ..
        }
    ));
    owner
        .step(
            "c".into(),
            RaftRpc::AppendEntriesResp {
                term: term + 1,
                success: false,
                match_index: 2,
            },
        )
        .unwrap();
    assert!(owner
        .submit_ownership("alice", "new-read", &observe())
        .is_err());
    assert_eq!(owner.applied_index(), 2);
}
#[test]
fn new_leader_reconfirms_pending_identity_once_in_its_current_term() {
    let path = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut owner = DurableDirectory::open(
        path.path(),
        "ownership",
        "b",
        vec!["a".into()],
        clock.clone(),
    )
    .unwrap();
    owner
        .step(
            "a".into(),
            append(
                vec![LogEntry {
                    index: 1,
                    term: 2,
                    cmd: encoded("alice", "pending", &claim(None)),
                }],
                0,
            ),
        )
        .unwrap();
    clock.advance(3000);
    owner.tick().unwrap();
    let term = owner.current_term();
    owner
        .step(
            "a".into(),
            RaftRpc::RequestVoteResp {
                term,
                vote_granted: true,
            },
        )
        .unwrap();
    assert_eq!(
        owner
            .submit_ownership("alice", "pending", &claim(None))
            .unwrap(),
        Status::Pending { index: 2, term }
    );
    assert_eq!(
        owner
            .submit_ownership("alice", "pending", &claim(None))
            .unwrap(),
        Status::Pending { index: 2, term }
    );
    owner
        .step(
            "a".into(),
            RaftRpc::AppendEntriesResp {
                term,
                success: true,
                match_index: 2,
            },
        )
        .unwrap();
    let status = owner
        .ownership_status("alice", "pending", claim(None).digest().unwrap())
        .unwrap();
    assert_eq!(changed(&status, Some("alice")).generation, 1);
    assert_eq!(owner.applied_index(), 2);
}
#[test]
fn invalid_commands_and_replicated_suffixes_change_neither_log_nor_business() {
    let path = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut owner = open(path.path(), clock.clone());
    elect(&mut owner, &clock);
    let before = std::fs::read(path.path().join("raft.state")).unwrap();
    for command in [
        Command::Observe {
            resource: "".into(),
        },
        Command::Observe {
            resource: "a\n".into(),
        },
        Command::Observe {
            resource: "x".repeat(4097),
        },
        claim(Some(0)),
        release(0),
    ] {
        assert!(owner.submit_ownership("alice", "bad", &command).is_err());
    }
    assert!(owner.submit_ownership("alice", "", &claim(None)).is_err());
    assert!(owner
        .submit_ownership("not/a/node", "id", &claim(None))
        .is_err());
    assert_eq!(owner.applied_index(), 0);
    assert_eq!(
        std::fs::read(path.path().join("raft.state")).unwrap(),
        before
    );
    let follower = tempfile::tempdir().unwrap();
    let mut owner =
        DurableDirectory::open(follower.path(), "ownership", "b", vec!["a".into()], clock).unwrap();
    let before = std::fs::read(follower.path().join("raft.state")).unwrap();
    let mut trailing = encoded("alice", "bad", &claim(None));
    trailing.push(0);
    for bytes in [
        trailing,
        encoded("alice", "", &claim(None)),
        encoded("alice", "bad", &claim(Some(0))),
        b"\xffOWN\x01\xff\xff\xff\xff".to_vec(),
    ] {
        assert!(owner
            .step(
                "a".into(),
                append(
                    vec![
                        LogEntry {
                            index: 1,
                            term: 2,
                            cmd: encoded("alice", "good", &claim(None))
                        },
                        LogEntry {
                            index: 2,
                            term: 2,
                            cmd: bytes
                        }
                    ],
                    2
                )
            )
            .is_err());
        assert_eq!(owner.applied_index(), 0);
        assert_eq!(
            std::fs::read(follower.path().join("raft.state")).unwrap(),
            before
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
fn certificates(names: &[&str]) -> Vec<TlsConfig> {
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca = params.self_signed(&ca_key).unwrap().pem().into_bytes();
    let issuer = rcgen::Issuer::new(params, ca_key);
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mtls_concurrent_claims_bind_holder_to_certificate_and_require_admin_authority() {
    let names = [
        "own-dir.test",
        "own-alice.test",
        "own-bob.test",
        "own-denied.test",
    ];
    let certs = certificates(&names);
    let path = tempfile::tempdir().unwrap();
    let reserve = TcpListener::bind("127.0.0.1:0").unwrap();
    let route = NodeAddr::tcp(names[0], reserve.local_addr().unwrap());
    drop(reserve);
    let mut host = NetworkDirectory::start_with_timing(
        path.path().to_path_buf(),
        "ownership",
        RemoteConfig::tcp(names[0], Some(route.addr))
            .with_mtls(certs[0].clone(), [names[1].into(), names[2].into()]),
        vec![],
        Arc::new(Empty),
        DirectoryTiming {
            election_ms: 100,
            heartbeat_ms: 20,
            rpc_timeout_ms: 200,
        },
    )
    .await
    .unwrap();
    let mut remotes = Vec::new();
    let mut clients = Vec::new();
    for i in 1..4 {
        let remote = RemoteActorSystem::new(
            RemoteConfig::tcp(names[i], None).with_mtls(certs[i].clone(), []),
            Arc::new(Empty),
        )
        .unwrap();
        remote.start().await.unwrap();
        clients.push(OwnershipClient::new(remote.clone(), "ownership").unwrap());
        remotes.push(remote);
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.handle().observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let duration = Duration::from_secs(3);
    let concurrent_claim = claim(None);
    let (alice, bob) = tokio::join!(
        clients[0].submit_and_wait(&route, "claim", &concurrent_claim, duration),
        clients[1].submit_and_wait(&route, "claim", &concurrent_claim, duration)
    );
    let statuses = [alice.unwrap(), bob.unwrap()];
    let winners: Vec<_> = statuses
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            matches!(
                s,
                Status::Applied {
                    outcome: Outcome::Changed(_),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(winners.len(), 1, "{statuses:?}");
    let (winner, status) = winners[0];
    let record = changed(status, Some(names[winner + 1]));
    rejected(
        &statuses[1 - winner],
        Rejection::GenerationConflict,
        &record,
    );
    let before = host.handle().observation().applied_index;
    assert!(matches!(
        clients[2]
            .submit(&route, "denied", &claim(Some(record.generation)), duration)
            .await,
        Err(NetworkDirectoryError::Forbidden)
    ));
    assert!(matches!(
        clients[2]
            .status(
                &route,
                "claim",
                concurrent_claim.digest().unwrap(),
                duration
            )
            .await,
        Err(NetworkDirectoryError::Forbidden)
    ));
    let wrong_group = OwnershipClient::new(remotes[0].clone(), "other").unwrap();
    assert!(matches!(
        wrong_group
            .submit(
                &route,
                "wrong-group",
                &claim(Some(record.generation)),
                duration
            )
            .await,
        Err(NetworkDirectoryError::Forbidden)
    ));
    #[derive(serde::Serialize)]
    enum WireOperation {
        Submit { id: String, command: Command },
    }
    let valid = bincode::serde::encode_to_vec(
        (
            "ownership",
            WireOperation::Submit {
                id: "bad-wire".into(),
                command: claim(Some(record.generation)),
            },
        ),
        bincode::config::standard(),
    )
    .unwrap();
    let path = format!(
        "parrot://{}{}",
        names[0],
        parrot_remote::roles::directory::ownership::PATH
    );
    for (payload, target, hop) in [
        (
            valid.clone(),
            "parrot://wrong/_directory/ownership/v1".to_owned(),
            0,
        ),
        (valid.clone(), path.clone(), 1),
        (
            {
                let mut bytes = valid.clone();
                bytes.push(0);
                bytes
            },
            path.clone(),
            0,
        ),
        (vec![255; 64], path.clone(), 0),
        (vec![0; 8193], path.clone(), 0),
    ] {
        let source = &remotes[0];
        let sender = source
            .links_snapshot()
            .await
            .into_iter()
            .find(|(node, _, _)| node == names[0])
            .unwrap()
            .1;
        let cid = source.callbacks.next_cid();
        let (tx, rx) = tokio::sync::oneshot::channel();
        source
            .callbacks
            .insert_on(cid, names[0], tx, &sender)
            .unwrap();
        let mut frame = parrot_remote::Frame::ask(
            cid,
            &target,
            parrot_remote::roles::directory::ownership::KEY,
            payload.into(),
            None,
        );
        frame.header.hop_count = hop;
        sender.send(frame).await.unwrap();
        let reply = tokio::time::timeout(duration, rx).await;
        source.callbacks.remove(cid);
        let parrot_remote::ReplyPayload::Ok(bytes, key) = reply.unwrap().unwrap() else {
            panic!("expected structured control rejection");
        };
        assert_eq!(key, parrot_remote::roles::directory::ownership::REPLY_KEY);
        let (result, used): (Result<Status, NetworkDirectoryError>, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(used, bytes.len());
        assert!(result.is_err(), "malformed control accepted: {result:?}");
    }
    assert_eq!(host.handle().observation().applied_index, before);
    let observed = clients[0]
        .observe(&route, "/user/entity-alice", duration)
        .await
        .unwrap();
    assert!(
        matches!(observed, Status::Applied { outcome: Outcome::Observed(Some(actual)), .. } if actual == record)
    );
    let maximum_resource = "é".repeat(2048);
    let maximum_id = "i".repeat(128);
    let maximum_command = Command::Claim {
        resource: maximum_resource.clone(),
        expected: None,
    };
    let maximum = clients[0]
        .submit_and_wait(&route, &maximum_id, &maximum_command, duration)
        .await
        .unwrap();
    assert!(
        matches!(&maximum, Status::Applied { outcome: Outcome::Changed(record), .. } if record.resource == maximum_resource && record.holder.as_deref() == Some(names[1]))
    );
    assert_eq!(
        clients[0]
            .status(
                &route,
                &maximum_id,
                maximum_command.digest().unwrap(),
                duration
            )
            .await
            .unwrap(),
        maximum
    );
    assert!(
        matches!(clients[1].observe(&route, &maximum_resource, duration).await.unwrap(), Status::Applied { outcome: Outcome::Observed(Some(record)), .. } if record.resource == maximum_resource && record.holder.as_deref() == Some(names[1]))
    );
    for remote in &remotes {
        remote.shutdown().await.unwrap();
        assert_eq!(remote.callbacks.len(), 0);
    }
    host.close().await.unwrap();
}

#[test]
fn ownership_control_cannot_be_an_application_mirror_shadow() {
    let mut frame = parrot_remote::Frame::tell_with_ack(
        1,
        "parrot://a/_directory/ownership/v1",
        parrot_remote::roles::directory::ownership::KEY,
        bytes::Bytes::copy_from_slice(&1000u32.to_le_bytes()),
    );
    frame.header.flags |= parrot_remote::frame::flags::MIRROR_SHADOW;
    assert!(frame.validate_payload_flags().is_err());
    frame.type_key = "application:message#v1".into();
    assert!(frame.validate_payload_flags().is_ok());
}

#[tokio::test]
async fn invalid_client_deadlines_and_commands_publish_nothing() {
    let name = format!("ownership-deadline-{}", uuid::Uuid::new_v4());
    let remote = RemoteActorSystem::new(RemoteConfig::mem(&name), Arc::new(Empty)).unwrap();
    let client = OwnershipClient::new(remote.clone(), "ownership").unwrap();
    let peer = NodeAddr::mem("missing-owner");
    for timeout in [Duration::ZERO, Duration::MAX, Duration::from_secs(61)] {
        assert!(matches!(
            client.submit(&peer, "id", &claim(None), timeout).await,
            Err(NetworkDirectoryError::Invalid(_))
        ));
        assert!(matches!(
            client
                .submit_and_wait(&peer, "id", &claim(None), timeout)
                .await,
            Err(NetworkDirectoryError::Invalid(_))
        ));
    }
    assert!(matches!(
        client
            .submit(&peer, "id", &claim(Some(0)), Duration::from_secs(1))
            .await,
        Err(NetworkDirectoryError::Invalid(_))
    ));
    assert_eq!(remote.callbacks.len(), 0);
    remote.shutdown().await.unwrap();
}
