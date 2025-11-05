use crate::mailbox::{Control, Mailbox};
use crate::{panic_error, safe_drop, Owned, TypedActorRef, TypedReceive};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;
thread_local! { static WORKER_OF: Cell<usize> = const { Cell::new(0) }; }
struct Entry {
    generation: u64,
    control: Arc<dyn Control>,
}
#[derive(Default)]
struct State {
    actors: BTreeMap<String, Entry>,
    next_generation: u64,
    closing: bool,
}
struct Shared {
    state: Mutex<State>,
    started: AtomicUsize,
    exited: AtomicUsize,
    failures: AtomicUsize,
}
struct WorkerExit(Arc<Shared>);
impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.exited.fetch_add(1, Ordering::SeqCst);
        WORKER_OF.set(0);
    }
}
/// Owns every worker and the shared path namespace. Static payloads remain in
/// their concrete Mailbox<M,R>; only stop control is erased in the registry.
pub struct SimpleEngine {
    shared: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    capacity: usize,
}
impl SimpleEngine {
    pub fn new() -> Arc<Self> {
        Self::with_capacity(1024).unwrap()
    }
    pub fn with_capacity(capacity: usize) -> Result<Arc<Self>, String> {
        if capacity == 0 {
            return Err("mailbox capacity must be positive".into());
        }
        Ok(Arc::new(Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                started: AtomicUsize::new(0),
                exited: AtomicUsize::new(0),
                failures: AtomicUsize::new(0),
            }),
            workers: Mutex::new(vec![]),
            capacity,
        }))
    }
    pub fn spawn_typed<A, M>(
        &self,
        actor: A,
        path: &str,
    ) -> Result<TypedActorRef<M, A::Reply>, String>
    where
        A: TypedReceive<M>,
        M: Send + 'static,
    {
        // Separate owner prevents user Drop inside a registry lock even when
        // thread creation fails and drops its closure before returning Err.
        let startup = Arc::new(Mutex::new(Owned(Some(actor))));
        let mailbox = Arc::new(Mailbox::new(self.capacity));
        if path.is_empty() {
            return Err("empty actor path".into());
        }
        let mut state = self.shared.state.lock().unwrap();
        if state.closing {
            return Err("engine stopped".into());
        }
        if state.actors.contains_key(path) {
            return Err("actor path already registered".into());
        }
        let generation = state.next_generation;
        state.next_generation = generation.checked_add(1).ok_or("generation exhausted")?;
        let mut workers = self.workers.lock().unwrap();
        let mut i = 0;
        while i < workers.len() {
            if workers[i].is_finished() {
                if workers.swap_remove(i).join().is_err() {
                    self.shared.failures.fetch_add(1, Ordering::SeqCst);
                }
            } else {
                i += 1;
            }
        }
        state.actors.insert(
            path.into(),
            Entry {
                generation,
                control: mailbox.clone(),
            },
        );
        let shared = self.shared.clone();
        let queue = mailbox.clone();
        let owned = startup.clone();
        let worker_path = path.to_string();
        match std::thread::Builder::new()
            .name(format!("poc-typed-{generation}"))
            .spawn(move || worker(shared, queue, owned, worker_path, generation))
        {
            Ok(handle) => workers.push(handle),
            Err(error) => {
                state.actors.remove(path);
                drop(workers);
                drop(state);
                return Err(format!("thread spawn: {error}"));
            }
        }
        drop(workers);
        drop(state);
        Ok(TypedActorRef::new(mailbox, path.into(), generation))
    }
    pub fn actor<A, M>(&self, actor: A, path: &str) -> Result<TypedActorRef<M, A::Reply>, String>
    where
        A: TypedReceive<M>,
        M: Send + 'static,
    {
        self.spawn_typed(actor, path)
    }
    pub fn stop(&self, path: &str) -> Result<(), String> {
        let control = self
            .shared
            .state
            .lock()
            .unwrap()
            .actors
            .get(path)
            .map(|e| e.control.clone())
            .ok_or("actor not found")?;
        control.close();
        Ok(())
    }
    pub fn alive_count(&self) -> usize {
        self.shared.state.lock().unwrap().actors.len()
    }
    pub fn worker_counts(&self) -> (usize, usize) {
        (
            self.shared.started.load(Ordering::SeqCst),
            self.shared.exited.load(Ordering::SeqCst),
        )
    }
    pub fn request_shutdown(&self) {
        let controls = {
            let mut state = self.shared.state.lock().unwrap();
            state.closing = true;
            state
                .actors
                .values()
                .map(|e| e.control.clone())
                .collect::<Vec<_>>()
        };
        for control in controls {
            control.close();
        }
    }
    pub fn shutdown(&self) -> Result<(), String> {
        self.request_shutdown();
        if WORKER_OF.get() == Arc::as_ptr(&self.shared) as usize {
            return Err("shutdown requested from worker; cannot join itself".into());
        }
        let mut workers = self.workers.lock().unwrap();
        for worker in workers.drain(..) {
            if worker.join().is_err() {
                self.shared.failures.fetch_add(1, Ordering::SeqCst);
            }
        }
        if self.shared.failures.load(Ordering::SeqCst) != 0 {
            Err("worker panicked outside actor boundary".into())
        } else {
            Ok(())
        }
    }
}
impl Drop for SimpleEngine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
fn worker<A, M>(
    shared: Arc<Shared>,
    mailbox: Arc<Mailbox<M, A::Reply>>,
    startup: Arc<Mutex<Owned<A>>>,
    path: String,
    generation: u64,
) where
    A: TypedReceive<M>,
    M: Send + 'static,
{
    WORKER_OF.set(Arc::as_ptr(&shared) as usize);
    shared.started.fetch_add(1, Ordering::SeqCst);
    let _exit = WorkerExit(shared.clone());
    let mut actor = startup.lock().unwrap().0.take().unwrap();
    drop(startup);
    while let Some(mut work) = mailbox.take() {
        let failed = if work.deadline.is_some_and(|d| Instant::now() >= d) {
            work.complete(Err("ask expired before execution".into()));
            false
        } else {
            match catch_unwind(AssertUnwindSafe(|| {
                actor.receive_typed(work.message.take().unwrap())
            })) {
                Ok(reply) => {
                    work.complete(reply);
                    false
                }
                Err(payload) => {
                    work.complete(Err(panic_error(payload)));
                    true
                }
            }
        };
        mailbox.complete_one();
        drop(work);
        if failed {
            break;
        }
    }
    mailbox.request_stop();
    safe_drop(actor);
    drop(mailbox.drain());
    {
        let mut state = shared.state.lock().unwrap();
        assert_eq!(state.actors.get(&path).unwrap().generation, generation);
        state.actors.remove(&path);
    }
    mailbox.retired();
}
