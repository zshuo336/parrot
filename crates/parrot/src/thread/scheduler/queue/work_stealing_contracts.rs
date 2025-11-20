use super::*;
use crate::thread::mailbox::mpsc::MpscMailbox;
use parrot_api::address::ActorPath;

fn ready(queue: &SchedulingQueue, name: &str, lane: Lane, worker: Option<usize>) -> ReadyMailbox {
    let mailbox: ReadyMailbox = Arc::new(MpscMailbox::new(1, ActorPath::placeholder(name)));
    assert!(queue.register(&mailbox));
    assert!(mailbox.schedule_state().try_enqueue());
    assert!(queue.enqueue_on_worker(mailbox.clone(), lane, worker));
    mailbox
}
fn take(queue: &SchedulingQueue, worker: usize, expected: &ReadyMailbox) {
    let actual = queue
        .try_pop_for_worker(worker)
        .expect("accepted readiness must progress");
    assert!(Arc::ptr_eq(&actual, expected));
    actual.schedule_state().force_release();
}

#[test]
fn local_fifo_stealing_and_global_injection_share_one_capacity() {
    let queue = SchedulingQueue::with_workers(3, 2);
    let a = ready(&queue, "a", Lane::Normal, Some(0));
    let b = ready(&queue, "b", Lane::Normal, Some(1));
    let c = ready(&queue, "c", Lane::Normal, Some(0));
    let d = ready(&queue, "d", Lane::Normal, None);
    assert_eq!(
        (
            queue.len(),
            queue.normal_len(),
            queue.local_len(),
            queue.deferred_len()
        ),
        (3, 3, 3, 1)
    );
    let rejected: ReadyMailbox = Arc::new(MpscMailbox::new(1, ActorPath::placeholder("rejected")));
    assert!(Arc::ptr_eq(
        &queue.push(rejected.clone()).unwrap_err(),
        &rejected
    ));
    take(&queue, 1, &b);
    assert_eq!(
        (queue.len(), queue.local_len(), queue.deferred_len()),
        (3, 2, 0)
    );
    take(&queue, 1, &a);
    take(&queue, 1, &c);
    take(&queue, 1, &d);
    assert_eq!((queue.local_pops(), queue.steals()), (1, 2));
    assert!(queue.try_pop_for_worker(0).is_none());
}

#[test]
fn busy_local_worker_periodically_steals_older_work_from_a_blocked_owner() {
    let queue = SchedulingQueue::with_workers(4, 2);
    let remote = ready(&queue, "blocked-owner-ready", Lane::Normal, Some(0));
    let local = ready(&queue, "continuous-local-backlog", Lane::Normal, Some(1));
    for _ in 0..4 {
        take(&queue, 1, &local);
        assert!(local.schedule_state().try_enqueue());
        assert!(queue.enqueue_on_worker(local.clone(), Lane::Normal, Some(1)));
    }
    take(&queue, 1, &remote);
    take(&queue, 1, &local);
    assert_eq!(queue.steals(), 1);
    assert!(queue.is_empty());
}

#[test]
fn a_full_local_quota_falls_back_to_global_without_multiplying_storage() {
    let queue = SchedulingQueue::with_workers(4, 2);
    let a = ready(&queue, "a", Lane::Normal, Some(0));
    let b = ready(&queue, "b", Lane::Normal, Some(0));
    let c = ready(&queue, "c", Lane::Normal, Some(0));
    assert_eq!(queue.local_len(), 2);
    assert_eq!(queue.state.lock().unwrap().normal.len(), 1);
    take(&queue, 1, &a);
    take(&queue, 1, &b);
    take(&queue, 1, &c);
    assert_eq!(queue.steals(), 2);
}

#[test]
fn high_priority_deferred_work_preempts_full_local_queues() {
    let queue = SchedulingQueue::with_workers(1, 2);
    let normal = ready(&queue, "normal", Lane::Normal, Some(0));
    let high = ready(&queue, "high", Lane::High, Some(1));
    assert_eq!(queue.deferred_len(), 1);
    take(&queue, 0, &high);
    assert_eq!((queue.len(), queue.local_len()), (1, 1));
    take(&queue, 0, &normal);
    assert_eq!((queue.local_pops(), queue.steals()), (1, 0));
}

#[test]
fn oldest_global_injection_cannot_be_starved_by_a_local_requeue() {
    let queue = SchedulingQueue::with_workers(4, 2);
    let injected = ready(&queue, "injected", Lane::Normal, None);
    let local = ready(&queue, "local", Lane::Normal, Some(0));
    take(&queue, 0, &injected);
    take(&queue, 0, &local);
    assert_eq!(queue.local_pops(), 1);
}

#[test]
fn unregister_removes_local_and_deferred_ownership_before_reregistration() {
    let queue = SchedulingQueue::with_workers(1, 2);
    let local = ready(&queue, "local", Lane::Normal, Some(0));
    let deferred = ready(&queue, "deferred", Lane::Normal, Some(1));
    queue.unregister(&deferred);
    queue.unregister(&local);
    assert_eq!(
        (queue.len(), queue.local_len(), queue.deferred_len()),
        (0, 0, 0)
    );
    for mailbox in [&local, &deferred] {
        assert!(queue.register(mailbox));
        assert!(mailbox.schedule_state().try_enqueue());
        assert!(queue.enqueue_on_worker(mailbox.clone(), Lane::Normal, Some(1)));
    }
    take(&queue, 0, &local);
    take(&queue, 0, &deferred);
    assert!(queue.is_empty());
}

#[test]
fn close_transfers_every_lane_once_and_rejects_all_future_owners() {
    let queue = SchedulingQueue::with_workers(4, 2);
    let expected = [
        ready(&queue, "local0", Lane::Normal, Some(0)),
        ready(&queue, "local1", Lane::Normal, Some(1)),
        ready(&queue, "global", Lane::Normal, None),
        ready(&queue, "high", Lane::High, Some(0)),
        ready(&queue, "deferred", Lane::Normal, Some(1)),
    ];
    let mut taken: Vec<_> = queue
        .close_and_take()
        .into_iter()
        .map(|mailbox| {
            mailbox.schedule_state().force_release();
            identity(&mailbox)
        })
        .collect();
    let mut ids: Vec<_> = expected.iter().map(identity).collect();
    taken.sort_unstable();
    ids.sort_unstable();
    assert_eq!(taken, ids);
    assert_eq!(
        (
            queue.len(),
            queue.high_len(),
            queue.normal_len(),
            queue.local_len(),
            queue.deferred_len()
        ),
        (0, 0, 0, 0, 0)
    );
    assert!(queue.close_and_take().is_empty());
    for mailbox in expected {
        assert!(!queue.register(&mailbox));
        assert!(mailbox.schedule_state().try_enqueue());
        assert!(!queue.enqueue_on_worker(mailbox.clone(), Lane::Normal, Some(0)));
        assert!(
            mailbox.schedule_state().try_enqueue(),
            "rejected readiness must release its slot"
        );
        mailbox.schedule_state().force_release();
    }
}

#[test]
fn concurrent_thieves_deliver_each_saturated_registration_once() {
    let queue = Arc::new(SchedulingQueue::with_workers(3, 4));
    let mailboxes: Vec<_> = (0..200)
        .map(|n| ready(&queue, &format!("actor-{n}"), Lane::Normal, Some(n % 4)))
        .collect();
    let threads: Vec<_> = (0..4)
        .map(|worker| {
            let queue = queue.clone();
            std::thread::spawn(move || {
                let mut taken = Vec::new();
                while let Some(mailbox) = queue.try_pop_for_worker(worker) {
                    taken.push(identity(&mailbox));
                    mailbox.schedule_state().force_release();
                    assert!(queue.len() <= 3);
                }
                taken
            })
        })
        .collect();
    let mut actual: Vec<_> = threads
        .into_iter()
        .flat_map(|thread| thread.join().unwrap())
        .collect();
    let mut expected: Vec<_> = mailboxes.iter().map(identity).collect();
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected);
    assert_eq!((queue.len(), queue.deferred_len()), (0, 0));
}
