//! Shared with the archived, bincode-2.0.1 fixture producer. Never regenerate
//! committed fixtures with the codec under test: that would erase the oracle.
use bytes::Bytes;
use parrot_api::message::{serde_remote_deserialize, serde_remote_serialize};
use parrot_remote::{
    admin::{self, AdminCommand, AdminReply, SysEvent},
    admin_v2::{
        self, AdminArtifactRef, AdminCommandV2, AdminInstancePolicy, AdminReplyV2, ComponentDeploy,
    },
    durable::{DurableEnvelope, InboxLimits, TransactionalInbox, Wal, WalRecord},
    raft::{DurableRaft, LogEntry, ManualClock, RaftRpc, StateMachine},
    receptionist::{self, ApiKey, ReceptionistEntry, ReceptionistSync},
    roles::directory::{DirCmd, DirectorySm},
    swim::{self, Member, MemberEvent, MemberStatus, MembershipGossip, NodeAddrWire},
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path, sync::Arc};

fn fixture(root: &Path, name: &str, actual: &[u8], generate: bool) -> Vec<u8> {
    let path = root.join(name);
    if generate {
        // Generation is only called by the archived old-codec executable.
        assert!(!path.exists(), "refusing to overwrite a golden fixture");
        std::fs::write(&path, actual).unwrap();
    }
    let saved = std::fs::read(&path).unwrap();
    assert_eq!(actual, saved, "wire bytes changed: {name}");
    saved
}

fn check<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    root: &Path,
    name: &str,
    expected: &T,
    generate: bool,
) {
    let bytes = fixture(
        root,
        name,
        &serde_remote_serialize(expected).unwrap(),
        generate,
    );
    let decoded: T = serde_remote_deserialize(&bytes).unwrap();
    assert_eq!(&decoded, expected, "decoded fields changed: {name}");
    for end in 0..bytes.len() {
        assert!(
            serde_remote_deserialize::<T>(&bytes[..end]).is_err(),
            "prefix {end}: {name}"
        );
    }
    let mut suffix = bytes;
    suffix.push(0xa5);
    assert!(
        serde_remote_deserialize::<T>(&suffix).is_err(),
        "suffix: {name}"
    );
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum Application {
    Empty,
    Scalar(i128, u128, char, bool, f64),
    Nested {
        name: String,
        data: Bytes,
        values: BTreeMap<String, Option<Vec<i64>>>,
    },
}

pub fn wire(root: &Path, generate: bool) {
    if generate {
        std::fs::create_dir_all(root).unwrap();
    }
    for (i, value) in [
        Application::Empty,
        Application::Scalar(i128::MIN, u128::MAX, '鹦', true, -0.0),
        Application::Nested {
            name: "应用\0字段".into(),
            data: Bytes::from_static(&[0, 128, 255]),
            values: BTreeMap::from([
                ("a".into(), None),
                (
                    "b".into(),
                    Some(vec![i64::MIN, -251, 0, 250, 251, 65535, 65536, i64::MAX]),
                ),
            ]),
        },
    ]
    .iter()
    .enumerate()
    {
        check(root, &format!("application-{i}.bin"), value, generate);
    }
    let commands = [
        DirCmd::Upsert {
            node: "节点".into(),
            endpoints: vec!["tcp://127.0.0.1:7000".into(), "quic://[::1]:7001".into()],
        },
        DirCmd::Remove {
            node: "gone".into(),
        },
        DirCmd::BorderDeclare {
            prefix: "edge".into(),
            node: "border".into(),
        },
        DirCmd::KeyAggregate {
            key: "service:v1".into(),
            nodes: vec!["a".into(), "b".into()],
        },
    ];
    for (i, command) in commands.iter().enumerate() {
        let bytes = fixture(
            root,
            &format!("directory-{i}.bin"),
            &command.encode().unwrap(),
            generate,
        );
        assert_eq!(DirCmd::decode(&bytes).unwrap(), *command);
    }
    let rpcs = [
        RaftRpc::RequestVote {
            term: u64::MAX,
            candidate: "候选者".into(),
            last_log_index: 65536,
            last_log_term: 251,
        },
        RaftRpc::RequestVoteResp {
            term: 251,
            vote_granted: true,
        },
        RaftRpc::AppendEntries {
            term: 251,
            leader: "a".into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: history(3),
            leader_commit: 2,
        },
        RaftRpc::AppendEntriesResp {
            term: 251,
            success: false,
            match_index: 65536,
        },
    ];
    for (i, rpc) in rpcs.iter().enumerate() {
        check(root, &format!("raft-{i}.bin"), rpc, generate);
    }
    let commands = [
        AdminCommand::SpawnLocal {
            req_id: u64::MAX,
            props: "factory".into(),
            path: "/actor".into(),
            reply_to: "parrot://n/reply".into(),
        },
        AdminCommand::AdminStop {
            req_id: 251,
            target_path: "/actor".into(),
            reply_to: "parrot://n/reply".into(),
        },
    ];
    for (i, command) in commands.iter().enumerate() {
        let bytes = fixture(
            root,
            &format!("admin-command-{i}.bin"),
            &admin::encode_admin_cmd(command),
            generate,
        );
        let SysEvent::AdminCommand(decoded) = admin::decode_sys_event(&bytes).unwrap() else {
            panic!("wrong tag")
        };
        assert_eq!(decoded, *command);
    }
    for (i, reply) in [
        AdminReply::Spawned {
            req_id: 65536,
            path: "/actor".into(),
        },
        AdminReply::Failed {
            req_id: u64::MAX,
            code: 65535,
            detail: "失败".into(),
        },
        AdminReply::Stopped { req_id: 251 },
    ]
    .iter()
    .enumerate()
    {
        let bytes = fixture(
            root,
            &format!("admin-reply-{i}.bin"),
            &admin::encode_admin_reply(reply),
            generate,
        );
        let SysEvent::AdminReply(decoded) = admin::decode_sys_event(&bytes).unwrap() else {
            panic!("wrong tag")
        };
        assert_eq!(decoded, *reply);
    }
    // Keep the production corpus's outer SYSTEM_EVENT tags in the oracle too.
    for (i, vector) in admin_v2::admin_v2_vectors().iter().enumerate() {
        let bytes = fixture(root, &format!("admin-v2-{i}.bin"), &vector.bytes, generate);
        let encoded = match admin::decode_sys_event(&bytes).unwrap() {
            SysEvent::AdminCommandV2(v) => admin_v2::encode_admin_cmd_v2(&v),
            SysEvent::AdminReplyV2(v) => admin_v2::encode_admin_reply_v2(&v),
            _ => panic!("wrong v2 tag"),
        };
        assert_eq!(encoded.as_ref(), bytes);
    }
    for (i, artifact) in [
        AdminArtifactRef::Props {
            factory: "echo".into(),
        },
        AdminArtifactRef::Beam {
            app: "frontier".into(),
            uri: Some("file:///beam".into()),
        },
        AdminArtifactRef::PyModule {
            module: "worker".into(),
            runtime_env: Some("[env_vars]".into()),
            uri: None,
        },
        AdminArtifactRef::Jvm {
            main_class: "app.Main".into(),
            coords: None,
            uri: Some("file:///app.jar".into()),
        },
        AdminArtifactRef::Wasm {
            digest: "abcd".into(),
            uri: "file:///app.wasm".into(),
        },
        AdminArtifactRef::Dylib {
            digest: "abcd".into(),
            uri: "file:///app.so".into(),
            abi: 65536,
        },
    ]
    .into_iter()
    .enumerate()
    {
        for (j, instances) in [
            AdminInstancePolicy::Singleton,
            AdminInstancePolicy::Pool { count: 251 },
            AdminInstancePolicy::Sharded { count: 65536 },
        ]
        .into_iter()
        .enumerate()
        {
            check(
                root,
                &format!("deployment-{i}-{j}.bin"),
                &AdminCommandV2::DeployComponent {
                    req_id: u64::MAX,
                    component: ComponentDeploy {
                        name: "component".into(),
                        version: "1.0".into(),
                        artifact: artifact.clone(),
                        instances,
                        config: Some(vec![0, 128, 255]),
                    },
                },
                generate,
            );
        }
    }
    check(
        root,
        "metrics-command.bin",
        &AdminCommandV2::MetricsReportV2 { req_id: u64::MAX },
        generate,
    );
    check(
        root,
        "metrics-reply.bin",
        &AdminReplyV2::MeasuredMetrics {
            req_id: 65536,
            metrics: Default::default(),
        },
        generate,
    );
    let members: Vec<_> = [
        MemberStatus::Alive,
        MemberStatus::Suspect,
        MemberStatus::Dead,
    ]
    .iter()
    .enumerate()
    .map(|(i, status)| Member {
        node_id: format!("n{i}"),
        addr: NodeAddrWire {
            node_id: format!("n{i}"),
            scheme: "tcp".into(),
            host: "::1".into(),
            port: 65535,
        },
        status: *status,
        incarnation: 251,
        status_until_ms: u64::MAX,
        metadata: Bytes::from_static(&[0, 255]),
    })
    .collect();
    let gossip = MembershipGossip {
        events: vec![
            MemberEvent::Upsert(members[0].clone()),
            MemberEvent::Remove("gone".into()),
        ],
        seen_from: "sender".into(),
        digest: u64::MAX,
        full_sync: Some(members),
    };
    let bytes = fixture(root, "gossip.bin", &swim::encode_gossip(&gossip), generate);
    let decoded = swim::decode_gossip(&bytes[1..]).unwrap();
    assert_eq!(decoded.events, gossip.events);
    assert_eq!(decoded.full_sync, gossip.full_sync);
    assert_eq!(decoded.seen_from, gossip.seen_from);
    assert_eq!(decoded.digest, gossip.digest);
    let sync = ReceptionistSync {
        from: "sender".into(),
        entries: vec![ReceptionistEntry {
            key: ApiKey::new("service").unwrap(),
            remote_path: "parrot://n/actor".into(),
            node: "n".into(),
        }],
    };
    let bytes = fixture(
        root,
        "receptionist.bin",
        &receptionist::encode_sync(&sync),
        generate,
    );
    let decoded = receptionist::decode_sync(&bytes[1..]).unwrap();
    assert_eq!(decoded.from, sync.from);
    assert_eq!(decoded.entries, sync.entries);
}

fn record(sender: &str, sequence: u64) -> WalRecord {
    WalRecord {
        endpoint: "receiver".into(),
        sender: sender.into(),
        seq: sequence,
        path: "/counter".into(),
        type_key: "counter#v1".into(),
        payload: Bytes::from(vec![sequence as u8, 0, 255]),
    }
}
fn pending(wal: &Wal, first: u64, last: u64) {
    let mut actual = wal.replay("receiver").unwrap();
    actual.sort_by(|a, b| (&a.sender, a.seq).cmp(&(&b.sender, b.seq)));
    let expected: Vec<_> = ["s1", "s2"]
        .iter()
        .flat_map(|s| (first..=last).map(move |n| record(s, n)))
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(wal.pending_len().unwrap(), expected.len());
}
fn history(count: u64) -> Vec<LogEntry> {
    (1..=count)
        .map(|index| LogEntry {
            term: 251,
            index,
            cmd: DirCmd::Upsert {
                node: format!("n{index}"),
                endpoints: vec![format!("tcp://127.0.0.1:{}", 7000 + index)],
            }
            .encode()
            .unwrap(),
        })
        .collect()
}
fn message(sequence: u64, session: &str) -> DurableEnvelope {
    DurableEnvelope {
        source: "sender".into(),
        session: session.into(),
        receiver: "receiver".into(),
        path: "/counter".into(),
        sequence,
        type_key: "counter#v1".into(),
        payload: Bytes::from(7u64.to_le_bytes().to_vec()),
    }
}
fn add(state: &[u8], message: &DurableEnvelope) -> Result<Bytes, String> {
    let n = u64::from_le_bytes(state.try_into().unwrap());
    let delta = u64::from_le_bytes(message.payload.as_ref().try_into().unwrap());
    Ok(Bytes::from((n + delta).to_le_bytes().to_vec()))
}

/// Step 0 creates actual durable files. Steps 1 and 2 require the previous
/// writer's state, suppress duplicates, advance commits, then reopen once more.
pub async fn persistence(root: &Path, step: u64) {
    assert!(step <= 2);
    if step == 0 {
        std::fs::create_dir(root.join("raft")).unwrap();
    }
    let wal = Wal::open(root.join("wal")).unwrap();
    if step == 0 {
        for n in 1..=3 {
            for s in ["s1", "s2"] {
                wal.append(record(s, n)).unwrap();
            }
        }
    } else {
        pending(&wal, step + 1, step + 2);
        // Previously acknowledged messages cannot reappear after replay.
        for s in ["s1", "s2"] {
            wal.append(record(s, 1)).unwrap();
        }
        pending(&wal, step + 1, step + 2);
        for s in ["s1", "s2"] {
            wal.append(record(s, step + 3)).unwrap();
        }
    }
    for s in ["s1", "s2"] {
        wal.ack("receiver", s, step + 1).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);
    let wal = Wal::open(root.join("wal")).unwrap();
    pending(&wal, step + 2, step + 3);
    drop(wal);

    let open_raft = || {
        DurableRaft::open(
            root.join("raft"),
            "compat",
            "b",
            vec!["a".into(), "c".into()],
            Arc::new(ManualClock::new()),
        )
        .unwrap()
    };
    let mut raft = open_raft();
    if step != 0 {
        assert_eq!(raft.current_term(), 251);
        assert_eq!(raft.log(), history(step + 2));
        assert_eq!(raft.commit_index(), step + 1);
        assert_eq!(raft.committed().unwrap().len() as u64, step + 1);
    }
    raft.step(
        "a".into(),
        RaftRpc::AppendEntries {
            term: 251,
            leader: "a".into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: history(step + 3),
            leader_commit: step + 2,
        },
    )
    .unwrap();
    drop(raft);
    let mut raft = open_raft();
    assert_eq!(raft.log(), history(step + 3));
    assert_eq!(raft.commit_index(), step + 2);
    let mut directory = DirectorySm::default();
    for entry in raft.committed().unwrap() {
        directory.apply(&entry.cmd).unwrap();
    }
    assert_eq!(directory.version, step + 2);
    assert_eq!(directory.nodes.len() as u64, step + 2);
    for n in 1..=step + 2 {
        assert_eq!(
            directory.nodes[&format!("n{n}")].endpoints,
            [format!("tcp://127.0.0.1:{}", 7000 + n)]
        );
    }
    assert!(!directory.nodes.contains_key(&format!("n{}", step + 3)));
    drop(raft);

    let mut inbox = TransactionalInbox::open(
        root.join("inbox"),
        "receiver",
        "/counter",
        Bytes::from(0u64.to_le_bytes().to_vec()),
        InboxLimits::default(),
        add,
    )
    .await
    .unwrap();
    assert_eq!(inbox.handle().snapshot().revision, step * 2);
    assert_eq!(
        inbox.handle().snapshot().state.as_ref(),
        (step * 14).to_le_bytes()
    );
    if step != 0 {
        for session in ["alpha", "beta"] {
            inbox.handle().apply(message(step, session)).await.unwrap();
        }
        assert_eq!(inbox.handle().snapshot().revision, step * 2);
        assert_eq!(
            inbox.handle().snapshot().state.as_ref(),
            (step * 14).to_le_bytes()
        );
    }
    for session in ["alpha", "beta"] {
        inbox
            .handle()
            .apply(message(step + 1, session))
            .await
            .unwrap();
    }
    inbox.close().await.unwrap();
    let mut inbox = TransactionalInbox::open(
        root.join("inbox"),
        "receiver",
        "/counter",
        Bytes::new(),
        InboxLimits::default(),
        add,
    )
    .await
    .unwrap();
    assert_eq!(inbox.handle().snapshot().revision, (step + 1) * 2);
    assert_eq!(inbox.handle().snapshot().streams, 2);
    assert_eq!(
        inbox.handle().snapshot().state.as_ref(),
        ((step + 1) * 14).to_le_bytes()
    );
    inbox.close().await.unwrap();
}
