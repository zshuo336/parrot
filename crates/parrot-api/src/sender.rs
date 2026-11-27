// SPDX-License-Identifier: AGPL-3.0-only
//! The local actor generation executing a handler. An independent spawned task
//! has no implicit sender. This identity is distinct from a message's trace.

use std::{future::Future, sync::Arc};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActorSender(Arc<Generation>);

#[derive(Debug, Eq, PartialEq)]
struct Generation {
    path: String,
    incarnation: [u8; 16],
}

tokio::task_local! { static CURRENT: ActorSender; }

impl ActorSender {
    /// Engines create one identity for each actor context generation.
    pub fn new(path: &str) -> Self {
        Self(Arc::new(Generation {
            path: path.to_owned(),
            incarnation: *uuid::Uuid::new_v4().as_bytes(),
        }))
    }

    pub fn path(&self) -> &str {
        &self.0.path
    }
    pub fn incarnation(&self) -> [u8; 16] {
        self.0.incarnation
    }
    pub fn current() -> Option<Self> {
        CURRENT.try_with(Clone::clone).ok()
    }
    pub fn scope<F: Future>(&self, future: F) -> impl Future<Output = F::Output> + use<F> {
        CURRENT.scope(self.clone(), future)
    }
    pub fn in_scope<T>(&self, call: impl FnOnce() -> T) -> T {
        CURRENT.sync_scope(self.clone(), call)
    }
}
