//! Example: running a compute-heavy actor on a dedicated OS thread.
//!
//! Demonstrates the `DedicatedThreadScheduler`: one OS thread per actor,
//! each with its own single-threaded Tokio runtime.

use std::sync::Arc;
use std::time::Duration;

use parrot::thread::actor::ThreadActor;
use parrot::thread::address::ThreadActorRef;
use parrot::thread::config::{BackpressureStrategy, SchedulingMode, ThreadActorConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::mailbox::Mailbox;
use parrot::thread::mailbox::mpsc::MpscMailbox;
use parrot::thread::processor::ActorProcessor;
use parrot::thread::scheduler::dedicated_thread::{
    DedicatedThreadConfig, DedicatedThreadScheduler, TypedThreadSchedulerExt,
};
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorPath;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

/// A compute-heavy actor that folds numbers.
struct ComputeActor {
    counter: u64,
}

impl ComputeActor {
    fn new() -> Self {
        Self { counter: 0 }
    }
}

impl Actor for ComputeActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(n) = msg.downcast_ref::<u64>() {
                self.counter = self.counter.wrapping_add(*n);
                return Ok(Box::new(self.counter) as BoxedMessage);
            }
            Ok(msg)
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    // 1. Create the dedicated-thread scheduler.
    let scheduler = DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
        max_threads: 4,
        idle_sleep_duration: Duration::from_millis(5),
        ..Default::default()
    }));

    // 2. Build the actor stack: mailbox + processor.
    let path_str = "user/compute-1";
    let actor_path = ActorPath::placeholder(path_str);
    let mailbox = Arc::new(MpscMailbox::new(1024, actor_path.clone()));

    let context = ThreadContext::<ComputeActor>::new_for_test(path_str);
    let processor = Arc::new(ActorProcessor::<ComputeActor>::new(
        ThreadActor::new_for_test(ComputeActor::new()),
        context,
        path_str.to_string(),
        ThreadActorConfig::default(),
    ));
    mailbox
        .set_processor(processor.clone() as Arc<dyn parrot::thread::processor::ProcessorInterface>);

    // 3. Schedule the actor on a dedicated OS thread.
    scheduler
        .schedule_typed_by_processor::<ComputeActor>(
            path_str,
            mailbox.clone(),
            processor,
            ThreadActorConfig {
                scheduling_mode: Some(SchedulingMode::DedicatedThread),
                ..Default::default()
            },
        )
        .expect("failed to schedule dedicated actor");

    // 4. Create an actor ref and send work.
    let actor_ref = ThreadActorRef::<ComputeActor>::new(
        actor_path,
        Arc::downgrade(&mailbox) as parrot::thread::mailbox::WeakMailboxRef,
        BackpressureStrategy::Block,
        Duration::from_secs(2),
        None,
    );

    for value in [100u64, 250, 33] {
        let result = actor_ref.ask(Box::new(value) as BoxedMessage).await;
        match result {
            Ok(reply) => {
                let total = reply.downcast_ref::<u64>().copied().unwrap_or(0);
                println!("after adding {value}: total = {total}");
            }
            Err(e) => {
                eprintln!("ask failed: {e}");
                break;
            }
        }
    }

    // 5. Shut the scheduler down.
    scheduler.shutdown().await.expect("shutdown failed");
    println!("dedicated-thread example finished");
}
