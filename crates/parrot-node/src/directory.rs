//! Standard node's Directory read actor at `/system/directory`. Every reply is
//! explicitly a local applied observation, not a linearizable quorum read.
use parrot::thread::{
    config::ThreadActorConfig, context::ThreadContext, system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use parrot_remote::roles::directory::{network::DirectoryHandle, DirEntry, DirQuery};
use std::sync::Arc;
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueryLocal(pub DirQuery);
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalEntry(pub Option<DirEntry>);
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Inspect;
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Observation {
    pub is_leader: bool,
    pub term: u64,
    pub applied_index: u64,
    pub leader: Option<String>,
    pub closed: bool,
}
struct DirectoryActor(DirectoryHandle);
impl Actor for DirectoryActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(QueryLocal(query)) = msg.downcast_ref::<QueryLocal>() {
                let entry = self.0.query_local(query).await.map_err(|e| {
                    parrot_api::errors::ActorError::MessageHandlingError(e.to_string())
                })?;
                return Ok(Box::new(LocalEntry(entry)) as BoxedMessage);
            }
            if msg.downcast_ref::<Inspect>().is_some() {
                let view = self.0.observation();
                return Ok(Box::new(Observation {
                    is_leader: view.role == parrot_remote::raft::Role::Leader,
                    term: view.term,
                    applied_index: view.applied_index,
                    leader: view.leader,
                    closed: view.closed,
                }) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "Directory read actor: unknown request".into(),
            ))
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
pub(crate) async fn install(
    system: &Arc<ThreadActorSystem>,
    handle: DirectoryHandle,
) -> Result<(), crate::runtime::NodeError> {
    system
        .spawn_at(
            DirectoryActor(handle),
            "/system/directory",
            None,
            ThreadActorConfig::default(),
        )
        .await?;
    Ok(())
}
