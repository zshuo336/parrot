//! Host-owned values stay guarded until the public caller claims the result.
//! These are ownership containers, not ActorRef adapters: dispatch, as_any and
//! clone behavior still belong to the factory's original reference.
use parrot_api::{address::ActorRef, types::BoxedActorRef};

pub(super) struct OwnedReference(Option<BoxedActorRef>);
impl OwnedReference {
    pub(super) fn release(mut self) -> bool {
        crate::disposal::drop_catching(self.0.take())
    }
    fn into_inner(mut self) -> BoxedActorRef {
        self.0.take().expect("owned reference")
    }
}
impl std::ops::Deref for OwnedReference {
    type Target = dyn ActorRef;
    fn deref(&self) -> &Self::Target {
        self.0.as_deref().expect("owned reference")
    }
}
impl Drop for OwnedReference {
    fn drop(&mut self) {
        if !crate::disposal::drop_catching(self.0.take()) {
            tracing::warn!("host-owned actor reference destructor panicked");
        }
    }
}

#[derive(Default)]
pub(super) struct OwnedReferences(Vec<OwnedReference>);
impl std::fmt::Debug for OwnedReferences {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedReferences")
            .field("len", &self.len())
            .finish()
    }
}
impl OwnedReferences {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }
    pub(super) fn push(&mut self, actor: BoxedActorRef) {
        self.0.push(OwnedReference(Some(actor)));
    }
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
    pub(super) fn pop(&mut self) -> Option<OwnedReference> {
        self.0.pop()
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &OwnedReference> {
        self.0.iter()
    }
    pub(super) fn into_inner(self) -> Vec<BoxedActorRef> {
        self.0.into_iter().map(OwnedReference::into_inner).collect()
    }
}
