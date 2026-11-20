use super::*;
use parrot_api::{local_typed::LocalBoxedFuture, types::ActorResult};
use std::{
    rc::Rc,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct CaptureGate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
struct Capture(Arc<CaptureGate>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.entered.notify_one();
        let (open, _) = self
            .0
            .changed
            .wait_timeout_while(
                self.0.open.lock().unwrap(),
                Duration::from_secs(10),
                |open| !*open,
            )
            .unwrap();
        assert!(
            *open,
            "test must release the actual constructor capture destructor"
        );
    }
}
struct Release(Arc<CaptureGate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.open.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
struct Probe {
    _local: Rc<()>,
    initialized: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
}
impl LocalTypedDispatch for Probe {
    type Msg = ();
    type Reply = ();
    fn init<'a>(
        &'a mut self,
        _: &'a parrot_api::typed_context::TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.initialized.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        Box::pin(async { Ok(()) })
    }
    fn dispatch<'a>(
        &'a mut self,
        _: (),
        _: &'a parrot_api::typed_context::TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[actix::test]
async fn one_shot_capture_retirement_keeps_original_deadline_and_prevents_late_init() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    tokio::time::pause();
    let budget = Duration::from_secs(5);
    let (control, _) =
        ActorControl::with_lifetime(budget, 1, None, Arc::new(tokio::sync::Semaphore::new(1)));
    // Use the real owner-thread execution path with a real managed context.
    // No coordinator runs here, so the construction path must itself retain
    // and recheck the original deadline after the independent capture receipt.
    let tree = crate::typed_context::TypedTree::new_backend(
        crate::typed_context::Backend::Actix(Box::new(system.downgrade())),
        crate::typed_context::Identity {
            path: "/capture-deadline".into(),
            own: Arc::default(),
            parent: None,
        },
        crate::typed_context::Hooks {
            stop: Arc::new(|_| {}),
            escalate: Arc::new(|_| {}),
        },
        budget,
        tokio::runtime::Handle::current(),
        system.policy_slots.clone(),
        system.cleanup_slots.clone(),
    );
    control
        .typed_tree
        .set(Arc::new(tree))
        .unwrap_or_else(|_| unreachable!());
    let initialized = Arc::new(AtomicUsize::new(0));
    let init_entered = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(CaptureGate::default());
    let release = Release(gate.clone());
    let capture = Capture(gate.clone());
    let calls = initialized.clone();
    let entered = init_entered.clone();
    let factory = crate::owned_payload::OwnedPayload::new(
        move || {
            std::hint::black_box(&capture);
            Probe {
                _local: Rc::new(()),
                initialized: calls.clone(),
                entered: entered.clone(),
            }
        },
        &tokio::runtime::Handle::current(),
        &system.cleanup_slots,
    );
    let policy = crate::owned_payload::OwnedPayload::new(
        None,
        &tokio::runtime::Handle::current(),
        &system.cleanup_slots,
    );
    let (_, receiver) = parrot_api::typed_channel::typed_channel::<LocalProtocol<Probe>>(1);
    let (finished, retirement) = tokio::sync::oneshot::channel();
    let queued = Queued {
        receiver: Some(receiver),
        creation_context: None,
        control: control.clone(),
        constructor_slots: system.constructor_slots.clone(),
        factory_admission: system.factory_admission.clone(),
        policy_slots: system.policy_slots.clone(),
        finished: Some(finished),
    };
    let mut execution = Box::pin(run_source(
        queued,
        factory,
        policy,
        control.clone(),
        tokio::runtime::Handle::current(),
        false,
    ));
    assert!(futures::poll!(&mut execution).is_pending());
    gate.entered.notified().await;
    let retained_deadline = control.cleanup_deadline();
    tokio::time::advance(budget).await;
    drop(release);
    let exit = tokio::select! {
        biased;
        exit = &mut execution => exit,
        _ = init_entered.notified() => {
            control.request_stop(tokio::time::Instant::now() + budget);
            execution.await
        },
    };
    retirement.await.unwrap().result().unwrap();
    system.shutdown().await.unwrap();
    assert!(
        retained_deadline.is_some(),
        "capture disposal cleared the original construction deadline"
    );
    assert_eq!(
        initialized.load(Ordering::SeqCst),
        0,
        "expired capture disposal allowed late initialization"
    );
    assert!(matches!(exit.outcome, Err(ActorError::Timeout)));
    exit.cleanup.unwrap();
}
