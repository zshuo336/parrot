//! A register hosted by real shard children. The caller supplies immutable
//! write identity/revision/value; this is not an increment/exactly-once service.
use parrot_api::{
    actor::{Actor, ActorState},
    address::ActorRef,
    context::ActorContext,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use parrot_node::{
    configuration::NodeSettings,
    runtime::{NodeError, RunningNode},
    sharding::{ShardConfig, ShardEntity},
};
use parrot_remote::{
    roles::directory::ownership::OwnershipStatus, LocalLookup, NodeAddr, RemoteActorSystem,
    RemoteConfig,
};
use serde::{Deserialize, Serialize};
use std::{any::Any, sync::Arc, time::Duration};

#[derive(Serialize, Deserialize)]
enum Request {
    Read,
    Write {
        id: String,
        expected_revision: Option<u64>,
        value: Vec<u8>,
    },
}
#[derive(Debug, Serialize, Deserialize)]
struct Reply(Result<OwnershipStatus, String>);
macro_rules! codec {
    ($ty:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$ty>(),
                encode: |message: &BoxedMessage| parrot_api::message::serde_remote_serialize(
                    message.downcast_ref::<$ty>().ok_or("example codec type mismatch")?
                ),
                decode: |bytes| {
                    let value: $ty = parrot_api::message::serde_remote_deserialize(bytes)?;
                    Ok(Box::new(value) as BoxedMessage)
                },
            }
        }
    };
}
codec!(Request, "bin:example::sharded_register::Request#v1");
codec!(Reply, "bin:example::sharded_register::Reply#v1");
struct Register(ShardEntity);
impl Actor for Register {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let result = match message.downcast::<Request>() {
                Ok(request) => match *request {
                    Request::Read => self.0.read_value(Duration::from_secs(5)).await,
                    Request::Write {
                        id,
                        expected_revision,
                        value,
                    } => {
                        self.0
                            .write_value(&id, expected_revision, value, Duration::from_secs(5))
                            .await
                    }
                }
                .map_err(|e| e.to_string()),
                Err(_) => Err("expected a register request".into()),
            };
            Ok(Box::new(Reply(result)) as BoxedMessage)
        })
    }
}
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
fn routes(variable: &str) -> Result<Vec<NodeAddr>, NodeError> {
    std::env::var(variable)?
        .split(',')
        .map(|entry| entry.parse().map_err(Into::into))
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), NodeError> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let usage = "serve | read NODE KEY | write NODE KEY ID REVISION_OR_none VALUE";
    let settings = NodeSettings::from_environment(|key| std::env::var_os(key))?;
    let members = routes("PARROT_SHARD_MEMBERS")?;
    match args.first().map(String::as_str) {
        Some("serve") if args.len() == 1 => {
            let config = ShardConfig::new(
                "register",
                members,
                std::env::var("PARROT_SHARD_GROUP")?,
                routes("PARROT_SHARD_DIRECTORY")?,
            );
            let node = RunningNode::start(&settings).await?;
            let result: Result<(), NodeError> = async {
                node.install_shards(config, Arc::new(|entity| Ok(Box::new(Register(entity)))))
                    .await?;
                println!("SHARDED_REGISTER_READY={}", node.endpoint()?);
                tokio::signal::ctrl_c().await?;
                Ok(())
            }
            .await;
            let stopped = node.shutdown().await;
            result.and(stopped)
        }
        Some(mode @ ("read" | "write"))
            if (mode == "read" && args.len() == 3) || (mode == "write" && args.len() == 6) =>
        {
            let peer = members
                .iter()
                .find(|m| m.node_id == args[1])
                .ok_or("NODE is not configured in PARROT_SHARD_MEMBERS")?;
            let request = if mode == "read" {
                Request::Read
            } else {
                Request::Write {
                    id: args[3].clone(),
                    expected_revision: if args[4] == "none" {
                        None
                    } else {
                        Some(args[4].parse()?)
                    },
                    value: args[5].as_bytes().to_vec(),
                }
            };
            let config =
                RemoteConfig::from_resolved_async(&settings.resolved, Duration::from_secs(5))
                    .await?;
            let remote = RemoteActorSystem::new(config, Arc::new(Empty))?;
            let result: Result<(), NodeError> = async {
                remote.start().await?;
                remote.connect(peer).await?;
                let actor = remote.remote_ref(&format!(
                    "parrot://{}/shards/register/{}",
                    peer.node_id, args[2]
                ))?;
                let reply = actor
                    .send_with_timeout(Box::new(request), Some(Duration::from_secs(10)))
                    .await?;
                println!(
                    "{:?}",
                    reply
                        .downcast_ref::<Reply>()
                        .ok_or("unexpected reply type")?
                );
                Ok(())
            }
            .await;
            let stopped = remote
                .shutdown()
                .await
                .map_err(|e| Box::new(e) as NodeError);
            result.and(stopped)
        }
        _ => Err(usage.into()),
    }
}
