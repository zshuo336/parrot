//! Disconnect precedes drain, including raw sends already waiting for capacity.
use futures::{FutureExt, task::noop_waker};
use parrot::thread::{
    BackpressureStrategy,
    mailbox::{Mailbox, MailboxItem, mpsc::MpscMailbox},
};
use parrot_api::address::ActorPath;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct ReentrantWake {
    sender: flume::Sender<MailboxItem>,
    wakes: Arc<AtomicUsize>,
    panic: bool,
}
impl Wake for ReentrantWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        assert!(self.sender.is_disconnected());
        assert!(
            self.sender
                .try_send(MailboxItem::Plain(Box::new(99u64)))
                .is_err()
        );
        if self.panic {
            panic!("raw sender disconnect callback");
        }
    }
}

fn exercise(explicit_close: bool, hostile_waker: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mailbox = MpscMailbox::new(2, ActorPath::for_test("/raw-disconnect"));
    let sender = mailbox.sender();
    let drops = Arc::new(AtomicUsize::new(0));
    runtime.block_on(async {
        for _ in 0..2 {
            mailbox
                .push(
                    Box::new(Counted(drops.clone())),
                    BackpressureStrategy::Error,
                )
                .await
                .unwrap();
        }
    });
    let mut pending = sender
        .send_async(MailboxItem::Plain(Box::new(Counted(drops.clone()))))
        .boxed();
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = if hostile_waker {
        Waker::from(Arc::new(ReentrantWake {
            sender: sender.clone(),
            wakes: wakes.clone(),
            panic: true,
        }))
    } else {
        noop_waker()
    };
    assert!(matches!(
        pending.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let result = catch_unwind(AssertUnwindSafe(|| {
        if explicit_close {
            runtime.block_on(mailbox.close());
        }
        drop(mailbox);
    }));
    assert!(
        result.is_ok(),
        "disconnect callback must not escape mailbox cleanup"
    );
    assert!(sender.is_disconnected());
    assert_eq!(
        sender.len(),
        0,
        "disconnect must not refill an already drained queue"
    );
    assert_eq!(
        drops.load(Ordering::SeqCst),
        2,
        "only queued messages belonged to the mailbox"
    );
    let error = runtime
        .block_on(pending)
        .expect_err("unadmitted raw send must return its input");
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    drop(error);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    if hostile_waker {
        assert!(
            wakes.load(Ordering::SeqCst) > 0,
            "registered callback must actually execute"
        );
    }
    drop(sender);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}

#[test]
fn close_rejects_pending_raw_send_and_retires_queued_values() {
    exercise(true, false);
}

#[test]
fn drop_rejects_pending_raw_send_and_retires_queued_values() {
    exercise(false, false);
}

#[test]
fn close_finishes_cleanup_after_raw_sender_waker_panics() {
    exercise(true, true);
}

#[test]
fn drop_finishes_cleanup_after_raw_sender_waker_panics() {
    exercise(false, true);
}
