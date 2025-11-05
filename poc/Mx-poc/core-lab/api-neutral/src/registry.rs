//! Explicitly owned factory registration. No process-start side effects.
use crate::{panic_error, safe_drop, BoxedActorRef, EngineRuntime, ErasedActor};
use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, OnceLock, Weak};
type Factory = Box<dyn Fn() -> Result<Box<dyn ErasedActor>, String> + Send + Sync>;
struct FactoryOwner(Option<Factory>);
impl Drop for FactoryOwner {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            safe_drop(f);
        }
    }
}
struct Entry {
    generation: u64,
    factory: Arc<FactoryOwner>,
}
#[derive(Default)]
struct State {
    bindings: BTreeMap<String, Entry>,
    next_generation: u64,
}
#[derive(Default)]
struct Inner {
    state: Mutex<State>,
}
#[derive(Clone, Default)]
pub struct BindingRegistry {
    inner: Arc<Inner>,
}
#[must_use = "keep the registration alive while this binding should be available"]
pub struct Registration {
    registry: Weak<Inner>,
    name: String,
    generation: u64,
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let removed = {
                let mut state = registry.state.lock().unwrap();
                if state
                    .bindings
                    .get(&self.name)
                    .is_some_and(|e| e.generation == self.generation)
                {
                    state.bindings.remove(&self.name)
                } else {
                    None
                }
            };
            drop(removed); // user factory captures may reenter the registry
        }
    }
}
impl BindingRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(
        &self,
        name: &str,
        factory: impl Fn() -> Result<Box<dyn ErasedActor>, String> + Send + Sync + 'static,
    ) -> Result<Registration, String> {
        let owner = Arc::new(FactoryOwner(Some(Box::new(factory))));
        if name.is_empty() {
            return Err("empty binding name".into());
        }
        let mut state = self.inner.state.lock().unwrap();
        if state.bindings.contains_key(name) {
            return Err("binding already registered".into());
        }
        let generation = state.next_generation;
        state.next_generation = generation
            .checked_add(1)
            .ok_or("binding generation exhausted")?;
        state.bindings.insert(
            name.into(),
            Entry {
                generation,
                factory: owner,
            },
        );
        Ok(Registration {
            registry: Arc::downgrade(&self.inner),
            name: name.into(),
            generation,
        })
    }
    pub fn list(&self) -> Vec<String> {
        self.inner
            .state
            .lock()
            .unwrap()
            .bindings
            .keys()
            .cloned()
            .collect()
    }
    pub fn unregister(&self, name: &str) -> bool {
        let removed = self.inner.state.lock().unwrap().bindings.remove(name);
        let existed = removed.is_some();
        drop(removed);
        existed
    }
    /// Capture a registered factory, release the registry lock, then instantiate
    /// and initialize on the selected runtime. A concurrent unregister cannot
    /// revoke a spawn that has already captured its factory.
    pub fn spawn<E: EngineRuntime + ?Sized>(
        &self,
        name: &str,
        engine: &E,
        path: &str,
    ) -> Result<BoxedActorRef, String> {
        let factory = self
            .inner
            .state
            .lock()
            .unwrap()
            .bindings
            .get(name)
            .map(|e| e.factory.clone())
            .ok_or("binding not found")?;
        let actor = catch_unwind(AssertUnwindSafe(|| (factory.0.as_ref().unwrap())()))
            .map_err(panic_error)??;
        engine.spawn_erased(actor, path)
    }
}
fn global() -> &'static BindingRegistry {
    static REGISTRY: OnceLock<BindingRegistry> = OnceLock::new();
    REGISTRY.get_or_init(BindingRegistry::new)
}
pub fn register_engine_binding(
    name: &str,
    factory: impl Fn() -> Result<Box<dyn ErasedActor>, String> + Send + Sync + 'static,
) -> Result<Registration, String> {
    global().register(name, factory)
}
pub fn list_bindings() -> Vec<String> {
    global().list()
}
pub fn spawn_binding<E: EngineRuntime + ?Sized>(
    name: &str,
    engine: &E,
    path: &str,
) -> Result<BoxedActorRef, String> {
    global().spawn(name, engine, path)
}
