use parrot_remote::{
    handshake::HandshakeBody,
    roles::directory::{
        network::{DirectoryTiming, NetworkDirectory, NetworkDirectoryError},
        ownership::{
            OwnershipClusterClient, OwnershipCommand as Command, OwnershipOutcome as Outcome,
            OwnershipStatus as Status,
        },
    },
    transport::{memory::MemoryTransport, FrameSender, Transport},
    Frame, LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{sync::Arc, time::Duration};
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
fn remote(name: &str) -> Arc<RemoteActorSystem> {
    RemoteActorSystem::new(RemoteConfig::mem(name), Arc::new(Empty)).unwrap()
}
fn claim() -> Command {
    Command::Claim {
        resource: "entity".into(),
        expected: None,
    }
}

type RawInput = (Frame, FrameSender, String);
async fn raw_peer() -> (
    Arc<RemoteActorSystem>,
    MemoryTransport,
    tokio::sync::mpsc::Receiver<RawInput>,
    OwnershipClusterClient,
) {
    let id = uuid::Uuid::new_v4();
    let remote = remote(&format!("caller-{id}"));
    remote.start().await.unwrap();
    let name = format!("member-{id}");
    let (inbound, rx) = tokio::sync::mpsc::channel(16);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let peer = MemoryTransport::new(
        HandshakeBody {
            node_id: name.clone(),
            ..Default::default()
        },
        inbound,
        Arc::new(|_| {}),
        shutdown,
        None,
    )
    .unwrap();
    let route = NodeAddr::mem(name);
    let (_, _handle) = tokio::try_join!(remote.connect(&route), peer.accept()).unwrap();
    let client = OwnershipClusterClient::new(remote.clone(), "group", vec![route]).unwrap();
    (remote, peer, rx, client)
}

async fn reply(request: &Frame, back: &FrameSender, status: Status) {
    reply_result(request, back, Ok(status)).await;
}

async fn reply_result(
    request: &Frame,
    back: &FrameSender,
    result: Result<Status, NetworkDirectoryError>,
) {
    let payload = bincode::serde::encode_to_vec(result, bincode::config::standard()).unwrap();
    back.send(Frame::reply(
        request.header.correlation_id,
        &request.path,
        parrot_remote::roles::directory::ownership::REPLY_KEY,
        payload.into(),
    ))
    .await
    .unwrap();
}

fn observed(index: u64) -> Status {
    Status::Applied {
        index,
        term: 1,
        outcome: Outcome::Observed(None),
    }
}

#[tokio::test]
async fn pending_and_lost_reply_retry_identical_wire_and_ignore_late_response() {
    let (remote, peer, mut rx, client) = raw_peer().await;
    let client = client
        .with_attempt_timeout(Duration::from_millis(150))
        .unwrap();
    let mut invocation = tokio::spawn(async move {
        client
            .submit_and_wait(
                "immutable-id",
                &Command::Observe {
                    resource: "entity".into(),
                },
                Duration::from_secs(5),
            )
            .await
    });
    let (first, back, caller) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(caller, remote.config.node_id);
    assert_eq!(
        first.path,
        format!(
            "parrot://{}/_directory/ownership/v1",
            remote.config.node_id.replacen("caller-", "member-", 1)
        )
    );
    assert_eq!(
        first.type_key,
        parrot_remote::roles::directory::ownership::KEY
    );
    assert_eq!(remote.callbacks.len(), 1);
    reply(&first, &back, Status::Pending { index: 1, term: 1 }).await;
    let (second, _, second_caller) = tokio::select! {
        biased;
        result = &mut invocation => panic!("Pending completed the invocation: {result:?}"),
        request = tokio::time::timeout(Duration::from_secs(2), rx.recv()) => request.unwrap().unwrap(),
    };
    assert_eq!(second_caller, caller);
    assert_eq!(second.payload, first.payload);
    assert_ne!(second.header.correlation_id, first.header.correlation_id);
    // Lose the second reply. Only after a new callback exists, send the stale
    // response; it must not satisfy that different in-flight request.
    let (third, back, third_caller) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(third_caller, caller);
    assert_eq!(third.payload, first.payload);
    assert_ne!(third.header.correlation_id, second.header.correlation_id);
    assert_eq!(remote.callbacks.len(), 1);
    reply(&second, &back, observed(99)).await;
    reply(&third, &back, observed(3)).await;
    assert_eq!(invocation.await.unwrap().unwrap(), observed(3));
    assert_eq!(remote.callbacks.len(), 0);
    remote.shutdown().await.unwrap();
    peer.shutdown().await;
}

#[tokio::test]
async fn configured_attempt_budget_accepts_a_reply_after_the_default_budget() {
    let (remote, peer, mut rx, client) = raw_peer().await;
    let client = client.with_attempt_timeout(Duration::from_secs(2)).unwrap();
    let invocation =
        tokio::spawn(async move { client.observe("entity", Duration::from_secs(5)).await });
    let (request, back, _) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert!(
        rx.try_recv().is_err(),
        "configured attempt budget was ignored"
    );
    assert_eq!(remote.callbacks.len(), 1);
    reply(&request, &back, observed(1)).await;
    assert_eq!(invocation.await.unwrap().unwrap(), observed(1));
    assert_eq!(remote.callbacks.len(), 0);
    remote.shutdown().await.unwrap();
    peer.shutdown().await;
}

#[tokio::test]
async fn cancellation_deadline_and_shutdown_retire_live_reply_callbacks() {
    let (remote, peer, mut rx, client) = raw_peer().await;
    let client = client
        .with_attempt_timeout(Duration::from_secs(10))
        .unwrap();
    for mode in ["cancel", "deadline", "shutdown"] {
        let invocation = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .observe(
                        "entity",
                        if mode == "deadline" {
                            Duration::from_millis(300)
                        } else {
                            Duration::from_secs(60)
                        },
                    )
                    .await
            }
        });
        let _request = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            remote.callbacks.len(),
            1,
            "{mode} must interrupt a live callback"
        );
        match mode {
            "cancel" => {
                invocation.abort();
                assert!(invocation.await.unwrap_err().is_cancelled());
            }
            "deadline" => assert!(matches!(
                invocation.await.unwrap(),
                Err(NetworkDirectoryError::Timeout)
            )),
            "shutdown" => {
                remote.shutdown().await.unwrap();
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(1), invocation)
                        .await
                        .unwrap()
                        .unwrap(),
                    Err(NetworkDirectoryError::Closed)
                ));
            }
            _ => unreachable!(),
        }
        assert_eq!(remote.callbacks.len(), 0, "{mode}");
    }
    peer.shutdown().await;
}

#[tokio::test]
async fn admission_errors_are_terminal_and_transient_hints_keep_the_original_route() {
    let (remote, peer, mut rx, client) = raw_peer().await;
    for error in [
        NetworkDirectoryError::Invalid("invalid command".into()),
        NetworkDirectoryError::Forbidden,
        NetworkDirectoryError::Capacity,
    ] {
        let expected = error.to_string();
        let invocation = tokio::spawn({
            let client = client.clone();
            async move { client.observe("entity", Duration::from_secs(5)).await }
        });
        let (request, back, _) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        reply_result(&request, &back, Err(error)).await;
        let result = tokio::time::timeout(Duration::from_secs(1), invocation).await;
        assert!(matches!(result, Ok(Ok(Err(error))) if error.to_string() == expected));
        assert_eq!(remote.callbacks.len(), 0);
        assert!(
            rx.try_recv().is_err(),
            "terminal admission error was retried"
        );
    }
    let invocation =
        tokio::spawn(async move { client.observe("entity", Duration::from_secs(5)).await });
    let mut original = None;
    for result in [
        Err(NetworkDirectoryError::NotLeader(Some(
            "unconfigured-member".into(),
        ))),
        Err(NetworkDirectoryError::Operation("reply lost".into())),
        Err(NetworkDirectoryError::Closed),
        Ok(Status::Unknown),
        Ok(observed(7)),
    ] {
        let (request, back, _) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request.path,
            format!(
                "parrot://{}/_directory/ownership/v1",
                remote.config.node_id.replacen("caller-", "member-", 1)
            )
        );
        assert_eq!(
            request.payload,
            *original.get_or_insert_with(|| request.payload.clone())
        );
        reply_result(&request, &back, result).await;
    }
    assert_eq!(invocation.await.unwrap().unwrap(), observed(7));
    assert_eq!(remote.callbacks.len(), 0);
    remote.shutdown().await.unwrap();
    peer.shutdown().await;
}

#[test]
fn configuration_requires_bounded_explicit_distinct_matching_routes() {
    let remote = remote("cluster-config");
    let peer = NodeAddr::mem("member");
    let mut implicit = peer.clone();
    implicit.identity_is_explicit = false;
    let mut foreign = peer.clone();
    foreign.scheme = "tcp".into();
    let mut invalid = peer.clone();
    invalid.node_id = "bad/node".into();
    for peers in [
        vec![],
        vec![peer.clone(), peer.clone()],
        vec![implicit],
        vec![foreign],
        vec![invalid],
        (0..65).map(|i| NodeAddr::mem(format!("m{i}"))).collect(),
    ] {
        assert!(matches!(
            OwnershipClusterClient::new(remote.clone(), "group", peers),
            Err(NetworkDirectoryError::Invalid(_))
        ));
    }
    let valid = OwnershipClusterClient::new(remote.clone(), "group", vec![peer.clone()]).unwrap();
    for timeout in [Duration::ZERO, Duration::from_secs(61), Duration::MAX] {
        assert!(valid.clone().with_attempt_timeout(timeout).is_err());
    }
    assert!(valid.with_attempt_timeout(Duration::from_secs(2)).is_ok());
    assert!(OwnershipClusterClient::new(remote, "", vec![peer]).is_err());
}

#[tokio::test]
async fn invalid_invocations_and_total_deadlines_do_not_leave_callbacks() {
    let remote = remote("cluster-deadline");
    remote.start().await.unwrap();
    let client = OwnershipClusterClient::new(
        remote.clone(),
        "group",
        vec![NodeAddr::mem("absent-member")],
    )
    .unwrap();
    for timeout in [Duration::ZERO, Duration::from_secs(61), Duration::MAX] {
        assert!(matches!(
            client.submit_and_wait("id", &claim(), timeout).await,
            Err(NetworkDirectoryError::Invalid(_))
        ));
    }
    assert!(matches!(
        client
            .submit_and_wait("", &claim(), Duration::from_secs(1))
            .await,
        Err(NetworkDirectoryError::Invalid(_))
    ));
    assert!(matches!(
        client.observe("", Duration::from_secs(1)).await,
        Err(NetworkDirectoryError::Invalid(_))
    ));
    assert!(matches!(
        client
            .submit_and_wait("id", &claim(), Duration::from_millis(60))
            .await,
        Err(NetworkDirectoryError::Timeout)
    ));
    let command = claim();
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        client.submit_and_wait("cancel", &command, Duration::from_secs(5))
    )
    .await
    .is_err());
    assert_eq!(remote.callbacks.len(), 0);
    let blocked_command = claim();
    let blocked = client.submit_and_wait("shutdown", &blocked_command, Duration::from_secs(5));
    tokio::pin!(blocked);
    tokio::select! {
        biased;
        result = &mut blocked => panic!("retry finished before shutdown: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
    }
    remote.shutdown().await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), &mut blocked)
            .await
            .unwrap(),
        Err(NetworkDirectoryError::Closed)
    ));
    assert!(matches!(
        client.observe("entity", Duration::from_secs(1)).await,
        Err(NetworkDirectoryError::Closed)
    ));
    assert_eq!(remote.callbacks.len(), 0);
}

#[tokio::test]
async fn alternate_route_preserves_results_conflicts_and_fresh_observations() {
    let root = tempfile::tempdir().unwrap();
    let id = format!("cluster-{}", uuid::Uuid::new_v4());
    let mut host = NetworkDirectory::start_with_timing(
        root.path(),
        "group",
        RemoteConfig::mem(&id),
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
    let remote = remote(&format!("client-{}", uuid::Uuid::new_v4()));
    remote.start().await.unwrap();
    let routes = vec![NodeAddr::mem(format!("absent-{id}")), NodeAddr::mem(&id)];
    let client = OwnershipClusterClient::new(remote.clone(), "group", routes).unwrap();
    let timeout = Duration::from_secs(5);
    let first = client
        .submit_and_wait("first", &claim(), timeout)
        .await
        .unwrap();
    let record = match &first {
        Status::Applied {
            outcome: Outcome::Changed(record),
            ..
        } => record.clone(),
        other => panic!("{other:?}"),
    };
    let observed = client.observe("entity", timeout).await.unwrap();
    assert!(
        matches!(observed, Status::Applied { outcome: Outcome::Observed(Some(current)), .. } if current == record)
    );
    let replacement = Command::Claim {
        resource: "entity".into(),
        expected: Some(record.generation),
    };
    let changed = client
        .clone()
        .submit_and_wait("next", &replacement, timeout)
        .await
        .unwrap();
    let next = match changed {
        Status::Applied {
            outcome: Outcome::Changed(record),
            ..
        } => record,
        other => panic!("{other:?}"),
    };
    assert!(next.generation > record.generation);
    assert_eq!(
        client
            .submit_and_wait("first", &claim(), timeout)
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        client
            .submit_and_wait("first", &replacement, timeout)
            .await
            .unwrap(),
        Status::Conflict
    );
    assert!(
        matches!(client.observe("entity", timeout).await.unwrap(), Status::Applied { outcome: Outcome::Observed(Some(current)), .. } if current == next)
    );
    let wrong_group =
        OwnershipClusterClient::new(remote.clone(), "different-group", vec![NodeAddr::mem(&id)])
            .unwrap();
    assert!(matches!(
        wrong_group
            .submit_and_wait("forbidden", &claim(), timeout)
            .await,
        Err(NetworkDirectoryError::Forbidden)
    ));
    assert_eq!(remote.callbacks.len(), 0);
    remote.shutdown().await.unwrap();
    host.close().await.unwrap();
}
