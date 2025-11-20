use parrot::thread::{
    ActorProcessor, BackpressureStrategy, ProcessorInterface, ThreadActor, ThreadActorConfig,
    ThreadContext,
    envelope::ControlMessage,
    mailbox::{Mailbox, mpsc::MpscMailbox},
    processor::ProcessorStatus,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

#[derive(Clone, Copy)]
enum Init {
    Ready,
    Pending,
    Error,
    Panic,
}

#[derive(Default)]
struct Counts {
    constructed: AtomicUsize,
    entered: AtomicUsize,
    completed: AtomicUsize,
    disposed: AtomicUsize,
    messages: AtomicUsize,
    stopped: AtomicUsize,
    release: AtomicBool,
}

struct Initialization {
    counts: Arc<Counts>,
    mode: Init,
    entered: bool,
}
impl Future for Initialization {
    type Output = ActorResult<()>;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.entered {
            self.entered = true;
            self.counts.entered.fetch_add(1, Ordering::SeqCst);
        }
        match self.mode {
            Init::Error => Poll::Ready(Err(ActorError::InitializationError("init refused".into()))),
            Init::Panic => panic!("initialization contract panic"),
            Init::Pending if !self.counts.release.load(Ordering::SeqCst) => Poll::Pending,
            Init::Ready | Init::Pending => {
                self.counts.completed.fetch_add(1, Ordering::SeqCst);
                Poll::Ready(Ok(()))
            }
        }
    }
}
impl Drop for Initialization {
    fn drop(&mut self) {
        self.counts.disposed.fetch_add(1, Ordering::SeqCst);
    }
}

struct Probe {
    counts: Arc<Counts>,
    mode: Init,
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.counts.constructed.fetch_add(1, Ordering::SeqCst);
        Box::pin(Initialization {
            counts: self.counts.clone(),
            mode: self.mode,
            entered: false,
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.counts.messages.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(73_u64) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.stopped.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn actor(mode: Init) -> (ThreadActor<Probe>, ThreadContext<Probe>, Arc<Counts>) {
    let counts = Arc::new(Counts::default());
    (
        ThreadActor::new_for_test(Probe {
            counts: counts.clone(),
            mode,
        }),
        ThreadContext::new_for_test("/initialization-contract"),
        counts,
    )
}

fn processor(mode: Init) -> (Arc<ActorProcessor<Probe>>, Arc<Counts>) {
    let (actor, context, counts) = actor(mode);
    (
        Arc::new(ActorProcessor::new(
            actor,
            context,
            "/initialization-contract".into(),
            ThreadActorConfig::default(),
        )),
        counts,
    )
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(std::task::Waker::noop()))
}

async fn assert_dispatches(processor: &ActorProcessor<Probe>, counts: &Counts) {
    let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(
        2,
        ActorPath::placeholder("/initialization-contract"),
    ));
    mailbox
        .push(Box::new(()), BackpressureStrategy::Error)
        .await
        .unwrap();
    let before = counts.messages.load(Ordering::SeqCst);
    assert_eq!(
        processor
            .process_batch_of_messages(&mailbox, 1, false)
            .await
            .unwrap(),
        (1, 0)
    );
    assert_eq!(counts.messages.load(Ordering::SeqCst), before + 1);
}

#[tokio::test]
async fn thread_start_before_initialization_is_rejected_without_dispatch() {
    let (mut actor, mut context, counts) = actor(Init::Ready);
    let started = actor
        .process_message(Box::new(ControlMessage::Start), &mut context)
        .await;
    assert!(started.is_err(), "uninitialized actor accepted Start");
    assert_eq!(actor.state(), ActorState::Starting);
    assert!(
        actor
            .process_message(Box::new(()), &mut context)
            .await
            .is_err()
    );
    assert_eq!(counts.entered.load(Ordering::SeqCst), 0);
    assert_eq!(counts.messages.load(Ordering::SeqCst), 0);
    actor.initialize(&mut context).await.unwrap();
    actor
        .process_message(Box::new(ControlMessage::Start), &mut context)
        .await
        .unwrap();
    assert!(
        actor
            .process_message(Box::new(()), &mut context)
            .await
            .is_ok()
    );
    assert_eq!(counts.messages.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn processor_start_before_initialization_is_rejected_without_poisoning_admission() {
    let (processor, counts) = processor(Init::Ready);
    assert!(
        processor.start_actor().await.is_err(),
        "uninitialized processor accepted Start"
    );
    assert_eq!(processor.get_status(), ProcessorStatus::Initializing);
    assert!(!processor.has_failed());
    processor.initialize_and_start().await.unwrap();
    assert_dispatches(&processor, &counts).await;
    assert_eq!(counts.entered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_thread_initialization_cannot_restart_or_run_stop_hook() {
    let (mut actor, mut context, counts) = actor(Init::Pending);
    let mut initializing = Box::pin(actor.initialize(&mut context));
    assert!(poll_once(initializing.as_mut()).is_pending());
    drop(initializing);
    assert_eq!(counts.entered.load(Ordering::SeqCst), 1);
    assert_eq!(counts.disposed.load(Ordering::SeqCst), 1);
    assert_eq!(actor.state(), ActorState::Stopped);
    assert!(
        actor
            .process_message(Box::new(ControlMessage::Start), &mut context)
            .await
            .is_err()
    );
    assert!(actor.initialize(&mut context).await.is_err());
    actor.shutdown(&mut context).await.unwrap();
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 1);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_processor_initialization_is_terminal_and_retires_without_normal_hook() {
    let (processor, counts) = processor(Init::Pending);
    let mut initializing = Box::pin(processor.initialize_actor());
    assert!(poll_once(initializing.as_mut()).is_pending());
    drop(initializing);
    assert_eq!(counts.disposed.load(Ordering::SeqCst), 1);
    assert_eq!(processor.get_status(), ProcessorStatus::Failed);
    assert!(processor.has_failed());
    assert!(processor.start_actor().await.is_err());
    assert!(processor.initialize_actor().await.is_err());
    processor.stop_actor().await.unwrap();
    processor.clone().retire_erased().await.unwrap();
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 1);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_thread_initialization_never_accepts_start() {
    for mode in [Init::Error, Init::Panic] {
        let (mut actor, mut context, counts) = actor(mode);
        assert!(actor.initialize(&mut context).await.is_err());
        assert!(
            actor
                .process_message(Box::new(ControlMessage::Start), &mut context)
                .await
                .is_err()
        );
        assert_eq!(actor.state(), ActorState::Stopped);
        actor.shutdown(&mut context).await.unwrap();
        assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn failed_processor_initialization_cannot_be_reported_running_by_start() {
    for mode in [Init::Error, Init::Panic] {
        let (processor, counts) = processor(mode);
        assert!(processor.initialize_actor().await.is_err());
        assert!(processor.start_actor().await.is_err());
        assert_eq!(processor.get_status(), ProcessorStatus::Failed);
        assert!(processor.has_failed());
        assert_eq!(counts.completed.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn stopped_thread_generation_rejects_start() {
    let (mut actor, mut context, counts) = actor(Init::Ready);
    actor.initialize(&mut context).await.unwrap();
    actor
        .process_message(Box::new(ControlMessage::Start), &mut context)
        .await
        .unwrap();
    actor.shutdown(&mut context).await.unwrap();
    assert!(
        actor
            .process_message(Box::new(ControlMessage::Start), &mut context)
            .await
            .is_err()
    );
    assert_eq!(actor.state(), ActorState::Stopped);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stopped_processor_generation_is_not_resurrected_by_start() {
    let (processor, counts) = processor(Init::Ready);
    processor.initialize_and_start().await.unwrap();
    processor.stop_actor().await.unwrap();
    assert!(processor.start_actor().await.is_err());
    assert_eq!(processor.get_status(), ProcessorStatus::Stopped);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn repeated_start_on_running_generation_preserves_identity_and_dispatch() {
    let (processor, counts) = processor(Init::Ready);
    processor.initialize_and_start().await.unwrap();
    processor.start_actor().await.unwrap();
    processor.start_actor().await.unwrap();
    assert_eq!(counts.entered.load(Ordering::SeqCst), 1);
    assert!(!processor.has_failed());
    assert_dispatches(&processor, &counts).await;
}

#[tokio::test]
async fn duplicate_initialization_does_not_poison_running_generation() {
    let (processor, counts) = processor(Init::Ready);
    processor.initialize_and_start().await.unwrap();
    assert!(processor.initialize_actor().await.is_err());
    assert_eq!(processor.get_status(), ProcessorStatus::Running);
    assert!(!processor.has_failed());
    assert_dispatches(&processor, &counts).await;
    assert_eq!(counts.entered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn duplicate_initialization_before_start_preserves_successful_admission() {
    let (processor, counts) = processor(Init::Ready);
    processor.initialize_actor().await.unwrap();
    assert!(processor.initialize_actor().await.is_err());
    assert!(!processor.has_failed());
    processor.start_actor().await.unwrap();
    assert_dispatches(&processor, &counts).await;
    assert_eq!(counts.completed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_lock_waiter_does_not_cancel_active_initialization() {
    let (processor, counts) = processor(Init::Pending);
    let mut active = Box::pin(processor.initialize_actor());
    assert!(poll_once(active.as_mut()).is_pending());
    let mut waiter = Box::pin(processor.initialize_actor());
    assert!(poll_once(waiter.as_mut()).is_pending());
    drop(waiter);
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 1);
    assert_eq!(processor.get_status(), ProcessorStatus::Initializing);
    counts.release.store(true, Ordering::SeqCst);
    active.await.unwrap();
    processor.start_actor().await.unwrap();
    assert!(!processor.has_failed());
    assert_dispatches(&processor, &counts).await;
}

#[tokio::test]
async fn cancelling_unpolled_initialization_keeps_new_generation_usable() {
    let (processor, counts) = processor(Init::Ready);
    drop(processor.initialize_actor());
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
    assert_eq!(processor.get_status(), ProcessorStatus::Initializing);
    processor.initialize_and_start().await.unwrap();
    assert_dispatches(&processor, &counts).await;
}

#[tokio::test]
async fn stopping_before_initialization_never_runs_normal_stop_hook() {
    let (processor, counts) = processor(Init::Ready);
    processor.stop_actor().await.unwrap();
    assert_eq!(processor.get_status(), ProcessorStatus::Stopped);
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);
    assert!(processor.initialize_actor().await.is_err());
    assert_eq!(processor.get_status(), ProcessorStatus::Stopped);
}

#[tokio::test]
async fn start_cannot_bypass_processor_pause() {
    let (processor, counts) = processor(Init::Ready);
    processor.initialize_and_start().await.unwrap();
    processor.pause();
    assert!(processor.start_actor().await.is_err());
    assert_eq!(processor.get_status(), ProcessorStatus::Paused);
    processor.resume();
    assert_dispatches(&processor, &counts).await;
}

#[tokio::test]
async fn completed_initialization_cannot_activate_after_processing_cancellation() {
    let (processor, counts) = processor(Init::Ready);
    processor.initialize_actor().await.unwrap();
    processor.cancel_processing();
    assert!(processor.start_actor().await.is_err());
    assert_ne!(processor.get_status(), ProcessorStatus::Running);
    assert_eq!(counts.messages.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn queued_start_cannot_overwrite_an_admitted_stop() {
    let (processor, counts) = processor(Init::Pending);
    let mut active = Box::pin(processor.initialize_actor());
    assert!(poll_once(active.as_mut()).is_pending());
    let mut start = Box::pin(processor.start_actor());
    assert!(poll_once(start.as_mut()).is_pending());
    let mut stop = Box::pin(processor.stop_actor());
    assert!(poll_once(stop.as_mut()).is_pending());
    assert_eq!(processor.get_status(), ProcessorStatus::Stopping);
    counts.release.store(true, Ordering::SeqCst);
    active.await.unwrap();
    assert!(start.await.is_err());
    assert_eq!(processor.get_status(), ProcessorStatus::Stopping);
    stop.await.unwrap();
    assert_eq!(processor.get_status(), ProcessorStatus::Stopped);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn overlapping_initialization_waiter_cannot_fail_a_successful_generation() {
    let (processor, counts) = processor(Init::Pending);
    let mut active = Box::pin(processor.initialize_and_start());
    assert!(poll_once(active.as_mut()).is_pending());
    let mut waiter = Box::pin(processor.initialize_actor());
    assert!(poll_once(waiter.as_mut()).is_pending());
    counts.release.store(true, Ordering::SeqCst);
    let (active, waiter) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(active, waiter)
    })
    .await
    .expect("overlapping initialization did not finish");
    active.unwrap();
    assert!(waiter.is_err());
    assert_eq!(processor.get_status(), ProcessorStatus::Running);
    assert!(!processor.has_failed());
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 1);
    assert_dispatches(&processor, &counts).await;
}
