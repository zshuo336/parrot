use super::{Mailbox, mpsc::MpscMailbox, spsc::SpscMailbox, spsc_ringbuf::SpscRingbufMailbox};
use crate::{test_support::isolated, thread::config::BackpressureStrategy};
use futures::FutureExt;
use parrot_api::{address::ActorPath, types::BoxedMessage};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(10);
// Contract limit, deliberately independent of the executor implementation.
const DRAIN_THREADS: usize = 32;

fn mailbox(kind: usize) -> Arc<dyn Mailbox> {
    let path = ActorPath::for_test("/drain-budget");
    match kind {
        0 => Arc::new(MpscMailbox::new(3, path)),
        1 => Arc::new(SpscMailbox::new(3, path)),
        _ => Arc::new(SpscRingbufMailbox::new(3, path)),
    }
}

#[derive(Default)]
struct Gate {
    released: Mutex<bool>,
    wake: Condvar,
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}
struct Blocked {
    gate: Arc<Gate>,
    entered: mpsc::Sender<std::thread::ThreadId>,
    nested: Option<Arc<dyn Mailbox>>,
    drops: Arc<AtomicUsize>,
    tls: Option<Box<Blocked>>,
}
std::thread_local! {
    static THREAD_EXIT: std::cell::RefCell<Option<Blocked>> = const { std::cell::RefCell::new(None) };
}
impl Drop for Blocked {
    fn drop(&mut self) {
        self.entered.send(std::thread::current().id()).unwrap();
        let (released, timeout) = self
            .gate
            .wake
            .wait_timeout_while(self.gate.released.lock().unwrap(), LIMIT, |released| {
                !*released
            })
            .unwrap();
        assert!(
            *released && !timeout.timed_out(),
            "test must release the destructor"
        );
        drop(released);
        if let Some(nested) = self.nested.take() {
            futures::executor::block_on(nested.close());
        }
        if let Some(tls) = self.tls.take() {
            THREAD_EXIT.with(|exit| *exit.borrow_mut() = Some(*tls));
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Counted(Arc<AtomicUsize>, Option<&'static str>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.1 {
            panic!("{error}");
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn cumulative_close_budget(kind: usize) {
    let host = runtime();
    let nested = mailbox(kind);
    let nested_drops = Arc::new(AtomicUsize::new(0));
    host.block_on(nested.push(
        Box::new(Counted(nested_drops.clone(), None)),
        BackpressureStrategy::Error,
    ))
    .unwrap();
    let exit_nested = mailbox(kind);
    let exit_nested_drops = Arc::new(AtomicUsize::new(0));
    host.block_on(exit_nested.push(
        Box::new(Counted(exit_nested_drops.clone(), None)),
        BackpressureStrategy::Error,
    ))
    .unwrap();
    let active_drops = Arc::new(AtomicUsize::new(0));
    let tls_drops = Arc::new(AtomicUsize::new(0));
    let tls_gate = Arc::new(Gate::default());
    let tls_release = Release(tls_gate.clone());
    let (entered, receive) = mpsc::channel();
    let mut owners = Vec::new();
    let mut releases = Vec::new();
    let mut threads = std::collections::HashSet::new();
    for index in 0..DRAIN_THREADS {
        let owner = mailbox(index % 3);
        let gate = Arc::new(Gate::default());
        releases.push(Release(gate.clone()));
        host.block_on(owner.push(
            Box::new(Blocked {
                gate,
                entered: entered.clone(),
                nested: (index == DRAIN_THREADS - 1).then(|| nested.clone()),
                drops: active_drops.clone(),
                tls: (index == DRAIN_THREADS - 1).then(|| {
                    Box::new(Blocked {
                        gate: tls_gate.clone(),
                        entered: entered.clone(),
                        nested: Some(exit_nested.clone()),
                        drops: tls_drops.clone(),
                        tls: None,
                    })
                }),
            }),
            BackpressureStrategy::Error,
        ))
        .unwrap();
        host.block_on(async {
            let mut close = Box::pin(owner.close());
            assert!(futures::poll!(&mut close).is_pending());
            threads.insert(receive.recv_timeout(LIMIT).unwrap());
            drop(close);
        });
        owners.push(owner);
    }
    assert_eq!(threads.len(), DRAIN_THREADS);
    let extra = mailbox(kind);
    let extra_drops = Arc::new(AtomicUsize::new(0));
    host.block_on(async {
        for error in [
            Some("first budgeted failure"),
            Some("second budgeted failure"),
            None,
        ] {
            extra
                .push(
                    Box::new(Counted(extra_drops.clone(), error)) as BoxedMessage,
                    BackpressureStrategy::Error,
                )
                .await
                .unwrap();
        }
        let mut close = Box::pin(AssertUnwindSafe(extra.close()).catch_unwind());
        assert!(futures::poll!(&mut close).is_pending());
        assert_eq!(
            extra.len().await,
            3,
            "saturation must retain the sealed queue instead of starting a 33rd drain"
        );
        assert!(extra.is_closed().await);
        assert_eq!(extra_drops.load(Ordering::SeqCst), 0);
        drop(close);
        let abandoned = mailbox(kind);
        let abandoned_drops = Arc::new(AtomicUsize::new(0));
        abandoned
            .push(
                Box::new(Counted(abandoned_drops.clone(), None)),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
        let mut waiting = Box::pin(abandoned.close());
        assert!(futures::poll!(&mut waiting).is_pending());
        assert_eq!(abandoned.len().await, 1);
        drop(waiting);
        drop(abandoned);
        assert_eq!(
            abandoned_drops.load(Ordering::SeqCst),
            1,
            "final owner must dispose a cancelled, unadmitted queue"
        );
        // Saturation of nonempty drains must not prevent empty mailboxes closing.
        tokio::time::timeout(LIMIT, mailbox(kind).close())
            .await
            .unwrap();
    });
    // The original executor can exit with both cancelled active and cancelled
    // admission waiters. No destructor or join depends on it remaining alive.
    drop(host);
    let host = runtime();
    drop(releases.pop());
    host.block_on(async {
        tokio::time::timeout(LIMIT, owners.last().unwrap().close())
            .await
            .unwrap();
        assert_eq!(
            nested_drops.load(Ordering::SeqCst),
            1,
            "nested cleanup must not wait for its own last slot"
        );
        assert_eq!(active_drops.load(Ordering::SeqCst), 1);
        receive
            .recv_timeout(LIMIT)
            .expect("last drain must enter its TLS destructor");
        let mut waiting = Box::pin(AssertUnwindSafe(extra.close()).catch_unwind());
        assert!(futures::poll!(&mut waiting).is_pending());
        assert_eq!(
            extra.len().await,
            3,
            "payload disposal is not native thread retirement"
        );
        assert_eq!(tls_drops.load(Ordering::SeqCst), 0);
        drop(waiting);
        drop(tls_release);
        let first = tokio::time::timeout(LIMIT, AssertUnwindSafe(extra.close()).catch_unwind())
            .await
            .unwrap()
            .unwrap_err();
        let first = crate::owned_payload::panic_message(first);
        assert!(
            first.contains("first budgeted failure") && first.contains("second budgeted failure"),
            "{first}"
        );
        let repeated = AssertUnwindSafe(extra.close())
            .catch_unwind()
            .await
            .unwrap_err();
        assert_eq!(crate::owned_payload::panic_message(repeated), first);
        assert_eq!(extra_drops.load(Ordering::SeqCst), 3);
        assert_eq!(tls_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            exit_nested_drops.load(Ordering::SeqCst),
            1,
            "TLS cleanup must not wait for its own native join lease"
        );
        assert_eq!(extra.len().await, 0);
    });
    drop(releases);
    host.block_on(async {
        for owner in owners {
            tokio::time::timeout(LIMIT, owner.close()).await.unwrap();
        }
    });
    assert_eq!(active_drops.load(Ordering::SeqCst), DRAIN_THREADS);
}

#[test]
fn mpsc_close_has_process_wide_drain_admission() {
    isolated(
        "thread::mailbox::drain_budget_contracts::mpsc_close_has_process_wide_drain_admission",
        || cumulative_close_budget(0),
    );
}
#[test]
fn spsc_close_has_process_wide_drain_admission() {
    isolated(
        "thread::mailbox::drain_budget_contracts::spsc_close_has_process_wide_drain_admission",
        || cumulative_close_budget(1),
    );
}
#[test]
fn ring_close_has_process_wide_drain_admission() {
    isolated(
        "thread::mailbox::drain_budget_contracts::ring_close_has_process_wide_drain_admission",
        || cumulative_close_budget(2),
    );
}
