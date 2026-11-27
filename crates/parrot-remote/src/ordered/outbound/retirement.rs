// SPDX-License-Identifier: AGPL-3.0-only
use super::{aliases::CloseScope, Identity, Lane, Outbound, SenderContext};
use crate::{registry::CallbackRegistry, system::RemoteActorSystem};
use futures::{stream, StreamExt};
use parrot_api::sender::WeakActorSender;
use std::{
    sync::{Arc, Weak},
    time::Duration,
};

pub(super) struct LaneEntry {
    pub(super) lane: Arc<tokio::sync::Mutex<Lane>>,
    pub(super) owner: Option<SenderLane>,
    pub(super) shared_by_actors: bool,
}

impl LaneEntry {
    pub(super) fn owned_by(&self, incarnation: Identity) -> bool {
        self.owner
            .as_ref()
            .is_none_or(|owner| owner.incarnation == incarnation)
    }
}

#[derive(Clone)]
pub(super) struct SenderLane {
    pub(super) source: String,
    pub(super) incarnation: Identity,
    pub(super) lifetime: WeakActorSender,
    pub(super) destination: String,
    pub(super) path: String,
}

impl Outbound {
    pub(crate) async fn retire_stopped_senders(
        &self,
        system: Weak<RemoteActorSystem>,
        callbacks: Arc<CallbackRegistry>,
    ) {
        loop {
            let pending = self
                .journal
                .as_ref()
                .map(|j| j.pending())
                .unwrap_or_default();
            let recovering = !pending.is_empty();
            stream::iter(pending)
                .for_each_concurrent(8, |saved| {
                    let system = &system;
                    let callbacks = &callbacks;
                    async move {
                        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                        let attempt = async {
                            let first =
                                crate::actor_path::split(crate::ordered::route::head(&saved.path))
                                    .map_or(saved.destination.as_str(), |(node, _)| node);
                            if let Some(current) =
                                RemoteActorSystem::sender_for(system, first).await
                            {
                                let original = Self {
                                    source: saved.source.clone(),
                                    incarnation: saved.incarnation,
                                    context: SenderContext::Node,
                                    tables: self.tables.clone(),
                                };
                                let _ = original
                                    .retire_ticket(
                                        &saved,
                                        None,
                                        &current,
                                        callbacks,
                                        crate::frame::DEFAULT_HOP_LIMIT,
                                        deadline,
                                    )
                                    .await;
                            }
                        };
                        let _ = tokio::time::timeout_at(deadline, attempt).await;
                    }
                })
                .await;
            let (has_owners, stopped) = {
                let lanes = self.lanes.lock().unwrap();
                let owners = lanes.values().filter_map(|entry| entry.owner.as_ref());
                let has_owners = owners.clone().next().is_some();
                (
                    has_owners,
                    owners
                        .filter(|owner| owner.lifetime.is_retired())
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            };
            if !has_owners && !recovering {
                self.sender_added.notified().await;
                continue;
            }
            // Every item is charged to a live slot. A lost route keeps its slot
            // until a later pass obtains the original receiver's retirement proof.
            stream::iter(stopped)
                .for_each_concurrent(8, |owner| {
                    let system = &system;
                    let callbacks = &callbacks;
                    async move {
                        let source = Self {
                            source: owner.source,
                            incarnation: owner.incarnation,
                            context: SenderContext::Actor(owner.lifetime),
                            tables: self.tables.clone(),
                        };
                        let _ =
                            source
                                .close_matching(
                                    &owner.destination,
                                    |node| async move {
                                        RemoteActorSystem::sender_for(system, &node).await
                                    },
                                    callbacks,
                                    tokio::time::Instant::now() + Duration::from_secs(5),
                                    CloseScope::Reference(&owner.path),
                                )
                                .await;
                    }
                })
                .await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}
