//! Fairness counts synchronous message completions since the last real suspension.
use std::future::{Future, poll_fn};

#[derive(Default)]
pub(super) struct ReadyBudget {
    completed: u8,
}
impl ReadyBudget {
    /// Wrap the complete control stage, not a branch inside its select. A
    /// Pending here propagates through the execution loop to the executor.
    pub(super) async fn wait<F: Future>(&mut self, future: F) -> F::Output {
        tokio::pin!(future);
        poll_fn(|cx| {
            let result = future.as_mut().poll(cx);
            if result.is_pending() {
                self.completed = 0;
            }
            result
        })
        .await
    }

    pub(super) async fn completed(&mut self) {
        self.completed += 1;
        if self.completed >= 10 {
            self.completed = 0;
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll};

    fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
        std::pin::pin!(future).poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
    }

    #[tokio::test]
    async fn ready_work_keeps_the_ten_message_fairness_bound() {
        let mut budget = ReadyBudget::default();
        for _ in 0..3 {
            for _ in 0..9 {
                assert!(poll_once(budget.wait(std::future::ready(()))).is_ready());
                assert!(poll_once(budget.completed()).is_ready());
            }
            let completion = budget.completed();
            tokio::pin!(completion);
            assert!(poll_once(&mut completion).is_pending());
            assert!(poll_once(&mut completion).is_ready());
        }
    }

    #[tokio::test]
    async fn real_stage_suspension_starts_a_new_burst() {
        let mut budget = ReadyBudget::default();
        for _ in 0..9 {
            assert!(poll_once(budget.completed()).is_ready());
        }
        {
            let mut suspended = false;
            let stage = budget.wait(poll_fn(|cx| {
                if suspended {
                    Poll::Ready(37)
                } else {
                    suspended = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }));
            tokio::pin!(stage);
            assert!(poll_once(&mut stage).is_pending());
            assert_eq!(stage.await, 37);
        }
        for _ in 0..9 {
            assert!(poll_once(budget.completed()).is_ready());
        }
        let completion = budget.completed();
        tokio::pin!(completion);
        assert!(poll_once(&mut completion).is_pending());
        assert!(poll_once(&mut completion).is_ready());
    }

    #[tokio::test]
    async fn pending_branch_absorbed_by_select_does_not_reset_budget() {
        let mut budget = ReadyBudget::default();
        for _ in 0..9 {
            assert!(poll_once(budget.completed()).is_ready());
        }
        let mut branch_polled = false;
        let stage = async {
            tokio::select! {
                biased;
                _ = poll_fn(|_| { branch_polled = true; Poll::<()>::Pending }) => unreachable!(),
                result = std::future::ready(73) => result,
            }
        };
        assert_eq!(poll_once(budget.wait(stage)), Poll::Ready(73));
        assert!(branch_polled);
        let completion = budget.completed();
        tokio::pin!(completion);
        assert!(poll_once(&mut completion).is_pending());
        assert!(poll_once(&mut completion).is_ready());
    }
}
