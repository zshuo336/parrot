// SPDX-License-Identifier: AGPL-3.0-only
use super::{Attempt, Lane, LaneError, Outbound};
use crate::{
    error::ErrCode, frame::Frame, ordered::OrderedEndpoint, ref_::RemoteInner,
    transport::FrameSender,
};
use parrot_api::{errors::ActorError, types::ActorResult};
use std::{
    collections::HashMap,
    hash::{BuildHasher, RandomState},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{sync::RwLock, time::Instant};

#[derive(Clone)]
pub(super) struct Binding {
    endpoint: OrderedEndpoint,
    lane: Weak<tokio::sync::Mutex<Lane>>,
    ticket: super::Identity,
}

pub(super) enum CloseScope<'a> {
    #[cfg(test)]
    Node,
    Reference(&'a str),
    Path(&'a str),
    Binding(&'a Binding),
}

impl CloseScope<'_> {
    pub(super) fn path(&self) -> Option<&str> {
        match self {
            #[cfg(test)]
            Self::Node => None,
            Self::Reference(path) | Self::Path(path) => Some(path),
            Self::Binding(binding) => Some(&binding.endpoint.path),
        }
    }
    pub(super) fn matches_lane(&self, lane: &Arc<tokio::sync::Mutex<Lane>>) -> bool {
        !matches!(self, Self::Binding(binding) if binding.lane.as_ptr() != Arc::as_ptr(lane))
    }
    pub(super) fn matches_grant(&self, grant: Option<&super::Grant>) -> bool {
        match self {
            #[cfg(test)]
            Self::Node => true,
            Self::Reference(_) => true,
            Self::Path(path) => grant.is_some_and(|g| g.path == *path),
            Self::Binding(binding) => grant.is_some_and(|g| {
                g.receipt.ticket == binding.ticket
                    && g.receipt.receiver == binding.endpoint.receiver
            }),
        }
    }
}

pub(super) struct AliasRoutes {
    // Native sends share a read lock. Only alias publication, remapping and
    // explicit close exclude another send with the same logical path.
    gates: [RwLock<()>; 64],
    hash: RandomState,
    bindings: Mutex<HashMap<String, Binding>>,
}

impl Default for AliasRoutes {
    fn default() -> Self {
        Self {
            gates: std::array::from_fn(|_| RwLock::new(())),
            hash: RandomState::new(),
            bindings: Mutex::default(),
        }
    }
}

impl AliasRoutes {
    fn gate(&self, path: &str) -> &RwLock<()> {
        &self.gates[self.hash.hash_one(path) as usize % self.gates.len()]
    }

    pub(super) fn clear(&self) {
        self.bindings.lock().unwrap().clear();
    }

    pub(super) fn bind(
        &self,
        path: &str,
        endpoint: &OrderedEndpoint,
        lane: &Arc<tokio::sync::Mutex<Lane>>,
        ticket: super::Identity,
        capacity: usize,
    ) -> ActorResult<()> {
        let mut table = self.bindings.lock().unwrap();
        if table.get(path).is_some_and(|current| {
            current.ticket == ticket
                && current.endpoint == *endpoint
                && current.lane.as_ptr() == Arc::as_ptr(lane)
        }) {
            return Ok(());
        }
        if !table.contains_key(path) && table.len() >= capacity {
            table.retain(|_, binding| binding.lane.strong_count() != 0);
            if table.len() >= capacity {
                return Err(
                    ErrCode::Overloaded.to_actor_error("ordered alias capacity exhausted".into())
                );
            }
        }
        table.insert(
            path.into(),
            Binding {
                endpoint: endpoint.clone(),
                lane: Arc::downgrade(lane),
                ticket,
            },
        );
        Ok(())
    }
}

impl Outbound {
    pub(crate) async fn send_reference(
        &self,
        destination: &str,
        sender: &FrameSender,
        inner: &RemoteInner,
        frame: Frame,
        required: bool,
    ) -> ActorResult<Option<Frame>> {
        crate::transport::notify_safely(async {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Err(ActorError::Stopped),
                result = self.dispatch_reference(destination, sender, inner, frame, required) => result,
            }
        }).await
    }

    async fn dispatch_reference(
        &self,
        destination: &str,
        sender: &FrameSender,
        inner: &RemoteInner,
        mut frame: Frame,
        required: bool,
    ) -> ActorResult<Option<Frame>> {
        let gate = self.aliases.gate(&frame.path);
        let read = gate.read().await;
        let mut at_capacity = false;
        if !self
            .aliases
            .bindings
            .lock()
            .unwrap()
            .contains_key(&frame.path)
        {
            match self
                .send_open(destination, sender, &inner.callbacks, frame, required, None)
                .await?
            {
                Attempt::Delivered => return Ok(None),
                Attempt::Legacy(frame) => return Ok(Some(frame)),
                Attempt::Alias(original) => frame = original,
                Attempt::Full(original) => {
                    at_capacity = true;
                    frame = original;
                }
            }
        }
        drop(read);
        let _write = gate.write().await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let endpoint = self
            .lookup(
                super::Route {
                    destination,
                    path: &frame.path,
                    hops: frame.header.hop_limit,
                },
                sender,
                &inner.callbacks,
                true,
                deadline,
            )
            .await;
        let (endpoint, signed) = match endpoint {
            Ok(result) if !at_capacity || result.0.path != frame.path => result,
            _ if at_capacity => return Err(LaneError::Full.into()),
            other => other?,
        };
        let (node, _) = crate::actor_path::split(&endpoint.path).expect("validated endpoint");
        let previous = self
            .aliases
            .bindings
            .lock()
            .unwrap()
            .get(&frame.path)
            .cloned();
        if let Some(binding) = previous {
            if binding.endpoint != endpoint {
                self.retire_binding(&binding, inner, deadline).await?;
            }
        } else if endpoint.path != frame.path {
            self.close_matching(
                destination,
                |first| async move { inner.ensure_sender(&first).await },
                &inner.callbacks,
                deadline,
                CloseScope::Path(&frame.path),
            )
            .await?;
        }
        let current = inner.ensure_sender(node).await;
        let (route, path) = match (current, signed) {
            (Some(direct), _) if direct.node_id() == node => (direct, endpoint.path.clone()),
            (_, Some(path)) => (sender.clone(), path),
            (current, None) => (
                current.unwrap_or_else(|| sender.clone()),
                endpoint.path.clone(),
            ),
        };
        let alias = std::mem::replace(&mut frame.path, path);
        match self
            .send_open(
                node,
                &route,
                &inner.callbacks,
                frame,
                true,
                Some((&alias, &endpoint)),
            )
            .await?
        {
            Attempt::Delivered => Ok(None),
            _ => Err(ErrCode::NotRemotable
                .to_actor_error("ordered endpoint changed before publication".into())),
        }
    }

    async fn retire_binding(
        &self,
        binding: &Binding,
        inner: &RemoteInner,
        deadline: Instant,
    ) -> ActorResult<()> {
        let (node, _) =
            crate::actor_path::split(&binding.endpoint.path).expect("validated endpoint");
        self.close_matching(
            node,
            |first| async move { inner.ensure_sender(&first).await },
            &inner.callbacks,
            deadline,
            CloseScope::Binding(binding),
        )
        .await
    }

    pub(crate) async fn close_reference(
        &self,
        destination: &str,
        path: &str,
        inner: &RemoteInner,
        deadline: Instant,
    ) -> ActorResult<()> {
        tokio::time::timeout_at(deadline, async {
            let _write = self.aliases.gate(path).write().await;
            let binding = self.aliases.bindings.lock().unwrap().get(path).cloned();
            if let Some(binding) = binding {
                self.retire_binding(&binding, inner, deadline).await?;
                self.aliases.bindings.lock().unwrap().remove(path);
                Ok(())
            } else {
                self.close_matching(
                    destination,
                    |first| async move { inner.ensure_sender(&first).await },
                    &inner.callbacks,
                    deadline,
                    CloseScope::Reference(path),
                )
                .await
            }
        })
        .await
        .unwrap_or(Err(ActorError::Timeout))
    }
}
