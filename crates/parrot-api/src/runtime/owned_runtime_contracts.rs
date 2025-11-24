use super::*;
use std::{
    cell::RefCell,
    sync::atomic::{AtomicUsize, Ordering},
};

const WAIT: Duration = Duration::from_secs(5);

struct ExitProbe {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: mpsc::Receiver<()>,
    exited: Arc<AtomicUsize>,
}

impl Drop for ExitProbe {
    fn drop(&mut self) {
        let _ = self.entered.take().unwrap().send(());
        let _ = self.release.recv();
        self.exited.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    static EXIT: RefCell<Option<ExitProbe>> = const { RefCell::new(None) };
}

fn probe() -> (
    ExitProbe,
    tokio::sync::oneshot::Receiver<()>,
    mpsc::Sender<()>,
    Arc<AtomicUsize>,
) {
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, gate) = mpsc::channel::<()>();
    let exited = Arc::new(AtomicUsize::new(0));
    (
        ExitProbe {
            entered: Some(entered),
            release: gate,
            exited: exited.clone(),
        },
        observed,
        release,
        exited,
    )
}

#[tokio::test]
async fn retirement_receipt_waits_for_owner_thread_tls_and_actual_join() {
    let (input, entered, release, exited) = probe();
    let owner = OwnedRuntime::start_with(move || {
        EXIT.with(|slot| *slot.borrow_mut() = Some(input));
        Ok(tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap())
    })
    .await
    .unwrap();
    let receipt = owner.retirements[0].clone();
    owner.request_stop();
    tokio::time::timeout(WAIT, entered).await.unwrap().unwrap();
    let mut first = Box::pin(receipt.wait());
    assert!(
        futures::poll!(&mut first).is_pending(),
        "owner TLS is not an OS join receipt"
    );
    drop(first);
    drop(release);
    tokio::time::timeout(WAIT, receipt.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exited.load(Ordering::SeqCst), 1);
    assert!(receipt.thread.lock().unwrap().is_none());
    receipt.wait().await.unwrap();
}

#[tokio::test]
async fn owner_thread_creation_failure_never_builds_any_runtime() {
    let built = Arc::new(AtomicUsize::new(0));
    let building = built.clone();
    let result = OwnedRuntime::start_with_spawn(
        move || {
            building.fetch_add(1, Ordering::SeqCst);
            Err("builder must not run".into())
        },
        |_| Err(std::io::Error::other("injected owner thread exhaustion")),
    )
    .await;
    assert!(matches!(result, Err(error) if error.contains("injected owner thread exhaustion")));
    assert_eq!(built.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn partial_worker_pool_failure_joins_the_already_created_runtime_before_error() {
    let (input, entered, release, exited) = probe();
    let input = Arc::new(Mutex::new(Some(input)));
    let startup = tokio::spawn(OwnedRuntime::new_with_builder(
        Some(2),
        None,
        LoadBalancingStrategy::LeastLoaded,
        move |index, builder| {
            if index == 1 {
                return Err(std::io::Error::other("injected second runtime failure"));
            }
            let input = input.clone();
            builder.on_thread_start(move || {
                EXIT.with(|slot| *slot.borrow_mut() = input.lock().unwrap().take());
            });
            let runtime = builder.build()?;
            runtime.block_on(runtime.spawn(async {})).unwrap();
            Ok(runtime)
        },
    ));
    tokio::time::timeout(WAIT, entered).await.unwrap().unwrap();
    assert!(!startup.is_finished());
    assert_eq!(exited.load(Ordering::SeqCst), 0);
    drop(release);
    assert!(
        matches!(tokio::time::timeout(WAIT, startup).await.unwrap().unwrap(),
        Err(error) if error.contains("injected second runtime failure"))
    );
    assert_eq!(exited.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_startup_keeps_the_exact_owner_join_and_reclaims_late_resources() {
    let (building, entered) = tokio::sync::oneshot::channel();
    let (release, gate) = mpsc::channel::<()>();
    let owner_id = Arc::new(Mutex::new(None));
    let record = owner_id.clone();
    let mut startup = Box::pin(OwnedRuntime::start_with_spawn(
        move || {
            let _ = building.send(());
            let _ = gate.recv();
            Ok(tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .build()
                .unwrap())
        },
        move |job| {
            let thread = thread::Builder::new().spawn(job)?;
            *record.lock().unwrap() = Some(thread.thread().id());
            Ok(thread)
        },
    ));
    assert!(futures::poll!(&mut startup).is_pending());
    tokio::time::timeout(WAIT, entered).await.unwrap().unwrap();
    drop(startup);
    let join_owner = JoinOwner::shared().unwrap();
    let receipt = join_owner
        .pending
        .lock()
        .unwrap()
        .iter()
        .find(|job| Some(job.thread.thread().id()) == *owner_id.lock().unwrap())
        .expect("cancelled startup lost its OS join handle")
        .receipt
        .clone();
    drop(release);
    tokio::time::timeout(WAIT, receipt.wait())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn last_reference_on_an_owned_worker_is_joined_without_a_new_cleanup_thread() {
    let owner = Arc::new(
        OwnedRuntime::new(Some(1), None, LoadBalancingStrategy::RoundRobin)
            .await
            .unwrap(),
    );
    let receipt = owner.retirements[0].clone();
    let worker_owner = owner.clone();
    let (finish, gate) = tokio::sync::oneshot::channel();
    let task = owner.spawn_on(0, async move {
        let _ = gate.await;
        drop(worker_owner);
    });
    drop(owner);
    finish.send(()).unwrap();
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    tokio::time::timeout(WAIT, receipt.wait())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn cancelled_later_placement_startup_retires_earlier_and_late_workers() {
    let (first, first_entered, first_release, first_exited) = probe();
    let (second, second_entered, second_release, second_exited) = probe();
    let inputs = Arc::new(Mutex::new(vec![Some(first), Some(second)]));
    let ids = Arc::new(Mutex::new(Vec::new()));
    let record = ids.clone();
    let (built, observed) = tokio::sync::oneshot::channel();
    let built = Mutex::new(Some(built));
    let (release, gate) = mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let startup = tokio::spawn(OwnedRuntime::new_with_builder(
        Some(2),
        None,
        LoadBalancingStrategy::LeastLoaded,
        move |index, builder| {
            record.lock().unwrap().push(thread::current().id());
            let inputs = inputs.clone();
            builder.on_thread_start(move || {
                EXIT.with(|slot| *slot.borrow_mut() = inputs.lock().unwrap()[index].take());
            });
            let runtime = builder.build()?;
            runtime.block_on(runtime.spawn(async {})).unwrap();
            if index == 1 {
                let _ = built.lock().unwrap().take().unwrap().send(());
                let _ = gate.lock().unwrap().recv();
            }
            Ok(runtime)
        },
    ));
    tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
    startup.abort();
    assert!(matches!(startup.await, Err(error) if error.is_cancelled()));
    let join_owner = JoinOwner::shared().unwrap();
    let receipts: Vec<_> = join_owner
        .pending
        .lock()
        .unwrap()
        .iter()
        .filter(|job| ids.lock().unwrap().contains(&job.thread.thread().id()))
        .map(|job| job.receipt.clone())
        .collect();
    assert_eq!(
        receipts.len(),
        2,
        "cancelled group startup lost an owner join"
    );
    drop(release);
    tokio::time::timeout(WAIT, async {
        first_entered.await.unwrap();
        second_entered.await.unwrap();
    })
    .await
    .unwrap();
    drop(first_release);
    drop(second_release);
    tokio::time::timeout(WAIT, async {
        for receipt in receipts {
            receipt.wait().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(first_exited.load(Ordering::SeqCst), 1);
    assert_eq!(second_exited.load(Ordering::SeqCst), 1);
}

#[test]
fn host_executor_exit_cannot_cancel_the_owned_runtime_join() {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let receipt = host.block_on(async {
        let owner = OwnedRuntime::new(Some(1), None, LoadBalancingStrategy::RoundRobin)
            .await
            .unwrap();
        let receipt = owner.retirements[0].clone();
        let (entered, observed) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _owner = owner;
            entered.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        observed.await.unwrap();
        receipt
    });
    drop(host);
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    observer.block_on(async {
        tokio::time::timeout(WAIT, receipt.wait())
            .await
            .unwrap()
            .unwrap();
    });
}
