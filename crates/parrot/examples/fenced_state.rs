//! Two real actor generations share one durable, fenced external state register.
use parrot::thread::{
    config::ThreadActorConfig, context::ThreadContext, system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    engine::AsyncEngineRuntime,
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
    raft::Role,
    roles::directory::{
        network::{DirectoryTiming, NetworkDirectory},
        ownership::{
            OwnershipClient, OwnershipCommand as Command, OwnershipOutcome as Outcome,
            OwnershipRecord, OwnershipRejection, OwnershipStatus as Status,
        },
    },
};
use std::{sync::Arc, time::Duration};

const RESOURCE: &str = "/user/entity-example";
const GROUP: &str = "fenced-state-example";
const TIMEOUT: Duration = Duration::from_secs(5);
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
struct Replace {
    id: &'static str,
    expected_revision: Option<u64>,
    value: &'static [u8],
}
struct Writer {
    client: OwnershipClient,
    peer: NodeAddr,
    generation: u64,
}
impl Actor for Writer {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let replace = msg
                .downcast::<Replace>()
                .map_err(|_| ActorError::MessageHandlingError("expected Replace".into()))?;
            let status = self
                .client
                .submit_and_wait(
                    &self.peer,
                    replace.id,
                    &Command::WriteValue {
                        resource: RESOURCE.into(),
                        generation: self.generation,
                        expected_revision: replace.expected_revision,
                        value: replace.value.to_vec(),
                    },
                    TIMEOUT,
                )
                .await
                .map_err(|error| ActorError::MessageHandlingError(error.to_string()))?;
            Ok(Box::new(status) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
async fn client(
    name: &str,
) -> Result<(Arc<RemoteActorSystem>, OwnershipClient), Box<dyn std::error::Error>> {
    let remote = RemoteActorSystem::new(RemoteConfig::mem(name), Arc::new(Empty))?;
    remote.start().await?;
    let client = OwnershipClient::new(remote.clone(), GROUP)?;
    Ok((remote, client))
}
fn claimed(status: Status) -> OwnershipRecord {
    match status {
        Status::Applied {
            outcome: Outcome::Changed(record),
            ..
        } => record,
        other => panic!("claim rejected: {other:?}"),
    }
}
async fn replace(
    actor: &dyn ActorRef,
    id: &'static str,
    expected_revision: Option<u64>,
    value: &'static [u8],
) -> ActorResult<Status> {
    let reply = actor
        .send(Box::new(Replace {
            id,
            expected_revision,
            value,
        }))
        .await?;
    reply
        .downcast::<Status>()
        .map(|status| *status)
        .map_err(|_| ActorError::MessageHandlingError("expected ownership result".into()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join(format!("parrot-fenced-state-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root)?;
    let mut directory = NetworkDirectory::start_with_timing(
        root.clone(),
        GROUP,
        RemoteConfig::mem("fenced-directory"),
        vec![],
        Arc::new(Empty),
        DirectoryTiming {
            election_ms: 100,
            heartbeat_ms: 20,
            rpc_timeout_ms: 200,
        },
    )
    .await?;
    tokio::time::timeout(TIMEOUT, async {
        while directory.handle().observation().role != Role::Leader {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let peer = NodeAddr::mem("fenced-directory");
    let (alice_remote, alice) = client("fenced-alice").await?;
    let (bob_remote, bob) = client("fenced-bob").await?;
    let system = ThreadActorSystem::shared(Default::default());
    let first_owner = claimed(
        alice
            .submit_and_wait(
                &peer,
                "claim",
                &Command::Claim {
                    resource: RESOURCE.into(),
                    expected: None,
                },
                TIMEOUT,
            )
            .await?,
    );
    let old = system
        .spawn_at(
            Writer {
                client: alice.clone(),
                peer: peer.clone(),
                generation: first_owner.generation,
            },
            "/user/old-writer",
            None,
            ThreadActorConfig::default(),
        )
        .await?;
    let first = replace(&old, "first-write", None, b"one").await?;
    let first_revision = match &first {
        Status::Applied {
            outcome: Outcome::ValueChanged { value, .. },
            ..
        } => value.revision,
        other => return Err(format!("first write rejected: {other:?}").into()),
    };
    let next_owner = claimed(
        bob.submit_and_wait(
            &peer,
            "claim",
            &Command::Claim {
                resource: RESOURCE.into(),
                expected: Some(first_owner.generation),
            },
            TIMEOUT,
        )
        .await?,
    );
    let current = system
        .spawn_at(
            Writer {
                client: bob.clone(),
                peer: peer.clone(),
                generation: next_owner.generation,
            },
            "/user/new-writer",
            None,
            ThreadActorConfig::default(),
        )
        .await?;
    let next = replace(&current, "second-write", Some(first_revision), b"two").await?;
    let next_value = match next {
        Status::Applied {
            outcome: Outcome::ValueChanged { value, .. },
            ..
        } => value,
        other => return Err(format!("second write rejected: {other:?}").into()),
    };
    assert!(old.is_alive().await);
    let stale = replace(&old, "delayed-write", Some(next_value.revision), b"stale").await?;
    assert!(matches!(
        stale,
        Status::Applied {
            outcome: Outcome::Rejected {
                reason: OwnershipRejection::GenerationConflict,
                ..
            },
            ..
        }
    ));
    assert_eq!(replace(&old, "first-write", None, b"one").await?, first);
    assert!(
        matches!(bob.read_value(&peer, RESOURCE, TIMEOUT).await?, Status::Applied { outcome: Outcome::ValueObserved { ownership: Some(record), value: Some(value) }, .. } if record == next_owner && value == next_value)
    );
    println!("value=two; live stale writer rejected; original retry preserved");
    AsyncEngineRuntime::shutdown(system.as_ref()).await?;
    alice_remote.shutdown().await?;
    bob_remote.shutdown().await?;
    directory.close().await?;
    std::fs::remove_dir_all(root)?;
    Ok(())
}
