use super::*;
use crate::thread::{
    actor::ThreadActor, config::ThreadActorConfig, context::ThreadContext, envelope::AskEnvelope,
    mailbox::mpsc::MpscMailbox, processor::ActorProcessor, scheduler::queue::Lane,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::mpsc;

struct Probe {
    entered: Option<Arc<Notify>>,
    release: Option<Mutex<mpsc::Receiver<()>>>,
    seen: Arc<Mutex<Vec<u64>>>,
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = *message.downcast_ref::<u64>().unwrap();
            if let Some(entered) = self.entered.take() {
                entered.notify_one();
                self.release
                    .take()
                    .unwrap()
                    .into_inner()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .expect("release blocked owner");
            }
            self.seen.lock().unwrap().push(value);
            Ok(message)
        })
    }
}
fn mailbox(name: &str, actor: Probe) -> Arc<dyn Mailbox> {
    let mailbox = Arc::new(MpscMailbox::new(8, ActorPath::placeholder(name)));
    mailbox.set_processor(Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(actor),
        ThreadContext::new_for_test(name),
        name.into(),
        ThreadActorConfig::default(),
    )));
    mailbox
}
struct Release(Option<mpsc::SyncSender<()>>);
impl Release {
    fn open(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        self.open();
    }
}

async fn blocked_owner(thief: usize) {
    let queue = Arc::new(SchedulingQueue::with_workers(2, 2));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let a = mailbox(
        "/steal/a",
        Probe {
            entered: None,
            release: None,
            seen: seen.clone(),
        },
    );
    let entered = Arc::new(Notify::new());
    let (release, wait) = mpsc::sync_channel(1);
    let mut release = Release(Some(release));
    let b = mailbox(
        "/steal/b",
        Probe {
            entered: Some(entered.clone()),
            release: Some(Mutex::new(wait)),
            seen: seen.clone(),
        },
    );
    let mut replies = Vec::new();
    for (mailbox, values) in [(&a, vec![1u64, 2]), (&b, vec![99u64])] {
        for value in values {
            let (ask, reply) = AskEnvelope::with_boxed(Box::new(value));
            mailbox
                .push_ask(ask, crate::thread::BackpressureStrategy::Error)
                .await
                .unwrap();
            replies.push(reply);
        }
        assert!(queue.register(mailbox));
        assert!(mailbox.schedule_state().try_enqueue());
        assert!(queue.enqueue_registered(mailbox.clone(), Lane::Normal));
    }
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = |id| {
        Worker::new(
            id,
            Handle::current(),
            queue.clone(),
            shutdown.clone(),
            WorkerConfig {
                batch_size: 1,
                idle_sleep_duration: Duration::from_millis(1),
                ..Default::default()
            },
        )
    };
    let owner = worker(0).spawn();
    let owner_blocked = tokio::time::timeout(Duration::from_secs(3), entered.notified()).await;
    let local_before = queue.local_len();
    let thief = worker(thief).spawn();
    let second = replies.remove(1);
    let outcome = tokio::time::timeout(Duration::from_secs(3), second).await;
    let stolen_before_release = queue.steals();
    let observed_before_release = seen.lock().unwrap().clone();
    release.open();
    let mut values = Vec::new();
    for reply in replies {
        values.push(
            tokio::time::timeout(Duration::from_secs(3), reply)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        );
    }
    shutdown.store(true, Ordering::Release);
    queue.notify_handle().notify_waiters();
    tokio::time::timeout(Duration::from_secs(3), owner)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), thief)
        .await
        .unwrap()
        .unwrap();
    a.close().await;
    b.close().await;
    assert!(owner_blocked.is_ok());
    assert_eq!(
        local_before, 1,
        "first worker must requeue a before blocking on b"
    );
    assert_eq!(
        *outcome
            .unwrap()
            .unwrap()
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        2
    );
    assert_eq!(
        observed_before_release,
        vec![1, 2],
        "b must remain blocked through the stolen reply"
    );
    assert_eq!(
        stolen_before_release, 1,
        "worker must consume a remote local entry, not only global injection"
    );
    assert_eq!(*values.remove(0).downcast::<u64>().unwrap(), 1);
    assert_eq!(*values.remove(0).downcast::<u64>().unwrap(), 99);
    assert_eq!(*seen.lock().unwrap(), vec![1, 2, 99]);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn idle_core_worker_steals_a_real_batch_while_its_owner_is_blocked() {
    blocked_owner(1).await;
}

#[tokio::test]
async fn burst_worker_without_a_local_slot_steals_and_delivers_the_original_reply() {
    blocked_owner(usize::MAX).await;
}
