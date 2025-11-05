use parrot_api::{address::ActorRef, types::BoxedMessage};
use parrot_node::{configuration::NodeSettings, runtime::RunningNode, NEcho, NEchoed};
use parrot_remote::{
    raft::{ManualClock, Role},
    roles::directory::{DirCmd, DirQuery, DurableDirectory, DurableDirectoryStatus},
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{sync::Arc, time::Duration};
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn ActorRef>> {
        None
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standard_node_owns_directory_actors_failure_supervision_and_storage_release() {
    let path =
        std::env::temp_dir().join(format!("parrot-standard-directory-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let mut c = parrot_config::ParrotConfig::builder()
        .remote_node_id("standard-directory")
        .remote_topology_role("directory");
    c.remote.node.scheme = Some("mem".into());
    c.remote.directory = Some(parrot_config::directory::DirectorySection {
        storage_dir: Some(path.to_str().unwrap().into()),
        group: Some("standard".into()),
        peers: Some(vec![]),
        ..Default::default()
    });
    let settings = NodeSettings {
        resolved: c.build().unwrap(),
        actors: vec!["echo".into()],
        ready_file: None,
        wasm_epoch_ticks: None,
    };
    let node = RunningNode::start(&settings).await.unwrap();
    let h = node.directory().unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        while h.observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let receipt = h
        .propose(&DirCmd::Upsert {
            node: "worker".into(),
            endpoints: vec!["mem://worker".into()],
        })
        .await
        .unwrap();
    assert_eq!(
        h.proposal_status(&receipt).await.unwrap(),
        DurableDirectoryStatus::Applied
    );
    let query = DirQuery::Resolve {
        prefix_or_node: "worker".into(),
    };
    let before = h.query_local(&query).await.unwrap().unwrap();
    let client = RemoteActorSystem::new(
        RemoteConfig::mem("standard-directory-client"),
        Arc::new(Empty),
    )
    .unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::mem("standard-directory"))
        .await
        .unwrap();
    let actor = client
        .remote_ref("parrot://standard-directory/user/echo")
        .unwrap();
    let reply = ActorRef::send_with_timeout(
        &actor,
        Box::new(NEcho(71)) as BoxedMessage,
        Some(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    assert_eq!(*reply.downcast::<NEchoed>().unwrap(), NEchoed(71));
    let read = client
        .remote_ref("parrot://standard-directory/system/directory")
        .unwrap();
    let result = ActorRef::send_with_timeout(
        &read,
        Box::new(parrot_node::directory::QueryLocal(query.clone())),
        Some(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    assert_eq!(
        *result
            .downcast::<parrot_node::directory::LocalEntry>()
            .unwrap(),
        parrot_node::directory::LocalEntry(Some(before.clone()))
    );
    let result = ActorRef::send_with_timeout(
        &read,
        Box::new(parrot_node::directory::Inspect),
        Some(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    let view = result
        .downcast::<parrot_node::directory::Observation>()
        .unwrap();
    assert!(view.is_leader && !view.closed);
    assert_eq!(view.applied_index, 1);
    // A real sync publication failure must fence the Directory, notify the
    // process supervisor, and still shut down actors and release the disk lock.
    std::fs::create_dir(path.join("raft.next")).unwrap();
    assert!(h
        .propose(&DirCmd::Remove {
            node: "worker".into()
        })
        .await
        .is_err());
    let error = tokio::time::timeout(Duration::from_secs(2), node.wait_directory_failure())
        .await
        .unwrap();
    assert!(error.to_string().contains("Directory owner terminated"));
    assert!(node.shutdown().await.is_err());
    assert!(h.query_local(&query).await.is_err());
    assert_eq!(node.remote.callbacks.len(), 0);
    assert!(
        ActorRef::send_with_timeout(&actor, Box::new(NEcho(72)), Some(Duration::from_secs(1)))
            .await
            .is_err()
    );
    client.shutdown().await.unwrap();
    drop(node);
    std::fs::remove_dir(path.join("raft.next")).unwrap();
    let reopened = DurableDirectory::open(
        &path,
        "standard",
        "standard-directory",
        vec![],
        Arc::new(ManualClock::new()),
    )
    .unwrap();
    assert_eq!(reopened.query_local(&query).unwrap(), Some(before));
    drop(reopened);
    std::fs::remove_dir_all(path).unwrap();
}
