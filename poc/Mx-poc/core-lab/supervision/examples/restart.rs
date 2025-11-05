//! Real state reconstruction followed by actual sibling-tree retirement.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use supervision::{ChildFactory, DeathReason, SupervisionStrategy, Supervisor, WatchEvent};
fn main() {
    let supervisor = Supervisor::new();
    let builds = Arc::new(AtomicUsize::new(0));
    let count = builds.clone();
    let factory: ChildFactory = Box::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        let mut state = 0u64;
        Box::new(move |msg| {
            let n = *msg.downcast::<u64>().unwrap();
            if n == 999 {
                panic!("demonstration failure");
            }
            state += n;
            Ok(Box::new(state))
        })
    });
    supervisor
        .spawn_supervised(
            factory,
            "counter",
            SupervisionStrategy::OneForOne {
                max_restarts: 1,
                window: Duration::from_secs(30),
            },
        )
        .unwrap();
    supervisor
        .spawn_supervised(
            Box::new(|| Box::new(Ok)),
            "sibling",
            SupervisionStrategy::Stop,
        )
        .unwrap();
    let counter = supervisor.watch("observer", "counter").unwrap();
    let sibling = supervisor.watch("observer", "sibling").unwrap();
    supervisor.ask("counter", Box::new(10u64)).unwrap();
    assert!(supervisor.ask("counter", Box::new(999u64)).is_err());
    let result = supervisor.ask("counter", Box::new(1u64)).unwrap();
    assert_eq!(*result.downcast::<u64>().unwrap(), 1);
    println!(
        "counter rebuilt with fresh state; factories={}",
        builds.load(Ordering::SeqCst)
    );
    assert!(supervisor.ask("counter", Box::new(999u64)).is_err());
    let WatchEvent::Terminated { reason, .. } =
        counter.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(matches!(reason, DeathReason::Escalated(_)));
    let WatchEvent::Terminated { reason, .. } =
        sibling.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(reason, DeathReason::Killed);
    supervisor.shutdown().unwrap();
    assert_eq!(supervisor.worker_counts(), (2, 2));
    assert_eq!(supervisor.alive_count(), 0);
    println!("restart budget exhausted; both child workers joined and resources retired");
}
