use super::*;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::task::{Context, Waker};

type Work = BoxedFuture<'static, ActorResult<BoxedMessage>>;

struct Reply {
    panic: bool,
    drops: Arc<AtomicUsize>,
}
impl Drop for Reply {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.panic {
            panic!("cancelled I/O reply disposal");
        }
    }
}

async fn cancelled_output(actix: bool, generation: bool) {
    for panic in [false, true] {
        let facade = contexts::facade(actix).await;
        let drops = Arc::new(AtomicUsize::new(0));
        let retired = drops.clone();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let input: Work = Box::pin(async move {
            entered.send(()).unwrap();
            wait.recv_timeout(LIMIT).expect("release I/O poll");
            Ok(Box::new(Reply {
                panic,
                drops: retired,
            }) as BoxedMessage)
        });
        let (actor, mut work) = if generation {
            let (actor, work) = contexts::capture_work(&facade, actix, input).await;
            (Some(actor), work)
        } else {
            (None, facade.io_runtime().unwrap().run_io(input))
        };
        // Hold the user poll while the caller deterministically drops its receiver.
        assert!(
            work.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        bounded(started).await.unwrap();
        drop(work);
        release.send(()).unwrap();
        if let Some(actor) = actor {
            let result = bounded(actor.stop()).await;
            assert_eq!(result.is_err(), panic, "generation cleanup receipt");
            if panic {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("cancelled I/O reply disposal")
                );
            }
        }
        let result = bounded(facade.shutdown_runtime()).await;
        assert_eq!(result.is_err(), panic, "system cleanup receipt");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        if panic {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("cancelled I/O reply disposal")
            );
        }
    }
}

#[tokio::test]
async fn thread_cancelled_output_preserves_actual_system_cleanup_failure() {
    cancelled_output(false, false).await;
}
#[actix::test]
async fn actix_cancelled_output_preserves_actual_system_cleanup_failure() {
    cancelled_output(true, false).await;
}
#[tokio::test]
async fn thread_cancelled_output_preserves_actual_generation_cleanup_failure() {
    cancelled_output(false, true).await;
}
#[actix::test]
async fn actix_cancelled_output_preserves_actual_generation_cleanup_failure() {
    cancelled_output(true, true).await;
}
