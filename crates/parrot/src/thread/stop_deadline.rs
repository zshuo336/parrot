//! Separate the public stop deadline from the owned work's actual retirement.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use tokio::time::Instant;

#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum Phase {
    Admitted,
    MailboxDrain,
    StopHook,
    SchedulerRetirement,
    ResourcesFinished,
    PolicyDisposal,
    FactoryDisposal,
    TerminationNotification,
    RegistryRemoval,
    Finished,
}

#[derive(Default)]
pub(super) struct Progress {
    phase: AtomicU8,
    children_finished: AtomicBool,
}
impl Progress {
    pub(super) fn enter(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Release);
    }

    pub(super) fn children_finished(&self) {
        self.children_finished.store(true, Ordering::Release);
    }

    // A timeout must not take user/registry locks just to describe stalled work.
    pub(super) fn timeout_detail(&self, path: &str) -> String {
        let phase = match self.phase.load(Ordering::Acquire) {
            value if value == Phase::Admitted as u8 => "admitted",
            value if value == Phase::MailboxDrain as u8 => "mailbox-drain",
            value if value == Phase::StopHook as u8 => "stop-hook",
            value if value == Phase::SchedulerRetirement as u8 => "scheduler-retirement",
            value if value == Phase::ResourcesFinished as u8 => "resources-finished",
            value if value == Phase::PolicyDisposal as u8 => "policy-disposal",
            value if value == Phase::FactoryDisposal as u8 => "factory-disposal",
            value if value == Phase::TerminationNotification as u8 => "termination-notification",
            value if value == Phase::RegistryRemoval as u8 => "registry-removal",
            _ => "finished",
        };
        let children = if self.children_finished.load(Ordering::Acquire) {
            "finished"
        } else {
            "pending"
        };
        format!(
            "actor {path} stop exceeded configured deadline (observed phase={phase}, children={children})"
        )
    }
}

pub(super) async fn join_until<F: Future>(
    work: F,
    deadline: Instant,
    publish_timeout: impl FnOnce(),
) -> (F::Output, bool) {
    tokio::pin!(work);
    tokio::select! {
        biased;
        result = &mut work => {
            // The inner stop hook and this owner share a deadline. Forced
            // cleanup can become ready in the same poll as the timer; the
            // biased result branch must not turn that into an on-time result.
            let timed_out = Instant::now() >= deadline;
            if timed_out {
                publish_timeout();
            }
            (result, timed_out)
        },
        _ = tokio::time::sleep_until(deadline) => {
            publish_timeout();
            (work.await, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test(start_paused = true)]
    async fn ready_work_at_or_after_deadline_preserves_timeout_and_retirement_result() {
        for overdue in [Duration::ZERO, Duration::from_millis(1)] {
            for expected in [Ok(()), Err("late destructor panic")] {
                let deadline = Instant::now() + Duration::from_millis(10);
                tokio::time::advance(Duration::from_millis(10) + overdue).await;
                let published = AtomicUsize::new(0);
                let (retirement, timed_out) =
                    join_until(std::future::ready(expected), deadline, || {
                        published.fetch_add(1, Ordering::SeqCst);
                    })
                    .await;
                assert!(
                    timed_out,
                    "ready retirement must not hide an expired stop deadline"
                );
                assert_eq!(published.load(Ordering::SeqCst), 1);
                assert_eq!(
                    retirement, expected,
                    "actual cleanup result remains independent"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn work_becoming_ready_after_deadline_cannot_win_the_repoll_race() {
        let deadline = Instant::now() + Duration::from_millis(10);
        let (complete, completed) = oneshot::channel();
        let published = AtomicUsize::new(0);
        let owner = join_until(completed, deadline, || {
            published.fetch_add(1, Ordering::SeqCst);
        });
        tokio::pin!(owner);
        assert!(futures::poll!(owner.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(11)).await;
        complete
            .send(Err::<(), _>("late destructor panic"))
            .unwrap();
        let (retirement, timed_out) = owner.await;
        assert!(
            timed_out,
            "both-ready repoll must retain the expired deadline"
        );
        assert_eq!(published.load(Ordering::SeqCst), 1);
        assert_eq!(retirement.unwrap(), Err("late destructor panic"));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_is_published_before_pending_cleanup_finishes() {
        let deadline = Instant::now() + Duration::from_millis(10);
        let (release, released) = oneshot::channel();
        let (public, observed) = oneshot::channel();
        let drops = Arc::new(AtomicUsize::new(0));
        struct Cleanup(Arc<AtomicUsize>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let cleanup = Cleanup(drops.clone());
        let owner = tokio::spawn(join_until(
            async move {
                let _cleanup = cleanup;
                released.await.unwrap();
                Err::<(), _>("actual cleanup error")
            },
            deadline,
            move || {
                public.send(()).unwrap();
            },
        ));
        observed.await.expect("public timeout receipt");
        assert!(
            !owner.is_finished(),
            "timeout must not abandon owned cleanup"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        let (retirement, timed_out) = owner.await.unwrap();
        assert!(timed_out);
        assert_eq!(retirement, Err("actual cleanup error"));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn early_success_and_failure_keep_their_original_outcome() {
        for expected in [Ok(()), Err("early destructor panic")] {
            let (retirement, timed_out) = join_until(
                std::future::ready(expected),
                Instant::now() + Duration::from_secs(1),
                || panic!("early work must not time out"),
            )
            .await;
            assert!(!timed_out);
            assert_eq!(retirement, expected);
        }
    }
}
