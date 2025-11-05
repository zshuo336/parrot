use crate::common::{deadline, finish_shutdown, safe_drop, ActorOwner, Command, Errors};
use api_neutral::{
    ActorRefErased, BoxedActorRef, BoxedMessage, BoxedResult, EngineRuntime, ErasedActor, SendError,
};
use beam_sched::{Priority, ReductionScheduler};
use std::collections::BTreeMap;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
#[derive(Default)]
struct Retired {
    done: Mutex<bool>,
    wake: Condvar,
}
impl Retired {
    fn finish(&self) {
        *self.done.lock().unwrap() = true;
        self.wake.notify_all();
    }
    fn wait(&self, timeout: Duration) -> Result<(), String> {
        let end = deadline(timeout)?;
        let mut done = self.done.lock().unwrap();
        while !*done {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("actor retirement timed out".into());
            }
            done = self.wake.wait_timeout(done, left).unwrap().0;
        }
        Ok(())
    }
}
struct Slot {
    generation: u64,
}
#[derive(Default)]
struct State {
    paths: BTreeMap<String, Slot>,
    next_generation: u64,
    closing: bool,
}
struct Shared {
    scheduler: Arc<ReductionScheduler>,
    state: Mutex<State>,
    errors: Errors,
}
pub struct SharedEngine {
    shared: Arc<Shared>,
    start_timeout: Duration,
}
impl SharedEngine {
    pub fn new(workers: usize) -> Result<Self, String> {
        Self::with_start_timeout(workers, Duration::from_secs(5))
    }
    pub fn with_start_timeout(workers: usize, timeout: Duration) -> Result<Self, String> {
        deadline(timeout)?;
        Ok(Self {
            shared: Arc::new(Shared {
                scheduler: ReductionScheduler::new(workers).map_err(|e| e.to_string())?,
                state: Mutex::new(State::default()),
                errors: Arc::new(Mutex::new(vec![])),
            }),
            start_timeout: timeout,
        })
    }
}
struct Request {
    command: Option<Command>,
    reply: Option<Sender<BoxedResult>>,
    deadline: Option<Instant>,
}
impl Request {
    fn complete(&mut self, result: BoxedResult) {
        if let Some(reply) = self.reply.take() {
            safe_drop(reply.send(result));
        } else {
            safe_drop(result);
        }
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if let Some(command) = self.command.take() {
            safe_drop(command);
        }
        if let Some(reply) = self.reply.take() {
            safe_drop(reply.send(Err("actor stopped before completion".into())));
        }
    }
}
impl EngineRuntime for SharedEngine {
    fn name(&self) -> &'static str {
        "shared-reduction"
    }
    fn spawn_erased(
        &self,
        actor: Box<dyn ErasedActor>,
        path: &str,
    ) -> Result<BoxedActorRef, String> {
        let startup = Arc::new(Mutex::new(Some(ActorOwner::new(
            actor,
            self.shared.errors.clone(),
        ))));
        if path.is_empty() {
            return Err("empty actor path".into());
        }
        let mut state = self.shared.state.lock().unwrap();
        if state.closing {
            return Err("engine stopped".into());
        }
        if state.paths.contains_key(path) {
            return Err("actor path already registered".into());
        }
        let generation = state.next_generation;
        state.next_generation = generation.checked_add(1).ok_or("generation exhausted")?;
        let retired = Arc::new(Retired::default());
        let completion = retired.clone();
        let registry = Arc::downgrade(&self.shared);
        let registered_path = path.to_string();
        startup.lock().unwrap().as_mut().unwrap().retired = Some(Box::new(move || {
            if let Some(registry) = registry.upgrade() {
                let mut state = registry.state.lock().unwrap();
                if state
                    .paths
                    .get(&registered_path)
                    .is_some_and(|s| s.generation == generation)
                {
                    state.paths.remove(&registered_path);
                }
            }
            completion.finish();
        }));
        state.paths.insert(path.into(), Slot { generation });
        let worker_startup = startup.clone();
        let mut owned = None;
        let result = self.shared.scheduler.spawn(
            Box::new(move |_, message| {
                if owned.is_none() {
                    owned = worker_startup.lock().unwrap().take();
                }
                let mut request = *message
                    .downcast::<Request>()
                    .expect("private scheduler message");
                if request.deadline.is_some_and(|d| Instant::now() >= d) {
                    request.complete(Err("ask expired before execution".into()));
                    return true;
                }
                let command = request.command.take().unwrap();
                let start = matches!(&command, Command::Start);
                let result = owned.as_mut().unwrap().apply(command);
                let keep_running = !start || result.is_ok();
                request.complete(result);
                keep_running
            }),
            0,
        );
        let id = match result {
            Ok(id) => id,
            Err(error) => {
                state.paths.remove(path);
                drop(state);
                return Err(error.to_string());
            }
        };
        drop(state);
        drop(startup);
        let reference = Arc::new(Reference {
            shared: self.shared.clone(),
            id,
            path: path.into(),
            generation,
            retired,
        });
        if let Err(error) = reference.ask_command(Command::Start, self.start_timeout) {
            reference.stop();
            let cleanup = reference.wait_stopped(self.start_timeout);
            return Err(match cleanup {
                Ok(()) => error,
                Err(e) => format!("{error}; cleanup pending: {e}"),
            });
        }
        Ok(reference)
    }
    fn shutdown(&self) -> Result<(), String> {
        self.shared.state.lock().unwrap().closing = true;
        finish_shutdown(
            self.shared.scheduler.shutdown().map_err(|e| e.to_string()),
            &self.shared.errors,
        )
    }
    fn worker_counts(&self) -> (usize, usize) {
        self.shared.scheduler.worker_counts()
    }
}
impl Drop for SharedEngine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
struct Reference {
    shared: Arc<Shared>,
    id: usize,
    path: String,
    generation: u64,
    retired: Arc<Retired>,
}
impl Reference {
    fn submit(&self, request: Request) -> Result<(), (String, Request)> {
        self.shared
            .scheduler
            .send(self.id, Box::new(request), Priority::Normal)
            .map_err(|(error, request)| {
                (
                    error.to_string(),
                    *request
                        .downcast::<Request>()
                        .expect("private scheduler rejection"),
                )
            })
    }
    fn ask_command(&self, command: Command, timeout: Duration) -> BoxedResult {
        let end = match deadline(timeout) {
            Ok(end) => end,
            Err(error) => {
                safe_drop(command);
                return Err(error);
            }
        };
        let (tx, rx) = channel();
        if let Err((error, request)) = self.submit(Request {
            command: Some(command),
            reply: Some(tx),
            deadline: Some(end),
        }) {
            drop(request);
            return Err(error);
        }
        rx.recv_timeout(end.saturating_duration_since(Instant::now()))
            .map_err(|e| e.to_string())?
    }
}
impl Drop for Reference {
    fn drop(&mut self) {
        let _ = self.shared.scheduler.stop(self.id);
    }
}
impl ActorRefErased for Reference {
    fn tell(&self, message: BoxedMessage) -> Result<(), SendError> {
        self.submit(Request {
            command: Some(Command::Message(message)),
            reply: None,
            deadline: None,
        })
        .map_err(
            |(reason, mut request)| match request.command.take().unwrap() {
                Command::Message(message) => SendError { reason, message },
                Command::Start => unreachable!("business command changed"),
            },
        )
    }
    fn ask_with_timeout(&self, message: BoxedMessage, timeout: Duration) -> BoxedResult {
        self.ask_command(Command::Message(message), timeout)
    }
    fn path(&self) -> String {
        self.path.clone()
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn stop(&self) {
        let _ = self.shared.scheduler.stop(self.id);
    }
    fn wait_stopped(&self, timeout: Duration) -> Result<(), String> {
        self.retired.wait(timeout)
    }
}
