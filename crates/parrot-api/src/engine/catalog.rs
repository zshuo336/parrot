//! Runtime discovery and registration ownership, independent of concrete engines.
use super::{BoxedActorRefErased, EngineRuntime, ErasedActor};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

struct Entry {
    identity: Arc<()>,
    runtime: Arc<dyn EngineRuntime>,
}
#[derive(Default)]
struct Inner {
    entries: Mutex<BTreeMap<String, Entry>>,
}

// The last directory handle owns all remaining references. Collection Drop
// cannot isolate independent user destructors, especially during caller unwind.
impl Drop for Inner {
    fn drop(&mut self) {
        let entries = self
            .entries
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        for (_, entry) in std::mem::take(entries) {
            drop(crate::owned_input::Input::new(entry.runtime));
        }
    }
}

/// An isolated, clonable runtime directory. Clones share the same directory;
/// separately constructed catalogs do not share registrations.
///
/// User methods and runtime destructors never execute under its mutex. Removing
/// a registration only removes discovery: it does not stop an engine or revoke
/// handles already returned by lookup. Engine shutdown remains the owner's job.
/// The last directory clone releases each retained runtime reference independently,
/// isolating destructor panics (including during caller unwinding). This is reference
/// disposal, not asynchronous shutdown; synchronous destructors must return.
#[derive(Clone, Default)]
pub struct EngineCatalog {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("engine already registered: {0}")]
pub struct RegistrationError(pub String);

/// An owned registration. It holds neither the runtime nor the catalog alive.
/// Dropping a stale token cannot remove a replacement with the same name.
#[must_use = "keep this token alive while the engine should be discoverable"]
pub struct EngineRegistration {
    catalog: Weak<Inner>,
    name: String,
    identity: Arc<()>,
}
impl EngineRegistration {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Remove this exact registration and return its runtime. Idempotent; a
    /// stale token returns None and leaves a replacement untouched.
    pub fn unregister(&self) -> Option<Arc<dyn EngineRuntime>> {
        let catalog = self.catalog.upgrade()?;
        let removed = {
            let mut entries = catalog.entries.lock().unwrap();
            if entries
                .get(&self.name)
                .is_some_and(|entry| Arc::ptr_eq(&entry.identity, &self.identity))
            {
                entries.remove(&self.name)
            } else {
                None
            }
        };
        removed.map(|entry| entry.runtime)
    }
}
impl Drop for EngineRegistration {
    fn drop(&mut self) {
        // Token cleanup can also run during unwinding. Isolate a user runtime's
        // destructor so it cannot cause a second unwind/abort here. Explicit
        // unregister returns ownership to callers who want to observe Drop.
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(self.unregister());
        })) {
            if let Err(secondary) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                std::mem::forget(secondary);
            }
        }
    }
}

impl EngineCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Persist until removal/replacement or catalog destruction. Returns any
    /// replaced runtime; the caller controls its release outside the lock.
    /// Empty names remain supported for compatibility with register_engine.
    /// Name panics propagate unchanged; independent input Drop panics are isolated.
    pub fn insert(&self, runtime: Arc<dyn EngineRuntime>) -> Option<Arc<dyn EngineRuntime>> {
        // Keep input disposal outside a failing user name callback's unwind.
        let mut runtime = crate::owned_input::Input::new(runtime);
        let name = runtime.value().name().to_string();
        // Preserve ordinary rejection/replacement ownership after name success.
        let runtime = runtime.take();
        let entry = Entry {
            identity: Arc::new(()),
            runtime,
        };
        let removed = self.inner.entries.lock().unwrap().insert(name, entry);
        removed.map(|entry| entry.runtime)
    }

    /// Register only if vacant. The runtime's name callback runs before locking;
    /// a panic leaves this operation uncommitted. Duplicate rejection leaves the
    /// existing entry intact, and releases rejected ownership outside the lock.
    /// Name panics preserve their original payload while input cleanup is isolated.
    /// After name success, explicit rejection retains its ordinary Drop behavior.
    pub fn register(
        &self,
        runtime: Arc<dyn EngineRuntime>,
    ) -> Result<EngineRegistration, RegistrationError> {
        // Keep input disposal outside a failing user name callback's unwind.
        let mut runtime = crate::owned_input::Input::new(runtime);
        let name = runtime.value().name().to_string();
        // Preserve ordinary rejection/replacement ownership after name success.
        let runtime = runtime.take();
        let identity = Arc::new(());
        let mut entries = self.inner.entries.lock().unwrap();
        if entries.contains_key(&name) {
            return Err(RegistrationError(name));
        }
        entries.insert(
            name.clone(),
            Entry {
                identity: identity.clone(),
                runtime,
            },
        );
        Ok(EngineRegistration {
            catalog: Arc::downgrade(&self.inner),
            name,
            identity,
        })
    }

    pub fn lookup(&self, name: &str) -> Option<Arc<dyn EngineRuntime>> {
        self.inner
            .entries
            .lock()
            .unwrap()
            .get(name)
            .map(|entry| entry.runtime.clone())
    }

    pub fn list(&self) -> Vec<String> {
        self.inner.entries.lock().unwrap().keys().cloned().collect()
    }

    pub fn remove(&self, name: &str) -> Option<Arc<dyn EngineRuntime>> {
        let removed = self.inner.entries.lock().unwrap().remove(name);
        removed.map(|entry| entry.runtime)
    }

    /// Resolve once, unlock, then invoke the selected runtime. A concurrent
    /// unregister/replacement cannot revoke a spawn that captured its runtime.
    /// This does not adapt asynchronous actors to the synchronous erased face.
    /// Delegate panics retain their original payload; captured runtime disposal
    /// is isolated during unwind. After a normal delegate return, capture Drop
    /// panics still propagate and the unreturned result is independently disposed.
    /// The delegate owns its actor argument and must avoid internal double panics.
    pub fn spawn(
        &self,
        engine: &str,
        actor: Box<dyn ErasedActor>,
        path: &str,
    ) -> Result<BoxedActorRefErased, String> {
        let runtime = self
            .lookup(engine)
            .ok_or_else(|| format!("engine not registered: {engine}"))?;
        // The selected runtime remains ours while its delegate may unwind.
        let mut runtime = crate::owned_input::Input::new(runtime);
        let mut result = crate::owned_input::Input::new(runtime.value().spawn_erased(actor, path));
        // A failed final capture disposal must not strand an unreturned reference.
        // Preserve its ordinary panic while independently disposing the result.
        drop(runtime.take());
        result.take()
    }
}
