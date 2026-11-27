// SPDX-License-Identifier: AGPL-3.0-only
//! Synchronous scheduler control must join the native thread before returning.
use parrot::thread::{
    ActorProcessor, BackpressureStrategy, ThreadActor, ThreadActorConfig, ThreadContext,
    error::SystemError,
    mailbox::{Mailbox, mpsc::MpscMailbox},
    scheduler::{ThreadScheduler, dedicated_thread::DedicatedThreadScheduler},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    cell::RefCell,
    sync::{Arc, mpsc},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(4);
const PATH: &str = "/sync-retirement";

struct ThreadExit {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
impl Drop for ThreadExit {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        let _ = self.release.recv_timeout(3 * WAIT);
    }
}
thread_local! {
    static EXIT: RefCell<Option<ThreadExit>> = const { RefCell::new(None) };
}
struct Probe;
impl Actor for Probe {
    type Context = ThreadContext<Self>;
    type Config = EmptyConfig;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let (guard, reply) = *msg
                .downcast::<(ThreadExit, mpsc::Sender<std::thread::ThreadId>)>()
                .unwrap();
            EXIT.with(|slot| *slot.borrow_mut() = Some(guard));
            reply.send(std::thread::current().id()).unwrap();
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}

#[derive(Clone, Copy)]
enum Host {
    Plain,
    Current,
    Multi,
}

fn arm_worker(
    runtime: &tokio::runtime::Runtime,
    scheduler: &DedicatedThreadScheduler,
) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, disposing) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let (reply, owner) = mpsc::channel::<std::thread::ThreadId>();
    runtime.block_on(async {
        let mailbox = Arc::new(MpscMailbox::new(2, ActorPath::placeholder(PATH)));
        let processor = Arc::new(ActorProcessor::new(
            ThreadActor::new_for_test(Probe),
            ThreadContext::new_for_test(PATH),
            PATH.into(),
            ThreadActorConfig::default(),
        ));
        mailbox.set_processor(processor);
        mailbox
            .push(
                Box::new((
                    ThreadExit {
                        entered,
                        release: wait,
                    },
                    reply,
                )),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
        ThreadScheduler::schedule(scheduler, PATH, mailbox, None).unwrap();
    });
    assert_ne!(
        owner.recv_timeout(WAIT).unwrap(),
        std::thread::current().id()
    );
    (disposing, release)
}

fn dedicated(host: Host, remove: bool) {
    let runtime = match host {
        Host::Plain | Host::Current => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
        Host::Multi => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap(),
    };
    let mut scheduler = DedicatedThreadScheduler::new(None);
    if !matches!(host, Host::Plain) {
        scheduler = scheduler.with_runtime_handle(runtime.handle().clone());
    }
    let scheduler = Arc::new(scheduler);
    let (disposing, release) = arm_worker(&runtime, &scheduler);
    let (returned, result) = mpsc::channel();
    std::thread::scope(|scope| {
        let witness = scope.spawn(move || {
            disposing
                .recv_timeout(WAIT)
                .expect("dedicated TLS destructor did not start");
            assert!(
                matches!(
                    result.recv_timeout(Duration::from_millis(100)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ),
                "scheduler returned before the worker's TLS destructor completed"
            );
            release.send(()).unwrap();
        });
        let handle = scheduler.clone();
        let call = move || {
            let scheduler: &dyn ThreadScheduler = handle.as_ref();
            if remove {
                scheduler.deschedule(PATH)
            } else {
                scheduler.shutdown()
            }
            .unwrap();
            let _ = returned.send(());
        };
        match host {
            Host::Plain => call(),
            Host::Current | Host::Multi => runtime
                .block_on(runtime.spawn(async move { call() }))
                .unwrap(),
        }
        witness.join().unwrap();
    });
    assert_eq!(scheduler.worker_count(), 0);
    ThreadScheduler::deschedule(scheduler.as_ref(), PATH).unwrap();
    let missing = ThreadScheduler::deschedule(scheduler.as_ref(), "/missing").unwrap_err();
    assert!(
        matches!(missing.downcast_ref::<SystemError>(), Some(SystemError::ActorNotFound(path)) if path == "/missing")
    );
    ThreadScheduler::shutdown(scheduler.as_ref()).unwrap();
    assert_eq!(scheduler.status_code(), 3);
}

#[test]
fn timeout_preserves_the_dedicated_worker_until_sync_retry_joins_it() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let scheduler = DedicatedThreadScheduler::new(None);
    let (disposing, release) = arm_worker(&runtime, &scheduler);
    let error = ThreadScheduler::shutdown(&scheduler).unwrap_err();
    assert!(
        matches!(error.downcast_ref::<SystemError>(), Some(SystemError::Timeout(detail)) if detail.contains(PATH))
    );
    disposing
        .try_recv()
        .expect("worker never reached TLS retirement");
    assert_eq!(scheduler.worker_count(), 1);
    assert_eq!(scheduler.status_code(), 2);
    release.send(()).unwrap();
    ThreadScheduler::shutdown(&scheduler).unwrap();
    assert_eq!(scheduler.worker_count(), 0);
    assert_eq!(scheduler.status_code(), 3);
}

#[test]
fn plain_thread_can_synchronously_remove_a_dedicated_worker() {
    dedicated(Host::Plain, true);
}
#[test]
fn plain_thread_can_synchronously_stop_a_dedicated_scheduler() {
    dedicated(Host::Plain, false);
}
#[test]
fn current_thread_callback_can_synchronously_remove_a_dedicated_worker() {
    dedicated(Host::Current, true);
}
#[test]
fn current_thread_callback_can_synchronously_stop_a_dedicated_scheduler() {
    dedicated(Host::Current, false);
}
#[test]
fn multi_thread_callback_can_synchronously_remove_a_dedicated_worker() {
    dedicated(Host::Multi, true);
}
#[test]
fn multi_thread_callback_can_synchronously_stop_a_dedicated_scheduler() {
    dedicated(Host::Multi, false);
}
