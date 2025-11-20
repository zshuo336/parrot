use parrot::thread::{
    BackpressureStrategy, SchedulingMode, SupervisorStrategy, ThreadActor, ThreadActorConfig,
    ThreadContext, mailbox::admission::MessageAdmission,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorPath, ActorRef},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Debug, Default)]
pub struct Counts {
    pub initialized: AtomicUsize,
    pub actor_drops: AtomicUsize,
    pub policy_drops: AtomicUsize,
    pub seen: Mutex<Vec<u64>>,
    pub batches: Mutex<Vec<usize>>,
}

pub struct Probe {
    pub counts: Arc<Counts>,
    pub panic_drop: bool,
    pub coalesce: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.actor_drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "processor actor disposal panic");
    }
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.counts.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn can_batch_message(&self, _: &BoxedMessage) -> bool {
        self.coalesce
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.counts
            .seen
            .lock()
            .unwrap()
            .push(*message.downcast_ref::<u64>().unwrap());
        Box::pin(async { Ok(message) })
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        self.counts.batches.lock().unwrap().push(messages.len());
        Box::pin(async move {
            let mut results = Vec::new();
            for message in messages {
                results.push(self.receive_message(message, context).await);
            }
            results
        })
    }
}

#[derive(Debug)]
pub struct Policy {
    pub counts: Arc<Counts>,
    pub panic_drop: bool,
}
impl Drop for Policy {
    fn drop(&mut self) {
        self.counts.policy_drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "processor policy disposal panic");
    }
}
impl MessageAdmission for Policy {
    fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, parrot::thread::MailboxError> {
        Ok(message)
    }
}

pub fn inputs(
    counts: &Arc<Counts>,
    panic_drop: bool,
    coalesce: bool,
) -> (ThreadActor<Probe>, ThreadContext<Probe>, Weak<dyn ActorRef>) {
    let path = ActorPath::placeholder("/raw-processor");
    let context_owner = Arc::downgrade(&path.target);
    let context = ThreadContext::new(
        Weak::default(),
        tokio::runtime::Handle::current(),
        path,
        None,
        SupervisorStrategy::Stop,
    );
    let actor = ThreadActor::new_for_test(Probe {
        counts: counts.clone(),
        panic_drop,
        coalesce,
    });
    (actor, context, context_owner)
}

pub fn invalid_configs() -> Vec<(&'static str, ThreadActorConfig)> {
    vec![
        (
            "actor.message_batch_size",
            ThreadActorConfig {
                message_batch_size: Some(0),
                ..Default::default()
            },
        ),
        (
            "actor.message_batch_size",
            ThreadActorConfig {
                message_batch_size: Some(257),
                ..Default::default()
            },
        ),
        (
            "actor.mailbox_capacity",
            ThreadActorConfig {
                mailbox_capacity: Some(0),
                ..Default::default()
            },
        ),
        (
            "actor.mailbox_capacity",
            ThreadActorConfig {
                mailbox_capacity: Some(parrot_config::ThreadResolved::MAX_QUEUE_CAPACITY + 1),
                ..Default::default()
            },
        ),
        (
            "actor.thread_stack_size",
            ThreadActorConfig {
                thread_stack_size: Some(ThreadActorConfig::MAX_THREAD_STACK_SIZE + 1),
                ..Default::default()
            },
        ),
        (
            "actor.ask_timeout",
            ThreadActorConfig {
                ask_timeout: Some(Duration::MAX),
                ..Default::default()
            },
        ),
        (
            "actor.idle_sleep_duration",
            ThreadActorConfig {
                idle_sleep_duration: Some(Duration::MAX),
                ..Default::default()
            },
        ),
        (
            "actor.supervisor_strategy.within",
            ThreadActorConfig {
                supervisor_strategy: Some(SupervisorStrategy::Restart {
                    max_retries: 1,
                    within: Duration::MAX,
                }),
                ..Default::default()
            },
        ),
        (
            "actor.scheduling_mode.max_messages_per_run",
            ThreadActorConfig {
                scheduling_mode: Some(SchedulingMode::SharedPool {
                    max_messages_per_run: 0,
                }),
                backpressure_strategy: Some(BackpressureStrategy::Error),
                ..Default::default()
            },
        ),
    ]
}

pub fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic")
}
