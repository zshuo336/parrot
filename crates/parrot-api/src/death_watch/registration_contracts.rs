use super::*;
use crate::DeathReason;
use std::sync::mpsc;

struct Endpoint<F>(F);
impl<F: Fn(ActorTerminated) + Send + Sync> TerminationRecipient for Endpoint<F> {
    fn deliver(&self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        (self.0)(event);
        Box::pin(async { Ok(()) })
    }
}

fn event(target: &DeathWatch) -> ActorTerminated {
    ActorTerminated {
        generation: target.id(),
        path: "/target".into(),
        reason: DeathReason::Normal,
    }
}

#[tokio::test]
async fn resumed_watcher_replaces_an_edge_still_being_detached() {
    let watcher = DeathWatch::new(2);
    let (sent, received) = mpsc::channel();
    watcher
        .set_recipient(Arc::new(Endpoint(move |e: ActorTerminated| {
            sent.send(e.generation).unwrap();
        })))
        .unwrap();
    let targets = [DeathWatch::new(1), DeathWatch::new(1)];
    for target in &targets {
        target.watch(&watcher).unwrap();
    }
    // Hold the first peer in the outgoing map's drain order. The second peer
    // remains available for a real registration while suspend unlinks the first.
    let first = *watcher
        .0
        .state
        .lock()
        .unwrap()
        .outgoing
        .keys()
        .next()
        .unwrap();
    let index = usize::from(targets[1].id() == first);
    let target = &targets[1 - index];
    std::thread::scope(|scope| {
        let held = targets[index].0.state.lock().unwrap();
        let suspending = scope.spawn(|| watcher.suspend());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !watcher.0.state.lock().unwrap().suspended {
            assert!(
                std::time::Instant::now() < deadline,
                "suspend did not detach outgoing edges"
            );
            std::thread::yield_now();
        }
        watcher.resume().unwrap();
        target.watch(&watcher).unwrap();
        target.watch(&watcher).unwrap();
        drop(held);
        suspending.join().unwrap();
    });
    assert_eq!(target.subscriptions(), (1, 0));
    assert_eq!(watcher.subscriptions(), (0, 1));
    target
        .terminated_checked(event(target), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(received.try_iter().collect::<Vec<_>>(), [target.id()]);
    assert_eq!(watcher.subscriptions(), (0, 0));
}

async fn cancel_sibling_before_construction(close: bool) {
    let target = DeathWatch::new(2);
    let watchers = [DeathWatch::new(1), DeathWatch::new(1)];
    let (sent, received) = mpsc::channel();
    for (index, watcher) in watchers.iter().enumerate() {
        let peer = Arc::downgrade(&watchers[1 - index].0);
        let target_ref = Arc::downgrade(&target.0);
        let sent = sent.clone();
        watcher
            .set_recipient(Arc::new(Endpoint(move |e: ActorTerminated| {
                sent.send((index, e.generation)).unwrap();
                let peer = DeathWatch(peer.upgrade().unwrap());
                if close {
                    peer.close();
                } else {
                    DeathWatch(target_ref.upgrade().unwrap()).unwatch(&peer);
                }
            })))
            .unwrap();
        target.watch(watcher).unwrap();
    }
    target
        .terminated_checked(event(&target), Duration::from_secs(1))
        .await
        .unwrap();
    let events = received.try_iter().collect::<Vec<_>>();
    assert_eq!(
        events.len(),
        1,
        "a cancelled sibling endpoint received the event: {events:?}"
    );
    assert_eq!(events[0].1, target.id());
    assert_eq!(target.delivery_failures(), 0);
    for watcher in watchers {
        assert_eq!(watcher.subscriptions(), (0, 0));
    }
}

#[tokio::test]
async fn unwatch_before_dispatch_skips_recipient_construction() {
    cancel_sibling_before_construction(false).await;
}

#[tokio::test]
async fn watcher_close_before_dispatch_skips_recipient_construction() {
    cancel_sibling_before_construction(true).await;
}
