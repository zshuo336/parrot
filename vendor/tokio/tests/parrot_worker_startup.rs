#![cfg(all(feature = "full", not(target_os = "wasi")))]

use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Barrier, Mutex,
    },
    task::{Context, Poll},
    thread,
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

struct PendingInput(Arc<AtomicUsize>);

impl Future for PendingInput {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        Poll::Pending
    }
}

impl Drop for PendingInput {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct WorkerExit {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    exited: Arc<AtomicUsize>,
}

impl Drop for WorkerExit {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        let _ = self.release.recv();
        self.exited.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    static EXIT: RefCell<Option<WorkerExit>> = const { RefCell::new(None) };
}

#[test]
fn builder_callback_failure_joins_started_worker_tls_and_cancels_its_tasks() {
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (started, startup) = mpsc::channel();
    let exited = Arc::new(AtomicUsize::new(0));
    let inputs = Arc::new(AtomicUsize::new(0));
    let dropped = inputs.clone();
    let probe = Arc::new(Mutex::new(Some(WorkerExit {
        entered,
        release: gate,
        exited: exited.clone(),
    })));
    let builder = thread::spawn(move || {
        let sequence = AtomicUsize::new(0);
        let startup = Mutex::new(startup);
        std::panic::catch_unwind(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_name_fn(move || {
                    if sequence.fetch_add(1, Ordering::SeqCst) == 1 {
                        startup.lock().unwrap().recv_timeout(WAIT).unwrap();
                        panic!("second worker name failure");
                    }
                    "startup-probe".into()
                })
                .on_thread_start(move || {
                    EXIT.with(|slot| *slot.borrow_mut() = probe.lock().unwrap().take());
                    tokio::spawn(PendingInput(dropped.clone()));
                    started.send(()).unwrap();
                })
                .build()
        })
        .is_err()
    });
    let entered = observed.recv_timeout(WAIT);
    let waiting = !builder.is_finished();
    let retired = inputs.load(Ordering::SeqCst);
    drop(release);
    assert!(
        builder.join().unwrap(),
        "builder did not preserve callback panic"
    );
    entered.unwrap();
    assert!(
        waiting,
        "builder failure returned before worker TLS was joined"
    );
    assert_eq!(retired, 1, "startup callback task was not cancelled");
    assert_eq!(exited.load(Ordering::SeqCst), 1);
}

#[test]
fn successful_build_starts_and_joins_every_worker() {
    let ready = Arc::new(Barrier::new(5));
    let worker = ready.clone();
    let exited = Arc::new(AtomicUsize::new(0));
    let stopping = exited.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .on_thread_start(move || {
            worker.wait();
        })
        .on_thread_stop(move || {
            stopping.fetch_add(1, Ordering::SeqCst);
        })
        .build()
        .unwrap();
    ready.wait();
    assert_eq!(runtime.block_on(runtime.spawn(async { 42 })).unwrap(), 42);
    drop(runtime);
    assert_eq!(exited.load(Ordering::SeqCst), 4);
}
