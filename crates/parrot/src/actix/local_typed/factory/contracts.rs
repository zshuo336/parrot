use super::*;
use crate::constructor::{ConstructionControl, admission::Admission};
use std::{
    rc::Rc,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
};
use tokio::sync::Semaphore;

const WAIT: Duration = Duration::from_secs(5);

struct Probe {
    local: Rc<()>,
    owner: std::thread::ThreadId,
    disposal: Option<(mpsc::Sender<()>, Arc<Gate>)>,
    bad_drop: bool,
}
impl Probe {
    fn new() -> Self {
        Self {
            local: Rc::new(()),
            owner: std::thread::current().id(),
            disposal: None,
            bad_drop: false,
        }
    }
}
impl LocalTypedDispatch for Probe {
    type Msg = ();
    type Reply = ();
    fn dispatch<'a>(
        &'a mut self,
        _: (),
        _: &'a parrot_api::typed_context::TypedContext,
    ) -> parrot_api::local_typed::LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
        assert_eq!(Rc::strong_count(&self.local), 1);
        if let Some((entered, gate)) = &self.disposal {
            entered.send(()).unwrap();
            let (open, _) = gate
                .changed
                .wait_timeout_while(gate.open.lock().unwrap(), WAIT, |open| !*open)
                .unwrap();
            assert!(*open, "observer must release the actual Local destructor");
        }
        assert!(!self.bad_drop, "Local prepared output disposal failed");
    }
}
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}
struct Open(Arc<Gate>);
impl Drop for Open {
    fn drop(&mut self) {
        *self.0.open.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
fn control() -> Arc<ActorControl> {
    ActorControl::with_lifetime(WAIT, 1, None, Arc::new(Semaphore::new(1))).0
}
fn source(calls: &Arc<AtomicUsize>) -> OwnedPayload<impl Fn() -> Probe + Send + 'static> {
    let calls = calls.clone();
    OwnedPayload::new(
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Probe::new()
        },
        &tokio::runtime::Handle::current(),
        &Arc::new(Semaphore::new(1)),
    )
}

#[actix::test]
async fn stop_while_waiting_for_worker_returns_invocation_without_calling_local_constructor() {
    let admission = Admission::new(1);
    let workers = Arc::new(Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = source(&calls);
    let control = control();
    let mut waiting = Box::pin(prepare(
        &factory,
        &control,
        workers,
        &admission,
        tokio::time::Instant::now() + WAIT,
        None,
    ));
    assert!(futures::poll!(&mut waiting).is_pending());
    assert_eq!(admission.available_permits(), 0);
    control.request_stop(tokio::time::Instant::now() + WAIT);
    assert!(
        tokio::time::timeout(WAIT, waiting)
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    factory.retire().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(admission.available_permits(), 1);
}

#[actix::test]
async fn invocation_wait_obeys_original_deadline_without_calling_local_constructor() {
    let admission = Admission::new(1);
    let held = admission
        .acquire::<()>(&ConstructionControl::default())
        .await
        .unwrap();
    let workers = Arc::new(Semaphore::new(1));
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = source(&calls);
    let control = control();
    let result = prepare(
        &factory,
        &control,
        workers.clone(),
        &admission,
        tokio::time::Instant::now() + Duration::from_millis(20),
        None,
    )
    .await;
    assert!(matches!(result, Err(ActorError::Timeout)));
    assert!(control.deadline_was_enforced());
    assert_eq!(workers.available_permits(), 1);
    factory.retire().await.unwrap();
    drop(held);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(admission.available_permits(), 1);
}

async fn late_output_disposal(bad_drop: bool) {
    tokio::time::pause();
    let admission = Admission::new(1);
    let workers = Arc::new(Semaphore::new(1));
    let gate = Arc::new(Gate::default());
    let (entered, observed) = mpsc::channel();
    let captured = gate.clone();
    let factory = OwnedPayload::new(
        move || {
            let mut actor = Probe::new();
            actor.disposal = Some((entered.clone(), captured.clone()));
            actor.bad_drop = bad_drop;
            actor
        },
        &tokio::runtime::Handle::current(),
        &Arc::new(Semaphore::new(1)),
    );
    let control = control();
    let deadline = control.begin_construction(tokio::time::Instant::now() + WAIT);
    let prepared = prepare(
        &factory,
        &control,
        workers.clone(),
        &admission,
        deadline,
        None,
    )
    .await
    .unwrap();
    let (finished, receipt) = tokio::sync::oneshot::channel();
    let mut inputs = Queued::<Probe> {
        receiver: None,
        creation_context: None,
        control: control.clone(),
        constructor_slots: workers.clone(),
        factory_admission: admission.clone(),
        policy_slots: Arc::new(Semaphore::new(1)),
        finished: Some(finished),
    }
    .activate();
    inputs.install(prepared);
    tokio::time::advance(WAIT).await;
    assert!(matches!(
        control.construction_complete(deadline),
        Err(ActorError::Timeout)
    ));
    assert_eq!(workers.available_permits(), 1);
    assert_eq!(admission.available_permits(), 0);
    let observed_admission = admission.clone();
    let observer = std::thread::spawn(move || {
        let release = Open(gate);
        observed.recv_timeout(WAIT).unwrap();
        let available_during_disposal = observed_admission.available_permits();
        drop(release);
        available_during_disposal
    });
    let result = inputs.retire();
    assert_eq!(observer.join().unwrap(), 0);
    assert_eq!(result.is_err(), bad_drop);
    assert_eq!(receipt.await.unwrap().result().is_err(), bad_drop);
    factory.retire().await.unwrap();
    assert_eq!(admission.available_permits(), 1);
}

#[actix::test]
async fn expired_local_output_keeps_invocation_until_actual_owner_thread_disposal() {
    late_output_disposal(false).await;
}

#[actix::test]
async fn panicking_local_output_returns_invocation_after_actual_owner_thread_disposal() {
    late_output_disposal(true).await;
}

#[actix::test]
async fn local_group_reservation_progresses_with_one_worker_ahead_of_queued_native_factory() {
    use crate::children::recovery::{Recovery, Step};
    let admission = Admission::new(2);
    let workers = Arc::new(Semaphore::new(1));
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = source(&calls);
    let ids = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
    let deadline = tokio::time::Instant::now() + WAIT;
    let group = Recovery::new(
        uuid::Uuid::new_v4(),
        ids.to_vec(),
        deadline,
        Box::new(|_| {}),
    );
    for result in
        futures::future::join_all(ids.iter().map(|id| group.arrive(*id, Step::Retired))).await
    {
        result.unwrap();
    }
    let first_control = control();
    let second_control = control();
    let first = group.creation_control(ids[0], deadline).unwrap();
    let second = group.creation_control(ids[1], deadline).unwrap();
    let first = prepare(
        &factory,
        &first_control,
        workers.clone(),
        &admission,
        deadline,
        Some(&first),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(admission.available_permits(), 0);
    assert_eq!(workers.available_permits(), 1);
    let native = crate::constructor::Factory::with_admission(
        || 42,
        workers.clone(),
        Arc::new(Semaphore::new(1)),
        admission.clone(),
    );
    let waiting = native.prepare::<()>(Arc::default());
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    let second = tokio::time::timeout(
        WAIT,
        prepare(
            &factory,
            &second_control,
            workers,
            &admission,
            deadline,
            Some(&second),
        ),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    for step in [Step::Prepared, Step::Published] {
        for result in futures::future::join_all(ids.iter().map(|id| group.arrive(*id, step))).await
        {
            result.unwrap();
        }
    }
    drop(first);
    assert!(futures::poll!(&mut waiting).is_pending());
    drop(second);
    let native_output = tokio::time::timeout(WAIT, waiting).await.unwrap().unwrap();
    assert_eq!(native_output.adopt(), 42);
    native.retire().await.unwrap();
    factory.retire().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(admission.available_permits(), 2);
}

#[actix::test]
async fn aborted_local_group_returns_partial_queue_reservation_without_late_construction() {
    use crate::children::recovery::{Recovery, Step};
    let admission = Admission::new(2);
    let held = admission
        .acquire::<()>(&ConstructionControl::default())
        .await
        .unwrap();
    let workers = Arc::new(Semaphore::new(1));
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = source(&calls);
    let ids = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
    let deadline = tokio::time::Instant::now() + WAIT;
    let group = Recovery::new(
        uuid::Uuid::new_v4(),
        ids.to_vec(),
        deadline,
        Box::new(|_| {}),
    );
    for result in
        futures::future::join_all(ids.iter().map(|id| group.arrive(*id, Step::Retired))).await
    {
        result.unwrap();
    }
    let control = control();
    let member = group.creation_control(ids[0], deadline).unwrap();
    let mut waiting = Box::pin(prepare(
        &factory,
        &control,
        workers.clone(),
        &admission,
        deadline,
        Some(&member),
    ));
    assert!(futures::poll!(&mut waiting).is_pending());
    group.cancel("cancel Local group before invocation capacity is available");
    assert!(matches!(
        tokio::time::timeout(WAIT, waiting).await.unwrap(),
        Err(ActorError::Stopped)
    ));
    for id in ids {
        group.retired_after_abort(id);
    }
    drop(held);
    factory.retire().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(workers.available_permits(), 1);
    assert_eq!(admission.available_permits(), 2);
}
