//! Known facade composition is acyclic, even across simultaneous registrations.
//! Metadata contains only weak nodes, never engines or user callbacks.
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard, Weak},
};
static MUTATION: Mutex<()> = Mutex::new(());
pub(super) fn mutation() -> MutexGuard<'static, ()> {
    MUTATION.lock().unwrap()
}
#[derive(Default)]
pub(super) struct Node {
    edges: Mutex<HashMap<String, Weak<Node>>>,
}
impl Node {
    /// Caller holds the global metadata mutation guard through registry commit.
    pub(super) fn accepts(self: &Arc<Self>, child: &Arc<Self>) -> bool {
        let mut seen = HashSet::new();
        let mut pending = vec![child.clone()];
        while let Some(node) = pending.pop() {
            if Arc::ptr_eq(self, &node) {
                return false;
            }
            if seen.insert(Arc::as_ptr(&node) as usize) {
                pending.extend(
                    node.edges
                        .lock()
                        .unwrap()
                        .values()
                        .filter_map(Weak::upgrade),
                );
            }
        }
        true
    }
    pub(super) fn set(&self, name: &str, child: Option<&Arc<Self>>) {
        let mut edges = self.edges.lock().unwrap();
        match child {
            Some(child) => {
                edges.insert(name.to_owned(), Arc::downgrade(child));
            }
            None => {
                edges.remove(name);
            }
        }
    }
}
