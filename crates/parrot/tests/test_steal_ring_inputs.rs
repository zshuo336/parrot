use parrot::thread::{
    mailbox::mpsc::MpscMailbox,
    scheduler::steal::{MailboxRef, StealDeque, StealRing},
};
use parrot_api::address::ActorPath;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn mailbox(id: usize) -> MailboxRef {
    Arc::new(MpscMailbox::new(
        1,
        ActorPath::placeholder(format!("/steal-input/{id}")),
    ))
}

#[test]
fn requested_capacity_is_exact_and_reusable() {
    for capacity in [1, 2, 3, 4, 7, 17] {
        let queue = StealDeque::new(capacity);
        for turn in 0..3 {
            for item in 0..capacity {
                assert!(queue.push((turn, item)));
            }
            assert!(
                !queue.push((turn, capacity)),
                "queue exceeded requested capacity {capacity}"
            );
            assert_eq!(queue.len(), capacity);
            for item in 0..capacity {
                let actual = if item % 2 == 0 {
                    queue.pop()
                } else {
                    queue.steal()
                };
                assert_eq!(actual, Some((turn, item)));
            }
            assert!(queue.is_empty());
        }
    }
}

#[test]
#[should_panic(expected = "steal queue capacity must be positive")]
fn zero_capacity_is_rejected_at_queue_construction() {
    let _ = StealDeque::<u8>::new(0);
}

#[test]
#[should_panic(expected = "steal ring worker count must be positive")]
fn zero_workers_are_rejected_before_allocating_queues() {
    let _ = StealRing::new(0, usize::MAX);
}

#[test]
#[should_panic(expected = "steal queue capacity must be positive")]
fn zero_capacity_is_rejected_at_ring_construction() {
    let _ = StealRing::new(1, 0);
}

#[test]
fn modulo_worker_aliases_never_steal_their_own_mailbox() {
    for workers in [1, 2, 3, 5] {
        for index in (0..workers * 3).chain([usize::MAX]) {
            let ring = StealRing::new(workers, 1);
            assert!(Arc::ptr_eq(ring.local(index), ring.local(index % workers)));
            let own = mailbox(index);
            assert!(ring.local(index).push(own.clone()));
            assert!(
                ring.steal_from_others(index).is_none(),
                "worker alias {index} stole its own queue in a {workers}-worker ring"
            );
            assert!(Arc::ptr_eq(&ring.local(index).pop().unwrap(), &own));
        }
    }
}

#[test]
fn scanning_reaches_every_other_queue_and_preserves_each_fifo() {
    let ring = StealRing::new(5, 2);
    let own = mailbox(100);
    assert!(ring.local(1).push(own.clone()));
    let lanes: Vec<Vec<MailboxRef>> = [0, 2, 4]
        .into_iter()
        .map(|worker| {
            (0..2)
                .map(|item| {
                    let message = mailbox(worker * 2 + item);
                    assert!(ring.local(worker).push(message.clone()));
                    message
                })
                .collect()
        })
        .collect();
    let mut retired = [0; 3];
    for _ in 0..6 {
        let actual = ring.steal_from_others(1).expect("other work was lost");
        let lane = lanes
            .iter()
            .position(|items| items.iter().any(|item| Arc::ptr_eq(item, &actual)))
            .expect("stole work belonging to the caller");
        assert!(retired[lane] < 2, "mailbox delivered twice");
        assert!(Arc::ptr_eq(&actual, &lanes[lane][retired[lane]]));
        retired[lane] += 1;
    }
    assert_eq!(retired, [2; 3]);
    assert!(ring.steal_from_others(1).is_none());
    assert!(Arc::ptr_eq(&ring.local(1).pop().unwrap(), &own));
}

struct Item {
    id: usize,
    drops: Arc<[AtomicUsize; 5]>,
}
impl Drop for Item {
    fn drop(&mut self) {
        self.drops[self.id].fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn rejection_and_close_retire_each_owner_once_without_discarding_accepted_work() {
    let drops = Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
    let item = |id| Item {
        id,
        drops: drops.clone(),
    };
    let queue = StealDeque::new(4);
    for id in 0..4 {
        assert!(queue.push(item(id)));
    }
    assert!(!queue.push(item(4)));
    assert_eq!(drops[4].load(Ordering::SeqCst), 1);
    queue.close();
    queue.close();
    assert!(!queue.push(item(4)));
    assert_eq!(drops[4].load(Ordering::SeqCst), 2);
    let first = queue.pop().unwrap();
    assert_eq!(first.id, 0);
    assert_eq!(drops[0].load(Ordering::SeqCst), 0);
    drop(first);
    assert!(
        !queue.push(item(4)),
        "closed queue admitted into a free slot"
    );
    assert_eq!(drops[4].load(Ordering::SeqCst), 3);
    let second = queue.steal().unwrap();
    assert_eq!(second.id, 1);
    drop(second);
    drop(queue);
    for id in 0..4 {
        assert_eq!(drops[id].load(Ordering::SeqCst), 1, "owner {id}");
    }
}
