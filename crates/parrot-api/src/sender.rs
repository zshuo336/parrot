// SPDX-License-Identifier: AGPL-3.0-only
//! The local actor generation executing a handler. An independent spawned task
//! has no implicit sender. This identity is distinct from a message's trace.

use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
};

#[derive(Clone, Debug)]
pub struct ActorSender(Arc<Generation>);

impl PartialEq for ActorSender {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ActorSender {}

#[derive(Debug)]
struct Generation {
    path: String,
    incarnation: [u8; 16],
    retired: AtomicBool,
}

/// The engine owns this guard until the actor generation is stopped or dropped.
#[derive(Debug)]
pub struct ActorSenderOwner {
    sender: ActorSender,
}

impl ActorSenderOwner {
    pub fn new(path: &str) -> Self {
        Self {
            sender: ActorSender::new(path),
        }
    }
    pub fn sender(&self) -> ActorSender {
        self.sender.clone()
    }
    pub fn retire(&self) {
        self.sender.0.retired.store(true, Ordering::Release);
    }
}
impl Drop for ActorSenderOwner {
    fn drop(&mut self) {
        self.retire();
    }
}

#[derive(Clone, Debug)]
pub struct WeakActorSender(Weak<Generation>);
impl WeakActorSender {
    pub fn is_retired(&self) -> bool {
        self.0
            .upgrade()
            .is_none_or(|generation| generation.retired.load(Ordering::Acquire))
    }
}

tokio::task_local! { static CURRENT: ActorSender; }

impl ActorSender {
    /// Engines create one identity for each actor context generation.
    pub fn new(path: &str) -> Self {
        Self(Arc::new(Generation {
            path: path.to_owned(),
            incarnation: *uuid::Uuid::new_v4().as_bytes(),
            retired: AtomicBool::new(false),
        }))
    }

    pub fn path(&self) -> &str {
        &self.0.path
    }
    pub fn incarnation(&self) -> [u8; 16] {
        self.0.incarnation
    }
    pub fn is_retired(&self) -> bool {
        self.0.retired.load(Ordering::Acquire)
    }
    pub fn downgrade(&self) -> WeakActorSender {
        WeakActorSender(Arc::downgrade(&self.0))
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
