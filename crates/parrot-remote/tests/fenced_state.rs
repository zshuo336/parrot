use parrot_remote::{
    raft::{ManualClock, RaftRpc, Role},
    roles::directory::{
        network::{DirectoryTiming, NetworkDirectory, NetworkDirectoryError},
        ownership::{
            OwnershipClient, OwnershipCommand as Command, OwnershipOutcome as Outcome,
            OwnershipRecord as Record, OwnershipRejection as Rejection, OwnershipStatus as Status,
            OwnershipValue as Value, MAX_VALUE_BYTES,
        },
        DurableDirectory,
    },
    tls::TlsConfig,
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{net::TcpListener, sync::Arc, time::Duration};

const RESOURCE: &str = "/user/entity-alice";
#[test]
fn existing_ownership_v1_encodings_remain_byte_compatible() {
    for (command, expected) in [
        (
            Command::Claim {
                resource: "r".into(),
                expected: None,
            },
            vec![0, 1, b'r', 0],
        ),
        (
            Command::Release {
                resource: "r".into(),
                expected: 7,
            },
            vec![1, 1, b'r', 7],
        ),
        (
            Command::Observe {
                resource: "r".into(),
            },
            vec![2, 1, b'r'],
        ),
    ] {
        assert_eq!(
            bincode::serde::encode_to_vec(&command, bincode::config::standard()).unwrap(),
            expected
        );
    }
    let status = Status::Applied {
        index: 1,
        term: 2,
        outcome: Outcome::Changed(Record {
            resource: "r".into(),
            generation: 1,
            holder: Some("a".into()),
        }),
    };
    assert_eq!(
        bincode::serde::encode_to_vec(status, bincode::config::standard()).unwrap(),
        [2, 1, 2, 0, 1, b'r', 1, 1, 1, b'a']
    );
}
fn claim(expected: Option<u64>) -> Command {
    Command::Claim {
        resource: RESOURCE.into(),
        expected,
    }
}
fn write(generation: u64, expected_revision: Option<u64>, value: &[u8]) -> Command {
    Command::WriteValue {
        resource: RESOURCE.into(),
        generation,
        expected_revision,
        value: value.into(),
    }
}
fn read() -> Command {
    Command::ReadValue {
        resource: RESOURCE.into(),
    }
}
fn owner(status: Status) -> Record {
    match status {
        Status::Applied {
            index,
            outcome: Outcome::Changed(record),
            ..
        } => {
            assert_eq!(record.generation, index);
            record
        }
        other => panic!("expected ownership change: {other:?}"),
    }
}
fn written(status: &Status, record: &Record, expected: &[u8]) -> Value {
    match status {
        Status::Applied {
            index,
            outcome: Outcome::ValueChanged { ownership, value },
            ..
        } => {
            assert_eq!(ownership, record);
            assert_eq!(value.revision, *index);
            assert_eq!(value.value.as_ref(), expected);
            value.clone()
        }
        other => panic!("expected committed state change: {other:?}"),
    }
}
fn rejected(status: &Status, reason: Rejection, record: &Record) {
    assert!(
        matches!(status, Status::Applied { outcome: Outcome::Rejected { reason: actual, current: Some(current) }, .. } if *actual == reason && current == record),
        "{status:?}"
    );
}
fn observed(status: &Status, record: &Record, expected: &Value) {
    assert!(
        matches!(status, Status::Applied { outcome: Outcome::ValueObserved { ownership: Some(current), value: Some(value) }, .. } if current == record && value == expected),
        "{status:?}"
    );
}
fn solo(path: &std::path::Path, clock: Arc<ManualClock>) -> DurableDirectory {
    let mut directory =
        DurableDirectory::open(path, "fenced-state", "a", vec![], clock.clone()).unwrap();
    clock.advance(3000);
    directory.tick().unwrap();
    assert_eq!(directory.role(), Role::Leader);
    directory
}

#[test]
fn state_writes_require_current_holder_generation_and_exact_revision() {
    let path = tempfile::tempdir().unwrap();
    let mut directory = solo(path.path(), Arc::new(ManualClock::new()));
    assert!(matches!(
        directory
            .submit_ownership("alice", "empty", &read())
            .unwrap(),
        Status::Applied {
            outcome: Outcome::ValueObserved {
                ownership: None,
                value: None
            },
            ..
        }
    ));
    assert!(matches!(
        directory
            .submit_ownership("alice", "unowned-write", &write(1, None, b"bad"))
            .unwrap(),
        Status::Applied {
            outcome: Outcome::Rejected {
                reason: Rejection::GenerationConflict,
                current: None
            },
            ..
        }
    ));
    let alice = owner(
        directory
            .submit_ownership("alice", "claim", &claim(None))
            .unwrap(),
    );
    rejected(
        &directory
            .submit_ownership("bob", "foreign", &write(alice.generation, None, b"foreign"))
            .unwrap(),
        Rejection::NotHolder,
        &alice,
    );
    let first_command = write(alice.generation, None, b"first");
    let first_result = directory
        .submit_ownership("alice", "write-first", &first_command)
        .unwrap();
    let first = written(&first_result, &alice, b"first");
    assert_ne!(first.revision, alice.generation);
    rejected(
        &directory
            .submit_ownership(
                "alice",
                "lost-update",
                &write(alice.generation, None, b"lost"),
            )
            .unwrap(),
        Rejection::ValueRevisionConflict,
        &alice,
    );
    let bob = owner(
        directory
            .submit_ownership("bob", "takeover", &claim(Some(alice.generation)))
            .unwrap(),
    );
    rejected(
        &directory
            .submit_ownership(
                "alice",
                "delayed",
                &write(alice.generation, Some(first.revision), b"stale"),
            )
            .unwrap(),
        Rejection::GenerationConflict,
        &bob,
    );
    rejected(
        &directory
            .submit_ownership(
                "alice",
                "forged-generation",
                &write(bob.generation, Some(first.revision), b"forged"),
            )
            .unwrap(),
        Rejection::NotHolder,
        &bob,
    );
    observed(
        &directory
            .submit_ownership("bob", "after-takeover", &read())
            .unwrap(),
        &bob,
        &first,
    );
    let empty = written(
        &directory
            .submit_ownership(
                "bob",
                "clear",
                &write(bob.generation, Some(first.revision), b""),
            )
            .unwrap(),
        &bob,
        b"",
    );
    assert!(empty.revision > first.revision);
    rejected(
        &directory
            .submit_ownership(
                "bob",
                "empty-is-not-absent",
                &write(bob.generation, None, b"bad"),
            )
            .unwrap(),
        Rejection::ValueRevisionConflict,
        &bob,
    );
    let tombstone = owner(
        directory
            .submit_ownership(
                "bob",
                "release",
                &Command::Release {
                    resource: RESOURCE.into(),
                    expected: bob.generation,
                },
            )
            .unwrap(),
    );
    rejected(
        &directory
            .submit_ownership(
                "bob",
                "released-write",
                &write(tombstone.generation, Some(empty.revision), b"bad"),
            )
            .unwrap(),
        Rejection::NotHolder,
        &tombstone,
    );
    observed(
        &directory
            .submit_ownership("bob", "released-state", &read())
            .unwrap(),
        &tombstone,
        &empty,
    );
    let next = owner(
        directory
            .submit_ownership("alice", "reclaim", &claim(Some(tombstone.generation)))
            .unwrap(),
    );
    let last = written(
        &directory
            .submit_ownership(
                "alice",
                "new-state",
                &write(next.generation, Some(empty.revision), b"last"),
            )
            .unwrap(),
        &next,
        b"last",
    );
    assert!(last.revision > empty.revision);
    assert_eq!(
        directory
            .submit_ownership("alice", "write-first", &first_command)
            .unwrap(),
        first_result
    );
    observed(
        &directory
            .submit_ownership("alice", "final-state", &read())
            .unwrap(),
        &next,
        &last,
    );
    assert_eq!(directory.state_local().unwrap().version, 0);
}

#[test]
fn same_holder_new_generation_rejects_old_write_even_with_latest_revision() {
    let path = tempfile::tempdir().unwrap();
    let mut directory = solo(path.path(), Arc::new(ManualClock::new()));
    let old = owner(
        directory
            .submit_ownership("alice", "old", &claim(None))
            .unwrap(),
    );
    let value = written(
        &directory
            .submit_ownership("alice", "first", &write(old.generation, None, b"first"))
            .unwrap(),
        &old,
        b"first",
    );
    let current = owner(
        directory
            .submit_ownership("alice", "next", &claim(Some(old.generation)))
            .unwrap(),
    );
    assert_eq!(current.holder, old.holder);
    assert!(current.generation > old.generation);
    rejected(
        &directory
            .submit_ownership(
                "alice",
                "stale",
                &write(old.generation, Some(value.revision), b"must-not-apply"),
            )
            .unwrap(),
        Rejection::GenerationConflict,
        &current,
    );
    observed(
        &directory
            .submit_ownership("alice", "read", &read())
            .unwrap(),
        &current,
        &value,
    );
}

#[test]
fn original_state_success_rejection_and_read_survive_replay() {
    let path = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut directory = solo(path.path(), clock.clone());
    let record = owner(
        directory
            .submit_ownership("alice", "claim", &claim(None))
            .unwrap(),
    );
    let command = write(record.generation, None, b"original");
    let success = directory
        .submit_ownership("alice", "write", &command)
        .unwrap();
    let value = written(&success, &record, b"original");
    let failed_command = write(record.generation, None, b"rejected");
    let rejection = directory
        .submit_ownership("alice", "rejection", &failed_command)
        .unwrap();
    rejected(&rejection, Rejection::ValueRevisionConflict, &record);
    let observation = directory
        .submit_ownership("alice", "read", &read())
        .unwrap();
    observed(&observation, &record, &value);
    let next_command = write(record.generation, Some(value.revision), b"replacement");
    let next = written(
        &directory
            .submit_ownership("alice", "next", &next_command)
            .unwrap(),
        &record,
        b"replacement",
    );
    drop(directory);
    let mut directory =
        DurableDirectory::open(path.path(), "fenced-state", "a", vec![], clock.clone()).unwrap();
    assert_eq!(directory.role(), Role::Follower);
    for (id, command, original) in [
        ("write", &command, &success),
        ("rejection", &failed_command, &rejection),
        ("read", &read(), &observation),
    ] {
        assert_eq!(
            directory.submit_ownership("alice", id, command).unwrap(),
            *original
        );
    }
    assert_eq!(
        directory
            .submit_ownership("alice", "write", &next_command)
            .unwrap(),
        Status::Conflict
    );
    clock.advance(3000);
    directory.tick().unwrap();
    observed(
        &directory
            .submit_ownership("alice", "fresh-read", &read())
            .unwrap(),
        &record,
        &next,
    );
}

fn ack(directory: &mut DurableDirectory, index: u64) {
    directory
        .step(
            "b".into(),
            RaftRpc::AppendEntriesResp {
                term: directory.current_term(),
                success: true,
                match_index: index,
            },
        )
        .unwrap();
}

#[test]
fn replicated_state_duplicates_and_conflicts_never_replace_original_results() {
    let path = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let open = || {
        DurableDirectory::open(
            path.path(),
            "fenced-state",
            "b",
            vec!["a".into()],
            clock.clone(),
        )
        .unwrap()
    };
    let mut directory = open();
    let commands = [
        ("claim", claim(None)),
        ("first", write(1, None, b"first")),
        ("read", read()),
        ("second", write(1, Some(2), b"second")),
        ("first", write(1, None, b"first")),
        ("read", read()),
        ("first", write(1, Some(4), b"conflicting")),
        ("fresh-read", read()),
    ];
    let entries = commands
        .iter()
        .enumerate()
        .map(|(index, (id, command))| {
            let mut cmd = b"\xffOWN\x01".to_vec();
            bincode::serde::encode_into_std_write(
                ("alice", id, command),
                &mut cmd,
                bincode::config::standard(),
            )
            .unwrap();
            parrot_remote::raft::LogEntry {
                index: index as u64 + 1,
                term: 2,
                cmd,
            }
        })
        .collect();
    directory
        .step(
            "a".into(),
            RaftRpc::AppendEntries {
                term: 2,
                leader: "a".into(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries,
                leader_commit: 8,
            },
        )
        .unwrap();
    let record = Record {
        resource: RESOURCE.into(),
        generation: 1,
        holder: Some("alice".into()),
    };
    let first = directory
        .ownership_status("alice", "first", commands[1].1.digest().unwrap())
        .unwrap();
    let first_value = written(&first, &record, b"first");
    assert_eq!(first_value.revision, 2);
    let original_read = directory
        .ownership_status("alice", "read", read().digest().unwrap())
        .unwrap();
    assert!(matches!(original_read, Status::Applied { index: 3, .. }));
    observed(&original_read, &record, &first_value);
    assert_eq!(
        directory
            .ownership_status("alice", "first", commands[6].1.digest().unwrap())
            .unwrap(),
        Status::Conflict
    );
    let last = directory
        .ownership_status("alice", "second", commands[3].1.digest().unwrap())
        .unwrap();
    let last_value = written(&last, &record, b"second");
    assert_eq!(last_value.revision, 4);
    let fresh = directory
        .ownership_status("alice", "fresh-read", read().digest().unwrap())
        .unwrap();
    observed(&fresh, &record, &last_value);
    drop(directory);
    let directory = open();
    assert_eq!(
        directory
            .ownership_status("alice", "first", commands[1].1.digest().unwrap())
            .unwrap(),
        first
    );
    assert_eq!(
        directory
            .ownership_status("alice", "read", read().digest().unwrap())
            .unwrap(),
        original_read
    );
    assert_eq!(
        directory
            .ownership_status("alice", "fresh-read", read().digest().unwrap())
            .unwrap(),
        fresh
    );
}
#[test]
fn queued_takeover_fences_write_at_apply_and_state_waits_for_quorum() {
    let path = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut directory = DurableDirectory::open(
        path.path(),
        "fenced-state",
        "a",
        vec!["b".into(), "c".into()],
        clock.clone(),
    )
    .unwrap();
    clock.advance(3000);
    directory.tick().unwrap();
    directory
        .step(
            "b".into(),
            RaftRpc::RequestVoteResp {
                term: directory.current_term(),
                vote_granted: true,
            },
        )
        .unwrap();
    let term = directory.current_term();
    assert_eq!(
        directory
            .submit_ownership("alice", "claim", &claim(None))
            .unwrap(),
        Status::Pending { index: 1, term }
    );
    ack(&mut directory, 1);
    let alice = owner(
        directory
            .submit_ownership("alice", "claim", &claim(None))
            .unwrap(),
    );
    let first = write(alice.generation, None, b"committed");
    assert_eq!(
        directory
            .submit_ownership("alice", "first", &first)
            .unwrap(),
        Status::Pending { index: 2, term }
    );
    assert_eq!(directory.applied_index(), 1);
    ack(&mut directory, 2);
    let value = written(
        &directory
            .ownership_status("alice", "first", first.digest().unwrap())
            .unwrap(),
        &alice,
        b"committed",
    );
    let takeover = claim(Some(alice.generation));
    assert_eq!(
        directory
            .submit_ownership("bob", "takeover", &takeover)
            .unwrap(),
        Status::Pending { index: 3, term }
    );
    let stale = write(alice.generation, Some(value.revision), b"must-not-apply");
    assert_eq!(
        directory
            .submit_ownership("alice", "stale", &stale)
            .unwrap(),
        Status::Pending { index: 4, term }
    );
    assert_eq!(
        directory.submit_ownership("bob", "read", &read()).unwrap(),
        Status::Pending { index: 5, term }
    );
    assert_eq!(directory.applied_index(), 2);
    ack(&mut directory, 5);
    let bob = owner(
        directory
            .ownership_status("bob", "takeover", takeover.digest().unwrap())
            .unwrap(),
    );
    rejected(
        &directory
            .ownership_status("alice", "stale", stale.digest().unwrap())
            .unwrap(),
        Rejection::GenerationConflict,
        &bob,
    );
    observed(
        &directory
            .ownership_status("bob", "read", read().digest().unwrap())
            .unwrap(),
        &bob,
        &value,
    );
    drop(directory);
    let directory = DurableDirectory::open(
        path.path(),
        "fenced-state",
        "a",
        vec!["b".into(), "c".into()],
        clock,
    )
    .unwrap();
    observed(
        &directory
            .ownership_status("bob", "read", read().digest().unwrap())
            .unwrap(),
        &bob,
        &value,
    );
}

#[test]
fn invalid_state_commands_and_suffixes_do_not_modify_persistent_input() {
    let path = tempfile::tempdir().unwrap();
    let mut directory = solo(path.path(), Arc::new(ManualClock::new()));
    let record = owner(
        directory
            .submit_ownership("alice", "claim", &claim(None))
            .unwrap(),
    );
    let before = std::fs::read(path.path().join("raft.state")).unwrap();
    for command in [
        write(0, None, b"bad"),
        write(record.generation, Some(0), b"bad"),
        write(record.generation, None, &vec![0; MAX_VALUE_BYTES + 1]),
    ] {
        assert!(command.digest().is_err());
        assert!(directory
            .submit_ownership("alice", "invalid", &command)
            .is_err());
        assert_eq!(
            std::fs::read(path.path().join("raft.state")).unwrap(),
            before
        );
    }
    drop(directory);
    let path = tempfile::tempdir().unwrap();
    let mut follower = DurableDirectory::open(
        path.path(),
        "fenced-state",
        "b",
        vec!["a".into()],
        Arc::new(ManualClock::new()),
    )
    .unwrap();
    let before = std::fs::read(path.path().join("raft.state")).unwrap();
    let mut encoded = b"\xffOWN\x01".to_vec();
    bincode::serde::encode_into_std_write(
        (
            "alice",
            "invalid",
            write(1, None, &vec![0; MAX_VALUE_BYTES + 1]),
        ),
        &mut encoded,
        bincode::config::standard(),
    )
    .unwrap();
    assert!(follower
        .step(
            "a".into(),
            RaftRpc::AppendEntries {
                term: 2,
                leader: "a".into(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: vec![parrot_remote::raft::LogEntry {
                    index: 1,
                    term: 2,
                    cmd: encoded
                }],
                leader_commit: 0
            }
        )
        .is_err());
    assert_eq!(
        std::fs::read(path.path().join("raft.state")).unwrap(),
        before
    );
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
async fn client(
    name: &str,
    cert: &TlsConfig,
    group: &str,
) -> (Arc<RemoteActorSystem>, OwnershipClient) {
    let remote = RemoteActorSystem::new(
        RemoteConfig::tcp(name, None).with_mtls(cert.clone(), []),
        Arc::new(Empty),
    )
    .unwrap();
    remote.start().await.unwrap();
    let client = OwnershipClient::new(remote.clone(), group).unwrap();
    (remote, client)
}
async fn elected(host: &NetworkDirectory) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.handle().observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mtls_concurrent_state_cas_takeover_and_restart_keep_exact_effects() {
    let names = [
        "value-dir.test",
        "value-alice.test",
        "value-bob.test",
        "value-denied.test",
    ];
    let certs = certificates(&names);
    let path = tempfile::tempdir().unwrap();
    let reserve = TcpListener::bind("127.0.0.1:0").unwrap();
    let route = NodeAddr::tcp(names[0], reserve.local_addr().unwrap());
    drop(reserve);
    let group = "g".repeat(1011);
    let config = RemoteConfig::tcp(names[0], Some(route.addr))
        .with_mtls(certs[0].clone(), [names[1].into(), names[2].into()]);
    let timing = DirectoryTiming {
        election_ms: 100,
        heartbeat_ms: 20,
        rpc_timeout_ms: 200,
    };
    let mut host = NetworkDirectory::start_with_timing(
        path.path().to_path_buf(),
        &group,
        config.clone(),
        vec![],
        Arc::new(Empty),
        timing,
    )
    .await
    .unwrap();
    elected(&host).await;
    let (alice_remote, alice) = client(names[1], &certs[1], &group).await;
    let (bob_remote, bob) = client(names[2], &certs[2], &group).await;
    let (denied_remote, denied) = client(names[3], &certs[3], &group).await;
    let timeout = Duration::from_secs(3);
    let record = owner(
        alice
            .submit_and_wait(&route, "claim", &claim(None), timeout)
            .await
            .unwrap(),
    );
    let left = write(record.generation, None, b"left");
    let right = write(record.generation, None, b"right");
    let (left_result, right_result) = tokio::join!(
        alice.submit_and_wait(&route, "left", &left, timeout),
        alice.submit_and_wait(&route, "right", &right, timeout)
    );
    let statuses = [left_result.unwrap(), right_result.unwrap()];
    let wins: Vec<_> = statuses
        .iter()
        .enumerate()
        .filter(|(_, status)| {
            matches!(
                status,
                Status::Applied {
                    outcome: Outcome::ValueChanged { .. },
                    ..
                }
            )
        })
        .collect();
    assert_eq!(wins.len(), 1, "{statuses:?}");
    let (winner, result) = wins[0];
    let value = written(
        result,
        &record,
        if winner == 0 { b"left" } else { b"right" },
    );
    rejected(
        &statuses[1 - winner],
        Rejection::ValueRevisionConflict,
        &record,
    );
    let before = host.handle().observation().applied_index;
    assert!(matches!(
        denied
            .submit(
                &route,
                "denied",
                &write(record.generation, Some(value.revision), b"bad"),
                timeout
            )
            .await,
        Err(NetworkDirectoryError::Forbidden)
    ));
    assert_eq!(host.handle().observation().applied_index, before);
    let delayed = write(record.generation, Some(value.revision), b"delayed");
    let replacement = owner(
        bob.submit_and_wait(&route, "takeover", &claim(Some(record.generation)), timeout)
            .await
            .unwrap(),
    );
    rejected(
        &alice
            .submit_and_wait(&route, "delayed", &delayed, timeout)
            .await
            .unwrap(),
        Rejection::GenerationConflict,
        &replacement,
    );
    rejected(
        &alice
            .submit_and_wait(
                &route,
                "forged",
                &write(replacement.generation, Some(value.revision), b"forged"),
                timeout,
            )
            .await
            .unwrap(),
        Rejection::NotHolder,
        &replacement,
    );
    let final_command = write(replacement.generation, Some(value.revision), b"final");
    observed(
        &bob.read_value(&route, RESOURCE, timeout).await.unwrap(),
        &replacement,
        &value,
    );
    let final_result = bob
        .submit_and_wait(&route, "final", &final_command, timeout)
        .await
        .unwrap();
    let final_value = written(&final_result, &replacement, b"final");
    observed(
        &bob.read_value(&route, RESOURCE, timeout).await.unwrap(),
        &replacement,
        &final_value,
    );
    let maximum_resource = "é".repeat(2048);
    let maximum_owner = owner(
        bob.submit_and_wait(
            &route,
            "maximum-claim",
            &Command::Claim {
                resource: maximum_resource.clone(),
                expected: None,
            },
            timeout,
        )
        .await
        .unwrap(),
    );
    let maximum_command = Command::WriteValue {
        resource: maximum_resource.clone(),
        generation: maximum_owner.generation,
        expected_revision: None,
        value: vec![255; MAX_VALUE_BYTES],
    };
    let maximum_id = "i".repeat(128);
    let maximum = bob
        .submit_and_wait(&route, &maximum_id, &maximum_command, timeout)
        .await
        .unwrap();
    let maximum_value = written(&maximum, &maximum_owner, &vec![255; MAX_VALUE_BYTES]);
    observed(
        &bob.read_value(&route, &maximum_resource, timeout)
            .await
            .unwrap(),
        &maximum_owner,
        &maximum_value,
    );
    for remote in [&alice_remote, &bob_remote, &denied_remote] {
        remote.shutdown().await.unwrap();
        assert_eq!(remote.callbacks.len(), 0);
    }
    host.close().await.unwrap();
    let mut host = NetworkDirectory::start_with_timing(
        path.path().to_path_buf(),
        &group,
        config,
        vec![],
        Arc::new(Empty),
        timing,
    )
    .await
    .unwrap();
    elected(&host).await;
    let (alice_remote, alice) = client(names[1], &certs[1], &group).await;
    let (bob_remote, bob) = client(names[2], &certs[2], &group).await;
    assert_eq!(
        bob.submit_and_wait(&route, "final", &final_command, timeout)
            .await
            .unwrap(),
        final_result
    );
    assert_eq!(
        bob.status(
            &route,
            &maximum_id,
            maximum_command.digest().unwrap(),
            timeout
        )
        .await
        .unwrap(),
        maximum
    );
    observed(
        &bob.read_value(&route, &maximum_resource, timeout)
            .await
            .unwrap(),
        &maximum_owner,
        &maximum_value,
    );
    rejected(
        &alice
            .submit_and_wait(&route, "after-restart", &delayed, timeout)
            .await
            .unwrap(),
        Rejection::GenerationConflict,
        &replacement,
    );
    observed(
        &bob.read_value(&route, RESOURCE, timeout).await.unwrap(),
        &replacement,
        &final_value,
    );
    for remote in [&alice_remote, &bob_remote] {
        remote.shutdown().await.unwrap();
        assert_eq!(remote.callbacks.len(), 0);
    }
    host.close().await.unwrap();
}
