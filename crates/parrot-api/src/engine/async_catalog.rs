//! Open production runtime directory. Lookup returns a real async engine handle.
use super::{AsyncEngineRuntime, RegistrationError};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

struct Entry {
    identity: Arc<()>,
    runtime: Arc<dyn AsyncEngineRuntime>,
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
pub struct AsyncEngineCatalog {
    inner: Arc<Inner>,
}

/// An owned registration. It holds neither the runtime nor the catalog alive.
/// Dropping a stale token cannot remove a replacement with the same name.
#[must_use = "keep this token alive while the engine should be discoverable"]
pub struct AsyncEngineRegistration {
    catalog: Weak<Inner>,
    name: String,
    identity: Arc<()>,
}
impl AsyncEngineRegistration {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Remove this exact registration and return its runtime. Idempotent; a
    /// stale token returns None and leaves a replacement untouched.
    pub fn unregister(&self) -> Option<Arc<dyn AsyncEngineRuntime>> {
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
impl Drop for AsyncEngineRegistration {
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

impl AsyncEngineCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Persist until removal/replacement or catalog destruction. Returns any
    /// replaced runtime; the caller controls its release outside the lock.
    /// Names are opaque keys, including the empty string; aliases are allowed.
    /// Name panics propagate unchanged; independent input Drop panics are isolated.
    pub fn insert(
        &self,
        name: impl Into<String>,
        runtime: Arc<dyn AsyncEngineRuntime>,
    ) -> Option<Arc<dyn AsyncEngineRuntime>> {
        // Keep input disposal outside a failing user name callback's unwind.
        let mut runtime = crate::owned_input::Input::new(runtime);
        let name = name.into();
        // Preserve ordinary rejection/replacement ownership after name success.
        let runtime = runtime.take();
        let entry = Entry {
            identity: Arc::new(()),
            runtime,
        };
        let removed = self.inner.entries.lock().unwrap().insert(name, entry);
        removed.map(|entry| entry.runtime)
    }

    /// Register only if vacant. Name conversion runs before locking;
    /// a panic leaves this operation uncommitted. Duplicate rejection leaves the
    /// existing entry intact, and releases rejected ownership outside the lock.
    /// Name panics preserve their original payload while input cleanup is isolated.
    /// After name success, explicit rejection retains its ordinary Drop behavior.
    pub fn register(
        &self,
        name: impl Into<String>,
        runtime: Arc<dyn AsyncEngineRuntime>,
    ) -> Result<AsyncEngineRegistration, RegistrationError> {
        // Keep input disposal outside a failing user name callback's unwind.
        let mut runtime = crate::owned_input::Input::new(runtime);
        let name = name.into();
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
        Ok(AsyncEngineRegistration {
            catalog: Arc::downgrade(&self.inner),
            name,
            identity,
        })
    }

    pub fn lookup(&self, name: &str) -> Option<Arc<dyn AsyncEngineRuntime>> {
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

    pub fn remove(&self, name: &str) -> Option<Arc<dyn AsyncEngineRuntime>> {
        let removed = self.inner.entries.lock().unwrap().remove(name);
        removed.map(|entry| entry.runtime)
    }

    // Resolve the runtime before constructing spawn inputs. Missing lookup
    // therefore never consumes an actor or config whose cleanup needs an engine.
}
