use flume::*;
use std::time::{Duration, Instant};

#[test]
fn send_recv() {
    let (tx, rx) = unbounded();
    for i in 0..1000 {
        tx.send(i).unwrap();
    }
    for i in 0..1000 {
        assert_eq!(rx.try_recv().unwrap(), i);
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn iter() {
    let (tx, rx) = unbounded();
    for i in 0..1000 {
        tx.send(i).unwrap();
    }
    drop(tx);
    assert_eq!(rx.iter().sum::<u32>(), (0..1000).sum());
}

#[test]
fn try_iter() {
    let (tx, rx) = unbounded();
    for i in 0..1000 {
        tx.send(i).unwrap();
    }
    assert_eq!(rx.try_iter().sum::<u32>(), (0..1000).sum());
}

#[test]
fn iter_threaded() {
    let (tx, rx) = unbounded();
    for i in 0..1000 {
        let tx = tx.clone();
        std::thread::spawn(move || tx.send(i).unwrap());
    }
    drop(tx);
    assert_eq!(rx.iter().sum::<u32>(), (0..1000).sum());
}

// A deadline is a lower bound for reporting Timeout, not an operating-system
// scheduling guarantee. Keep a separate generous watchdog for broken liveness
// without asserting that this process is rescheduled within five milliseconds.
fn deadline_contract(name: &str, body: impl FnOnce() + Send + 'static) {
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            let _ = done.send(outcome);
        })
        .unwrap();
    let outcome = finished
        .recv_timeout(Duration::from_secs(5))
        .expect("deadline contract did not finish before the liveness watchdog");
    worker.join().unwrap();
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn send_timeout() {
    deadline_contract("timeout_send", || {
        let dur = Duration::from_millis(350);
        let (tx, rx) = bounded(1);
        assert_eq!(tx.send_timeout(42, Duration::ZERO), Ok(()));

        let then = Instant::now();
        assert_eq!(tx.send_timeout(43, dur), Err(SendTimeoutError::Timeout(43)));
        assert!(then.elapsed() >= dur, "send timed out before its deadline");

        // A timed-out send retains its payload and cannot remain queued.
        assert_eq!(rx.try_recv(), Ok(42));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(tx.send_deadline(44, Instant::now()), Ok(()));
        assert_eq!(
            tx.send_deadline(45, Instant::now()),
            Err(SendTimeoutError::Timeout(45))
        );
        assert_eq!(rx.try_recv(), Ok(44));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        drop(rx);
        assert_eq!(
            tx.send_timeout(46, dur),
            Err(SendTimeoutError::Disconnected(46))
        );
    });
}

#[test]
fn recv_timeout() {
    deadline_contract("timeout_recv", || {
        let dur = Duration::from_millis(350);
        let (tx, rx) = unbounded();
        let then = Instant::now();
        assert_eq!(rx.recv_timeout(dur), Err(RecvTimeoutError::Timeout));
        assert!(
            then.elapsed() >= dur,
            "receive timed out before its deadline"
        );

        // Zero timeout still consumes available messages. The earlier waiter
        // must have retired so that it cannot swallow this later send.
        tx.send(42).unwrap();
        assert_eq!(rx.recv_timeout(Duration::ZERO), Ok(42));
        assert_eq!(
            rx.recv_timeout(Duration::ZERO),
            Err(RecvTimeoutError::Timeout)
        );
        tx.send(43).unwrap();
        drop(tx);
        assert_eq!(rx.recv_timeout(Duration::ZERO), Ok(43));
        assert_eq!(rx.recv_timeout(dur), Err(RecvTimeoutError::Disconnected));
    });
}

#[test]
fn recv_deadline() {
    deadline_contract("deadline_recv", || {
        let dur = Duration::from_millis(350);
        let (tx, rx) = unbounded();
        let deadline = Instant::now().checked_add(dur).unwrap();
        assert_eq!(rx.recv_deadline(deadline), Err(RecvTimeoutError::Timeout));
        assert!(
            Instant::now() >= deadline,
            "receive timed out before its deadline"
        );

        // An expired absolute deadline has the same ready/disconnected
        // precedence as a zero relative timeout; it must not become a new wait.
        let expired = Instant::now().checked_sub(dur).unwrap();
        tx.send(42).unwrap();
        assert_eq!(rx.recv_deadline(expired), Ok(42));
        assert_eq!(rx.recv_deadline(expired), Err(RecvTimeoutError::Timeout));
        tx.send(43).unwrap();
        drop(tx);
        assert_eq!(rx.recv_deadline(expired), Ok(43));
        assert_eq!(
            rx.recv_deadline(expired),
            Err(RecvTimeoutError::Disconnected)
        );
    });
}

#[test]
fn recv_timeout_missed_send() {
    let (tx, rx) = bounded(10);

    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());

    tx.send(42).unwrap();

    assert_eq!(rx.recv(), Ok(42));
}

#[test]
fn disconnect_tx() {
    let (tx, rx) = unbounded::<()>();
    drop(tx);
    assert!(rx.recv().is_err());
}

#[test]
fn disconnect_rx() {
    let (tx, rx) = unbounded();
    drop(rx);
    assert!(tx.send(0).is_err());
}

#[test]
fn drain() {
    let (tx, rx) = unbounded();

    for i in 0..100 {
        tx.send(i).unwrap();
    }

    assert_eq!(rx.drain().sum::<u32>(), (0..100).sum());

    for i in 0..100 {
        tx.send(i).unwrap();
    }

    for i in 0..100 {
        tx.send(i).unwrap();
    }

    rx.recv().unwrap();

    (1u32..100)
        .chain(0..100)
        .zip(rx)
        .for_each(|(l, r)| assert_eq!(l, r));
}

#[test]
fn try_send() {
    let (tx, rx) = bounded(5);

    for i in 0..5 {
        tx.try_send(i).unwrap();
    }

    assert!(tx.try_send(42).is_err());

    assert_eq!(rx.recv(), Ok(0));

    assert_eq!(tx.try_send(42), Ok(()));

    assert_eq!(rx.recv(), Ok(1));
    drop(rx);

    assert!(tx.try_send(42).is_err());
}

#[test]
fn send_bounded() {
    let (tx, rx) = bounded(5);

    for _ in 0..5 {
        tx.send(42).unwrap();
    }

    let _ = rx.recv().unwrap();

    tx.send(42).unwrap();

    assert!(tx.try_send(42).is_err());

    rx.drain();

    let mut ts = Vec::new();
    for _ in 0..100 {
        let tx = tx.clone();
        ts.push(std::thread::spawn(move || {
            for i in 0..10000 {
                tx.send(i).unwrap();
            }
        }));
    }

    drop(tx);

    assert_eq!(rx.iter().sum::<u64>(), (0..10000).sum::<u64>() * 100);

    for t in ts {
        t.join().unwrap();
    }

    assert!(rx.recv().is_err());
}

#[test]
fn rendezvous() {
    deadline_contract("rendezvous", || {
        let (tx, rx) = bounded(0);
        for i in 0..5 {
            // No receiver can be waiting: this thread owns the only receiver
            // and has not started receiving. A zero-capacity channel must not
            // buffer either a nonblocking or a timed-out send.
            assert_eq!(tx.try_send(i), Err(TrySendError::Full(i)));
            let timeout = Duration::from_millis(20);
            let started = Instant::now();
            assert_eq!(
                tx.send_timeout(i, timeout),
                Err(SendTimeoutError::Timeout(i))
            );
            assert!(started.elapsed() >= timeout, "rendezvous timed out early");
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

            // The handoff is correct regardless of which thread runs first.
            // The outer watchdog detects broken liveness independently of
            // scheduling order; no sleep is used as a startup barrier.
            let sender = tx.clone();
            let worker = std::thread::spawn(move || {
                sender.send(i).unwrap();
            });
            assert_eq!(rx.recv(), Ok(i));
            worker.join().unwrap();
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        }
        drop(rx);
        assert_eq!(tx.send(5), Err(SendError(5)));
    });
}

#[test]
fn hydra() {
    let thread_num = 32;
    let msg_num = 1000;

    let (main_tx, main_rx) = unbounded::<()>();

    let mut txs = Vec::new();
    for _ in 0..thread_num {
        let main_tx = main_tx.clone();
        let (tx, rx) = unbounded();
        txs.push(tx);

        std::thread::spawn(move || {
            for msg in rx.iter() {
                main_tx.send(msg).unwrap();
            }
        });
    }

    drop(main_tx);

    for _ in 0..10 {
        for tx in &txs {
            for _ in 0..msg_num {
                tx.send(()).unwrap();
            }
        }

        for _ in 0..thread_num {
            for _ in 0..msg_num {
                main_rx.recv().unwrap();
            }
        }
    }

    drop(txs);
    assert!(main_rx.recv().is_err());
}

#[test]
fn robin() {
    let thread_num = 32;
    let msg_num = 10;

    let (mut main_tx, main_rx) = bounded::<()>(1);

    for _ in 0..thread_num {
        let (mut tx, rx) = bounded(100);
        std::mem::swap(&mut tx, &mut main_tx);

        std::thread::spawn(move || {
            for msg in rx.iter() {
                tx.send(msg).unwrap();
            }
        });
    }

    for _ in 0..10 {
        let main_tx = main_tx.clone();
        std::thread::spawn(move || {
            for _ in 0..msg_num {
                main_tx.send(()).unwrap();
            }
        });

        for _ in 0..msg_num {
            main_rx.recv().unwrap();
        }
    }
}

#[cfg(feature = "select")]
#[test]
fn select_general() {
    #[derive(Debug, PartialEq)]
    struct Foo(usize);

    let (tx0, rx0) = bounded(1);
    let (tx1, rx1) = unbounded();

    for (i, t) in vec![tx0.clone(), tx1].into_iter().enumerate() {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let _ = t.send(Foo(i));
        });
    }

    let x = Selector::new()
        .recv(&rx0, |x| x)
        .recv(&rx1, |x| x)
        .wait()
        .unwrap();

    if x == Foo(0) {
        assert!(rx1.recv().unwrap() == Foo(1));
    } else {
        assert!(rx0.recv().unwrap() == Foo(0));
    }

    tx0.send(Foo(42)).unwrap();

    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(rx0.recv().unwrap(), Foo(42));
        assert_eq!(rx0.recv().unwrap(), Foo(43));
    });

    Selector::new().send(&tx0, Foo(43), |x| x).wait().unwrap();

    t.join().unwrap();
}

struct MessageWithoutDebug(u32);

#[test]
// This is a 'does it build' test, to make sure that the error types can turn
// into a std::error::Error without requiring the payload (which is not used
// there) to impl Debug.
fn std_error_without_debug() {
    let (tx, rx) = unbounded::<MessageWithoutDebug>();

    match tx.send(MessageWithoutDebug(1)) {
        Ok(_) => {}
        Err(e) => {
            let _std_err: &dyn std::error::Error = &e;
        }
    }

    match rx.recv() {
        Ok(message) => assert_eq!(message.0, 1),
        Err(e) => {
            let _std_err: &dyn std::error::Error = &e;
        }
    }

    match tx.try_send(MessageWithoutDebug(2)) {
        Ok(_) => {}
        Err(e) => {
            let _std_err: &dyn std::error::Error = &e;
        }
    }

    match rx.try_recv() {
        Ok(message) => assert_eq!(message.0, 2),
        Err(e) => {
            let _std_err: &dyn std::error::Error = &e;
        }
    }

    match tx.send_timeout(MessageWithoutDebug(3), Duration::from_secs(1000000)) {
        Ok(_) => {}
        Err(e) => {
            let _std_err: &dyn std::error::Error = &e;
        }
    }

    match rx.recv_timeout(Duration::from_secs(10000000)) {
        Ok(message) => assert_eq!(message.0, 3),
        Err(e) => {
            let _std_err: &dyn std::error::Error = &e;
        }
    }

    // Exercise the error conversions too, and verify that failed sends keep
    // their non-Debug payload available to the caller.
    drop(rx);
    let error = tx.send(MessageWithoutDebug(4)).err().unwrap();
    let _: &dyn std::error::Error = &error;
    assert_eq!(error.0 .0, 4);
    let error = tx.try_send(MessageWithoutDebug(5)).err().unwrap();
    let _: &dyn std::error::Error = &error;
    assert_eq!(error.into_inner().0, 5);
    let error = tx
        .send_timeout(MessageWithoutDebug(6), Duration::ZERO)
        .err()
        .unwrap();
    let _: &dyn std::error::Error = &error;
    assert_eq!(error.into_inner().0, 6);
}

#[test]
fn weak_close() {
    let (tx, rx) = unbounded::<()>();
    let weak = tx.downgrade();
    drop(tx);
    assert!(weak.upgrade().is_none());
    assert!(rx.is_disconnected());
    assert!(rx.try_recv().is_err());
}

#[test]
fn weak_upgrade() {
    let (tx, rx) = unbounded();
    let weak = tx.downgrade();
    let tx2 = weak.upgrade().unwrap();
    drop(tx);
    assert!(!rx.is_disconnected());
    tx2.send(()).unwrap();
    assert!(rx.try_recv().is_ok());
}
