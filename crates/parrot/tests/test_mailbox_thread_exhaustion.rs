//! OS thread exhaustion must not abandon mailbox close ownership.
#![cfg(target_os = "linux")]

use futures::FutureExt;
use parrot::thread::config::BackpressureStrategy;
use parrot::thread::error::MailboxError;
use parrot::thread::mailbox::{
    Mailbox, mpsc::MpscMailbox, spsc::SpscMailbox, spsc_ringbuf::SpscRingbufMailbox,
};
use parrot_api::address::ActorPath;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[path = "support/thread_budget.rs"]
mod thread_budget;
use thread_budget::ThreadBudget;

struct Value {
    drops: Arc<AtomicUsize>,
    failure: Option<&'static str>,
}

impl Drop for Value {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(failure) = self.failure {
            panic!("{failure}");
        }
    }
}

async fn check_close(mailbox: Arc<dyn Mailbox>, hostile: bool) {
    let counts: Vec<_> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    for (index, count) in counts.iter().enumerate() {
        mailbox
            .push(
                Box::new(Value {
                    drops: count.clone(),
                    failure: if hostile {
                        [Some("first drain value"), Some("second drain value"), None][index]
                    } else {
                        None
                    },
                }),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
    }
    let mut previous = None;
    for _ in 0..3 {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            AssertUnwindSafe(mailbox.close()).catch_unwind(),
        )
        .await
        .expect("repeated close must publish completion after OS thread creation fails");
        if hostile {
            let failure = result.expect_err("disposal failures must reach every close");
            let failure = failure.downcast_ref::<String>().unwrap().clone();
            assert!(failure.contains("first drain value"), "{failure}");
            assert!(failure.contains("second drain value"), "{failure}");
            if let Some(previous) = previous.replace(failure.clone()) {
                assert_eq!(failure, previous, "close callers must share one receipt");
            }
        } else {
            assert!(result.is_ok(), "successful fallback must complete close");
        }
        assert!(mailbox.is_closed().await);
        assert_eq!(mailbox.len().await, 0);
        assert!(mailbox.pop().await.is_none());
        for count in &counts {
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }
    // The mailbox remains alive throughout the assertions above.
}

fn make_mailbox(kind: &str) -> Arc<dyn Mailbox> {
    match kind {
        "mpsc" => Arc::new(MpscMailbox::new(3, ActorPath::for_test("/exhaust-mpsc"))),
        "spsc" => Arc::new(SpscMailbox::new(3, ActorPath::for_test("/exhaust-spsc"))),
        "ring" => Arc::new(SpscRingbufMailbox::new(
            3,
            ActorPath::for_test("/exhaust-ring"),
        )),
        _ => unreachable!(),
    }
}

fn run(kind: &str, hostile: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _budget = ThreadBudget::exhaust();
    runtime.block_on(check_close(make_mailbox(kind), hostile));
}

macro_rules! exhaustion_case {
    ($name:ident, $kind:literal, $hostile:literal) => {
        #[test]
        #[ignore = "requires disposable Linux cgroup v2 container with --pids-limit=64"]
        fn $name() {
            run($kind, $hostile);
        }
    };
}

exhaustion_case!(thread_exhaustion_closes_mpsc, "mpsc", false);
exhaustion_case!(thread_exhaustion_closes_spsc, "spsc", false);
exhaustion_case!(thread_exhaustion_closes_ring, "ring", false);
exhaustion_case!(thread_exhaustion_preserves_mpsc_failures, "mpsc", true);
exhaustion_case!(thread_exhaustion_preserves_spsc_failures, "spsc", true);
exhaustion_case!(thread_exhaustion_preserves_ring_failures, "ring", true);

async fn admission_under_exhaustion(kind: &str) -> Vec<Arc<AtomicUsize>> {
    let mailbox = make_mailbox(kind);
    let mut counts = Vec::new();
    for value in 17_u64..20 {
        mailbox
            .push(Box::new(value), BackpressureStrategy::Error)
            .await
            .unwrap();
    }
    for strategy in [
        BackpressureStrategy::Error,
        BackpressureStrategy::DropNewest,
    ] {
        for hostile in [false, true] {
            let drops = Arc::new(AtomicUsize::new(0));
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                mailbox.push(
                    Box::new(Value {
                        drops: drops.clone(),
                        failure: hostile.then_some("rejected input failure"),
                    }),
                    strategy.clone(),
                ),
            )
            .await
            .expect("rejection must complete actual cleanup despite worker startup failure");
            assert!(
                match strategy {
                    BackpressureStrategy::Error =>
                        matches!(result, Err(MailboxError::Full { capacity: 3 })),
                    BackpressureStrategy::DropNewest =>
                        matches!(result, Err(MailboxError::Dropped { capacity: 3 })),
                    _ => unreachable!(),
                },
                "rejection category changed: {result:?}"
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            counts.push(drops);
        }
    }
    for hostile in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut pending = Box::pin(mailbox.push(
            Box::new(Value {
                drops: drops.clone(),
                failure: hostile.then_some("cancelled input failure"),
            }),
            BackpressureStrategy::Block,
        ));
        assert!(futures::poll!(&mut pending).is_pending());
        drop(pending);
        tokio::time::timeout(Duration::from_secs(2), async {
            while drops.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled admission must actually retire its input");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        counts.push(drops);
    }
    for value in 17_u64..20 {
        assert_eq!(
            *mailbox.pop().await.unwrap().downcast::<u64>().unwrap(),
            value
        );
    }
    for hostile in [false, true] {
        let eviction = make_mailbox(kind);
        let drops = Arc::new(AtomicUsize::new(0));
        eviction
            .push(
                Box::new(Value {
                    drops: drops.clone(),
                    failure: hostile.then_some("evicted input failure"),
                }),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
        for value in 17_u64..19 {
            eviction
                .push(Box::new(value), BackpressureStrategy::Error)
                .await
                .unwrap();
        }
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            eviction.push(Box::new(91_u64), BackpressureStrategy::DropOldest),
        )
        .await
        .expect("replacement must complete actual evicted input cleanup");
        if hostile {
            assert!(
                matches!(result, Err(MailboxError::PushError(ref error))
                if error.contains("replacement admitted") && error.contains("evicted input failure")),
                "replacement must report its own cleanup failure: {result:?}"
            );
        } else {
            result.unwrap();
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        for value in [17_u64, 18, 91] {
            assert_eq!(
                *eviction.pop().await.unwrap().downcast::<u64>().unwrap(),
                value
            );
        }
        eviction.close().await;
        counts.push(drops);
    }
    mailbox.seal();
    for hostile in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            mailbox.push(
                Box::new(Value {
                    drops: drops.clone(),
                    failure: hostile.then_some("closed input failure"),
                }),
                BackpressureStrategy::Error,
            ),
        )
        .await
        .expect("closed rejection must complete actual cleanup");
        assert!(matches!(result, Err(MailboxError::Closed)));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        counts.push(drops);
    }
    mailbox.close().await;
    assert!(mailbox.pop().await.is_none());
    counts
}

fn run_admission(kind: &str) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let budget = ThreadBudget::exhaust();
    let counts = runtime.block_on(admission_under_exhaustion(kind));
    drop(budget);
    assert_eq!(runtime.block_on(runtime.spawn_blocking(|| 91)).unwrap(), 91);
    for count in counts {
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "recovery must not repeat disposal"
        );
    }
}

#[test]
#[ignore = "requires disposable Linux cgroup v2 container with --pids-limit=64"]
fn thread_exhaustion_rejects_mpsc() {
    run_admission("mpsc");
}

#[test]
#[ignore = "requires disposable Linux cgroup v2 container with --pids-limit=64"]
fn thread_exhaustion_rejects_spsc() {
    run_admission("spsc");
}

#[test]
#[ignore = "requires disposable Linux cgroup v2 container with --pids-limit=64"]
fn thread_exhaustion_rejects_ring() {
    run_admission("ring");
}

#[test]
#[ignore = "requires disposable Linux cgroup v2 container with --pids-limit=64"]
fn thread_exhaustion_cancels_failed_tokio_job() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let budget = ThreadBudget::exhaust();
    let drops = Arc::new(AtomicUsize::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let value = Value {
        drops: drops.clone(),
        failure: None,
    };
    let ran = runs.clone();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        runtime.spawn_blocking(move || {
            ran.fetch_add(1, Ordering::SeqCst);
            drop(value);
        })
    }));
    assert!(
        result.is_err(),
        "Tokio must retain its public startup panic contract"
    );
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "failed spawn must immediately dispose its capture"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    drop(budget);
    assert_eq!(runtime.block_on(runtime.spawn_blocking(|| 91)).unwrap(), 91);
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "failed dispatch must not run after recovery"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
