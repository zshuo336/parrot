//! A blocked service cleanup must leave the shared lifecycle driver runnable.
use super::*;
use futures::FutureExt;
use parrot_api::receptionist::ReceptionistKey;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

struct Capture(Arc<AtomicUsize>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn contract(outcome: u8) {
    actix::System::new().block_on(async move {
        let lifecycle = crate::root_cohort::RootCohorts::new().unwrap();
        let runtime = lifecycle.retirement_runtime();
        let peer = crate::actix::ActixActorSystem::with_arbiter_count(1)
            .await
            .unwrap();
        let owner = ContextTasks::new(0);
        let scope = owner.registration_scope(runtime);
        let calls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let capture = Capture(drops.clone());
        let (entered, arrived) = tokio::sync::oneshot::channel();
        let mut entered = Some(entered);
        let (release, wait) = std::sync::mpsc::channel();
        scope
            .publish(
                (
                    1,
                    ReceptionistKey::new("isolation/service").unwrap(),
                    "/service".into(),
                ),
                move || {
                    let _capture = &capture;
                    called.fetch_add(1, Ordering::SeqCst);
                    entered.take().unwrap().send(()).unwrap();
                    wait.recv().unwrap();
                    match outcome {
                        0 => Ok(()),
                        1 => Err(ActorError::InternalError("isolated cleanup error".into())),
                        _ => panic!("isolated cleanup panic"),
                    }
                },
                || (),
            )
            .unwrap();
        owner.close();
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        let pending = owner.join().now_or_never().is_none();
        let timer = runtime.spawn(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        let progress = tokio::time::timeout(Duration::from_secs(2), async {
            timer.await.unwrap();
            peer.clone().shutdown().await.unwrap();
        })
        .await;
        // Unblock before any behavioral assertion, including on old production.
        release.send(()).unwrap();
        for _ in 0..2 {
            let receipt = tokio::time::timeout(Duration::from_secs(5), owner.join())
                .await
                .unwrap();
            match outcome {
                0 => receipt.unwrap(),
                1 => assert!(matches!(receipt, Err(ActorError::InternalError(message))
                    if message == "Internal error: isolated cleanup error")),
                _ => assert!(matches!(receipt, Err(ActorError::Panic(message))
                    if message == "isolated cleanup panic")),
            }
        }
        peer.shutdown().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(pending, "generation receipt must wait for actual cleanup");
        assert!(
            progress.is_ok(),
            "blocked registration cleanup stalled another system and lifecycle clock"
        );
    });
}

#[test]
fn blocked_success_preserves_lifecycle_progress() {
    contract(0);
}

#[test]
fn blocked_error_preserves_lifecycle_progress() {
    contract(1);
}

#[test]
fn blocked_panic_preserves_lifecycle_progress() {
    contract(2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_cleanup_uses_bounded_blocking_capacity() {
    let slots = Arc::new(Semaphore::new(1));
    let first = ContextTasks::with_cleanup_slots(0, slots.clone());
    let second = ContextTasks::with_cleanup_slots(0, slots);
    let first_scope = first.registration_scope(&tokio::runtime::Handle::current());
    let second_scope = second.registration_scope(&tokio::runtime::Handle::current());
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_first, wait_first) = std::sync::mpsc::channel();
    let (release_second, wait_second) = std::sync::mpsc::channel();
    let first_entered = entered.clone();
    first_scope
        .publish(
            (
                1,
                ReceptionistKey::new("edge/first").unwrap(),
                "/first".into(),
            ),
            move || {
                first_entered.send("first").unwrap();
                wait_first.recv().unwrap();
                Ok(())
            },
            || (),
        )
        .unwrap();
    let second_entered = entered.clone();
    second_scope
        .publish(
            (
                1,
                ReceptionistKey::new("edge/second").unwrap(),
                "/second".into(),
            ),
            move || {
                second_entered.send("second").unwrap();
                wait_second.recv().unwrap();
                Ok(())
            },
            || (),
        )
        .unwrap();
    drop(entered);
    first.close();
    second.close();

    let first_name = tokio::time::timeout(Duration::from_secs(2), entered_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let second_before_release = tokio::time::timeout(Duration::from_millis(100), entered_rx.recv())
        .await
        .is_ok();
    // Release the active callback before asserting, so a failed assertion
    // cannot leave a blocking cleanup worker behind.
    release_first.send(()).unwrap();
    release_second.send(()).unwrap();
    first.join().await.unwrap();
    second.join().await.unwrap();

    assert!(
        !second_before_release,
        "two registration callbacks ran concurrently"
    );
    assert!(first_name == "first" || first_name == "second");
}
