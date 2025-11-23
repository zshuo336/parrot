use super::*;
use crate::owned_payload::permit_wake_contracts::quota;
use std::{sync::mpsc, time::Duration};

const WAIT: Duration = Duration::from_secs(3);

#[test]
fn actual_join_contains_a_drain_admission_waker_panic() {
    crate::test_support::isolated(
        "thread::mailbox::drain::permit_wake_contracts::actual_join_contains_a_drain_admission_waker_panic",
        || {
            let (permit, waiter) = quota();
            let (entered, observe_entered) = mpsc::sync_channel(1);
            let (release, wait) = mpsc::sync_channel(1);
            let thread = std::thread::spawn(move || {
                entered.send(()).unwrap();
                wait.recv_timeout(WAIT).unwrap();
            });
            observe_entered.recv_timeout(WAIT).unwrap();
            let owner = Join {
                thread: Some(thread),
                _permit: permit.into(),
            };
            release.send(()).unwrap();
            let joined = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(owner)));
            assert!(
                joined.is_ok(),
                "native drain quota release unwound its join owner"
            );
            drop(waiter.ready().unwrap());
        },
    );
}

#[test]
fn cancelled_reservation_preserves_an_existing_unwind() {
    crate::test_support::isolated(
        "thread::mailbox::drain::permit_wake_contracts::cancelled_reservation_preserves_an_existing_unwind",
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let (permit, waiter) = quota();
            let reservation = Reservation::Native {
                permit: permit.into(),
                runtime: runtime.handle().clone(),
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _reservation = reservation;
                panic!("original drain cancellation unwind");
            }));
            assert_eq!(
                result.unwrap_err().downcast_ref::<&str>(),
                Some(&"original drain cancellation unwind")
            );
            drop(waiter.ready().unwrap());
        },
    );
}
