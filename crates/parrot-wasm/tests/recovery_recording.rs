#![cfg(feature = "runtime")]

use parrot_wasm::{
    runtime::{
        GuestMessaging, GuestScope, GuestSpan, GuestSpanStatus, GuestTrace, MessagingError,
        MessagingRequest, Msg, WasmComponent, WasmRuntime,
    },
    ComponentError, HostCtx, TraceParent, WasmConfig,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Invocation,
    Recovery,
    BusinessChild,
    RecoveryChild,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Factory(String),
    Start(Kind, String, TraceParent),
    Finish(Kind, GuestSpanStatus),
    Drop(Kind, bool),
}
#[derive(Clone, Copy, Debug, Default)]
enum Fault {
    #[default]
    None,
    Disabled,
    FactoryPanic,
    ParentPanic,
    ZeroTrace,
    ZeroSpan,
    ForeignTrace,
    RootSpan,
    ChildFinishPanic,
    RecoveryFinishPanic,
    ScopeFactoryPanic,
    ScopePanic,
    ScopeOmitted,
    ScopeRepeated,
    ScopeDropPanic,
}
#[derive(Debug, Default)]
struct Plan {
    next: AtomicU64,
    events: Mutex<Vec<Event>>,
    fault: Fault,
    drop_panic: bool,
}
impl Plan {
    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
    fn push(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
    fn owner(self: &Arc<Self>, kind: Kind) -> Owner {
        Owner {
            plan: self.clone(),
            kind,
            finished: false,
            identity: TraceParent {
                trace_id: [1; 16],
                span_id: if kind == Kind::Invocation {
                    2u64
                } else {
                    self.next.fetch_add(1, SeqCst) + 10
                }
                .to_be_bytes(),
            },
        }
    }
    fn root(self: &Arc<Self>) -> Box<dyn GuestTrace> {
        Box::new(self.owner(Kind::Invocation))
    }
}
#[derive(Debug)]
struct Owner {
    plan: Arc<Plan>,
    kind: Kind,
    identity: TraceParent,
    finished: bool,
}
impl GuestTrace for Owner {
    fn parent(&self) -> TraceParent {
        let mut identity = self.identity;
        if self.kind == Kind::Recovery {
            match self.plan.fault {
                Fault::ParentPanic => panic!("recovery parent failed"),
                Fault::ZeroTrace => identity.trace_id = [0; 16],
                Fault::ZeroSpan => identity.span_id = [0; 8],
                Fault::ForeignTrace => identity.trace_id = [9; 16],
                Fault::RootSpan => identity.span_id = 2u64.to_be_bytes(),
                _ => {}
            }
        }
        identity
    }
    fn start(&self, target: &str) -> Option<Box<dyn GuestSpan>> {
        let kind = match self.kind {
            Kind::Recovery | Kind::RecoveryChild => Kind::RecoveryChild,
            _ => Kind::BusinessChild,
        };
        let child = self.plan.owner(kind);
        self.plan
            .push(Event::Start(kind, target.into(), child.identity));
        Some(Box::new(child))
    }
    fn start_recovery(&self, target: &str) -> Option<Box<dyn GuestSpan>> {
        assert_eq!(self.kind, Kind::Invocation);
        self.plan.push(Event::Factory(target.into()));
        match self.plan.fault {
            Fault::Disabled => None,
            Fault::FactoryPanic => panic!("recovery factory failed"),
            _ => Some(Box::new(self.plan.owner(Kind::Recovery))),
        }
    }

    fn execution_scope(&self) -> Option<Arc<dyn GuestScope>> {
        assert_eq!(self.kind, Kind::Recovery);
        match self.plan.fault {
            Fault::ScopeFactoryPanic => panic!("recovery scope factory failed"),
            Fault::ScopePanic
            | Fault::ScopeOmitted
            | Fault::ScopeRepeated
            | Fault::ScopeDropPanic => Some(Arc::new(FaultyScope(self.plan.fault))),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct FaultyScope(Fault);
impl GuestScope for FaultyScope {
    fn in_scope(&self, operation: &mut dyn FnMut()) {
        match self.0 {
            Fault::ScopePanic => panic!("recovery scope failed"),
            Fault::ScopeOmitted => {}
            Fault::ScopeRepeated => {
                operation();
                operation();
            }
            Fault::ScopeDropPanic => operation(),
            _ => unreachable!(),
        }
    }
}
impl Drop for FaultyScope {
    fn drop(&mut self) {
        if matches!(self.0, Fault::ScopeDropPanic) {
            panic!("recovery scope disposal failed");
        }
    }
}
impl GuestSpan for Owner {
    fn finish(mut self: Box<Self>, status: GuestSpanStatus) {
        self.finished = true;
        self.plan.push(Event::Finish(self.kind, status));
        match (self.kind, self.plan.fault) {
            (Kind::RecoveryChild, Fault::ChildFinishPanic) => {
                panic!("recovery child finish failed")
            }
            (Kind::Recovery, Fault::RecoveryFinishPanic) => panic!("recovery finish failed"),
            _ => {}
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.plan.push(Event::Drop(self.kind, self.finished));
        if self.kind == Kind::Recovery && self.plan.drop_panic {
            panic!("recovery disposal failed");
        }
    }
}
#[derive(Debug)]
struct Legacy(Box<dyn GuestTrace>);
impl GuestTrace for Legacy {
    fn parent(&self) -> TraceParent {
        self.0.parent()
    }
    fn start(&self, target: &str) -> Option<Box<dyn GuestSpan>> {
        self.0.start(target)
    }
}

const RECORDING: &[u8] = include_bytes!("fixtures/recording-component.wasm");
fn runtime() -> WasmRuntime {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<WasmComponent>();
    WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 2_000_000,
        epoch_deadline: 10_000,
        ..Default::default()
    })
    .unwrap()
}
async fn guest(asynchronous: bool, mode: &str) -> WasmComponent {
    let runtime = runtime();
    let mut host = HostCtx {
        self_path: "/user/recovery".into(),
        ..Default::default()
    };
    host.config_overlay.insert("recording-on-start-mode", mode);
    let mut guest = if asynchronous {
        runtime
            .prepare_bytes(RECORDING)
            .unwrap()
            .instantiate_async(host, None)
            .await
            .unwrap()
    } else {
        runtime.instantiate_bytes(RECORDING, host).unwrap()
    };
    if asynchronous {
        guest.on_start_async(None).await.unwrap();
    } else {
        guest.on_start().unwrap();
    }
    guest
}
async fn call(
    guest: &mut WasmComponent,
    key: &str,
    payload: &[u8],
    root: Option<Box<dyn GuestTrace>>,
) -> Result<Vec<u8>, ComponentError> {
    if guest.uses_async() {
        guest.handle_async(key, payload, root).await
    } else {
        match root {
            Some(root) => guest.handle_with_recording(key, payload, root),
            None => guest.handle(key, payload),
        }
    }
}
async fn poison(guest: &mut WasmComponent) {
    let plan = Arc::new(Plan::default());
    assert!(matches!(
        call(guest, "trap", b"", Some(Box::new(Legacy(plan.root())))).await,
        Err(ComponentError::Trap(_))
    ));
    guest.take_metrics();
}
fn factories(plan: &Plan) -> usize {
    plan.events()
        .iter()
        .filter(|event| matches!(event, Event::Factory(_)))
        .count()
}
fn finishes(plan: &Plan, kind: Kind) -> Vec<GuestSpanStatus> {
    plan.events()
        .iter()
        .filter_map(|event| match event {
            Event::Finish(k, s) if *k == kind => Some(*s),
            _ => None,
        })
        .collect()
}
fn starts(plan: &Plan, kind: Kind) -> usize {
    plan.events()
        .iter()
        .filter(|event| matches!(event, Event::Start(k,_,_) if *k == kind))
        .count()
}
fn drops(plan: &Plan, kind: Kind) -> Vec<bool> {
    plan.events()
        .iter()
        .filter_map(|event| match event {
            Event::Drop(k, finished) if *k == kind => Some(*finished),
            _ => None,
        })
        .collect()
}

async fn independent_budget(asynchronous: bool) {
    let mut guest = guest(asynchronous, "budget").await;
    poison(&mut guest).await;
    let plan = Arc::new(Plan::default());
    call(&mut guest, "budget", b"", Some(plan.root()))
        .await
        .unwrap();
    let identity = call(&mut guest, "start-identity", b"", None).await.unwrap();
    let first_handle = u64::from_le_bytes(identity[32..].try_into().unwrap());
    assert_eq!(
        first_handle, 2,
        "rebuild reused the handle admitted before the trap"
    );
    let reply = call(
        &mut guest,
        "stale",
        &first_handle.to_le_bytes(),
        Some(plan.root()),
    )
    .await
    .unwrap();
    assert_eq!(
        u64::from_le_bytes(reply.try_into().unwrap()),
        first_handle + 512
    );
    assert_eq!(factories(&plan), 1);
    assert!(plan
        .events()
        .contains(&Event::Factory("wasm/recovery-start:/user/recovery".into())));
    assert_eq!(starts(&plan, Kind::RecoveryChild), 256);
    assert_eq!(starts(&plan, Kind::BusinessChild), 257);
    assert_eq!(finishes(&plan, Kind::Recovery), [GuestSpanStatus::Ok]);
    assert_eq!(
        finishes(&plan, Kind::RecoveryChild),
        vec![GuestSpanStatus::Ok; 256]
    );
    assert_eq!(
        finishes(&plan, Kind::BusinessChild),
        vec![GuestSpanStatus::Ok; 257]
    );
    assert_eq!(drops(&plan, Kind::Recovery), [true]);
    assert_eq!(drops(&plan, Kind::Invocation), [false, false]);
    let events = plan.events();
    let completed = events
        .iter()
        .position(|event| matches!(event, Event::Finish(Kind::Recovery, _)))
        .unwrap();
    let first_business = events
        .iter()
        .position(|event| matches!(event, Event::Start(Kind::BusinessChild, _, _)))
        .unwrap();
    assert!(completed < first_business);
}
#[tokio::test]
async fn sync_recovery_and_business_have_independent_full_budgets_and_stale_handles() {
    independent_budget(false).await;
}
#[tokio::test]
async fn async_recovery_and_business_have_independent_full_budgets_and_stale_handles() {
    independent_budget(true).await;
}

async fn failed_recovery(asynchronous: bool) {
    for (mode, expected) in [
        ("trap", GuestSpanStatus::Error),
        ("fuel", GuestSpanStatus::Cancelled),
    ] {
        let mut guest = guest(asynchronous, mode).await;
        poison(&mut guest).await;
        let plan = Arc::new(Plan::default());
        for _ in 0..2 {
            let error = call(&mut guest, "nested", b"", Some(plan.root()))
                .await
                .unwrap_err();
            if mode == "fuel" {
                assert_eq!(error, ComponentError::OutOfFuel);
            } else {
                assert!(matches!(error, ComponentError::Trap(_)));
            }
        }
        assert_eq!(factories(&plan), 2, "a failed recovery Store was reused");
        assert_eq!(finishes(&plan, Kind::Recovery), [expected; 2]);
        assert_eq!(finishes(&plan, Kind::RecoveryChild), [expected; 2]);
        assert_eq!(starts(&plan, Kind::BusinessChild), 0);
        assert_eq!(drops(&plan, Kind::Recovery), [true; 2]);
        assert_eq!(drops(&plan, Kind::Invocation), [false; 2]);
        assert_eq!(guest.take_metrics().messages, 0);
        assert!(call(&mut guest, "start-identity", b"", None)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            call(&mut guest, "saved", b"", None).await.unwrap(),
            0u64.to_le_bytes()
        );
    }
}
#[tokio::test]
async fn sync_failed_recovery_never_calls_business_and_rebuilds_again() {
    failed_recovery(false).await;
}
#[tokio::test]
async fn async_failed_recovery_never_calls_business_and_rebuilds_again() {
    failed_recovery(true).await;
}

async fn invalid_factory(asynchronous: bool) {
    for fault in [
        Fault::FactoryPanic,
        Fault::ParentPanic,
        Fault::ZeroTrace,
        Fault::ZeroSpan,
        Fault::ForeignTrace,
        Fault::RootSpan,
    ] {
        let mut guest = guest(asynchronous, "").await;
        poison(&mut guest).await;
        let plan = Arc::new(Plan {
            fault,
            drop_panic: matches!(fault, Fault::ParentPanic | Fault::ZeroTrace),
            ..Default::default()
        });
        let error = call(&mut guest, "nested", b"", Some(plan.root()))
            .await
            .unwrap_err();
        match fault {
            Fault::FactoryPanic => assert_eq!(
                error,
                ComponentError::Panic("recovery factory failed".into())
            ),
            Fault::ParentPanic => assert_eq!(
                error,
                ComponentError::Panic("recovery parent failed".into())
            ),
            _ => assert!(
                matches!(error, ComponentError::Trap(ref message) if message.contains("invalid host recovery recording identity")),
                "{error:?}"
            ),
        }
        assert_eq!(factories(&plan), 1);
        assert_eq!(starts(&plan, Kind::BusinessChild), 0);
        assert_eq!(starts(&plan, Kind::RecoveryChild), 0);
        assert!(finishes(&plan, Kind::Recovery).is_empty());
        assert_eq!(
            drops(&plan, Kind::Recovery).len(),
            usize::from(!matches!(fault, Fault::FactoryPanic))
        );
        assert_eq!(drops(&plan, Kind::Invocation), [false]);
        let retry = Arc::new(Plan::default());
        call(&mut guest, "nested", b"", Some(retry.root()))
            .await
            .unwrap();
        assert_eq!(
            factories(&retry),
            1,
            "factory failure left an initialized Store reusable"
        );
        assert_eq!(finishes(&retry, Kind::Recovery), [GuestSpanStatus::Ok]);
    }
}
#[tokio::test]
async fn sync_factory_invalid_identity_parent_and_drop_failures_are_contained() {
    invalid_factory(false).await;
}
#[tokio::test]
async fn async_factory_invalid_identity_parent_and_drop_failures_are_contained() {
    invalid_factory(true).await;
}

async fn scope_failure(asynchronous: bool) {
    for (fault, message, panicked) in [
        (
            Fault::ScopeFactoryPanic,
            "recovery scope factory failed",
            true,
        ),
        (Fault::ScopePanic, "recovery scope failed", true),
        (
            Fault::ScopeOmitted,
            "host execution scope omitted callback",
            false,
        ),
        (
            Fault::ScopeRepeated,
            "host execution scope repeated callback",
            false,
        ),
        (
            Fault::ScopeDropPanic,
            "recovery scope disposal failed",
            true,
        ),
    ] {
        let mut guest = guest(asynchronous, "unfinished").await;
        poison(&mut guest).await;
        let plan = Arc::new(Plan {
            fault,
            ..Default::default()
        });
        let error = call(&mut guest, "nested", b"", Some(plan.root()))
            .await
            .unwrap_err();
        if panicked {
            assert_eq!(error, ComponentError::Panic(message.into()));
        } else {
            assert!(
                matches!(error, ComponentError::Trap(ref reason) if reason.contains(message)),
                "{error:?}"
            );
        }
        assert_eq!(factories(&plan), 1);
        assert_eq!(starts(&plan, Kind::BusinessChild), 0);
        assert_eq!(
            starts(&plan, Kind::RecoveryChild),
            usize::from(matches!(
                fault,
                Fault::ScopeRepeated | Fault::ScopeDropPanic
            ))
        );
        assert_eq!(
            finishes(&plan, Kind::Recovery),
            [if panicked {
                GuestSpanStatus::Panicked
            } else {
                GuestSpanStatus::Error
            }]
        );
        assert_eq!(drops(&plan, Kind::Recovery), [true]);
        assert_eq!(drops(&plan, Kind::Invocation), [false]);
        if (asynchronous && !matches!(fault, Fault::ScopeFactoryPanic))
            || matches!(fault, Fault::ScopeDropPanic)
        {
            let retry = Arc::new(Plan::default());
            let error = call(&mut guest, "nested", b"", Some(retry.root()))
                .await
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error:?}");
            assert_eq!(
                factories(&retry),
                0,
                "failed physical cleanup must veto reuse"
            );
            continue;
        }
        let retry = Arc::new(Plan::default());
        call(&mut guest, "nested", b"", Some(retry.root()))
            .await
            .unwrap();
        assert_eq!(
            factories(&retry),
            1,
            "invalid scope must poison the recovered Store"
        );
    }
}

#[tokio::test]
async fn sync_recovery_scope_faults_retire_owners_and_prevent_business_execution() {
    scope_failure(false).await;
}

#[tokio::test]
async fn async_recovery_scope_faults_retire_owners_and_prevent_business_execution() {
    scope_failure(true).await;
}

async fn retirement_failure(asynchronous: bool) {
    for (fault, message) in [
        (Fault::ChildFinishPanic, "recovery child finish failed"),
        (Fault::RecoveryFinishPanic, "recovery finish failed"),
    ] {
        let mut guest = guest(asynchronous, "unfinished").await;
        poison(&mut guest).await;
        let plan = Arc::new(Plan {
            fault,
            ..Default::default()
        });
        assert_eq!(
            call(&mut guest, "nested", b"", Some(plan.root()))
                .await
                .unwrap_err(),
            ComponentError::Panic(message.into())
        );
        assert_eq!(
            finishes(&plan, Kind::RecoveryChild),
            [GuestSpanStatus::Cancelled]
        );
        assert_eq!(
            finishes(&plan, Kind::Recovery),
            [if matches!(fault, Fault::ChildFinishPanic) {
                GuestSpanStatus::Panicked
            } else {
                GuestSpanStatus::Ok
            }]
        );
        assert_eq!(drops(&plan, Kind::Recovery), [true]);
        assert_eq!(drops(&plan, Kind::RecoveryChild), [true]);
        assert_eq!(starts(&plan, Kind::BusinessChild), 0);
        let retry = Arc::new(Plan::default());
        assert_eq!(
            guest.cleanup_error(),
            Some(ComponentError::Panic(message.into()))
        );
        assert!(!guest.is_alive());
        assert_eq!(
            call(&mut guest, "nested", b"", Some(retry.root())).await,
            Err(ComponentError::Panic(message.into()))
        );
        assert_eq!(factories(&retry), 0);
    }
}
#[tokio::test]
async fn sync_recovery_retirement_reports_panics_and_aggregate_phase_status() {
    retirement_failure(false).await;
}
#[tokio::test]
async fn async_recovery_retirement_reports_panics_and_aggregate_phase_status() {
    retirement_failure(true).await;
}

async fn optional_capability(asynchronous: bool) {
    for legacy in [false, true] {
        let mut guest = guest(asynchronous, "").await;
        poison(&mut guest).await;
        let plan = Arc::new(Plan {
            fault: Fault::Disabled,
            ..Default::default()
        });
        let capability = if legacy {
            Box::new(Legacy(plan.root())) as Box<dyn GuestTrace>
        } else {
            plan.root()
        };
        call(&mut guest, "nested", b"", Some(capability))
            .await
            .unwrap();
        assert_eq!(factories(&plan), usize::from(!legacy));
        assert!(finishes(&plan, Kind::Recovery).is_empty());
        assert!(call(&mut guest, "start-identity", b"", None)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(starts(&plan, Kind::BusinessChild), 2);
    }
}
#[tokio::test]
async fn sync_legacy_and_explicit_disabled_capabilities_keep_recovery_unrecorded() {
    optional_capability(false).await;
}
#[tokio::test]
async fn async_legacy_and_explicit_disabled_capabilities_keep_recovery_unrecorded() {
    optional_capability(true).await;
}

async fn no_fabricated_recovery(asynchronous: bool) {
    let runtime = runtime();
    let host = HostCtx {
        self_path: "/user/recovery".into(),
        ..Default::default()
    };
    let mut guest = if asynchronous {
        runtime
            .prepare_bytes(RECORDING)
            .unwrap()
            .instantiate_async(host, None)
            .await
            .unwrap()
    } else {
        runtime.instantiate_bytes(RECORDING, host).unwrap()
    };
    poison(&mut guest).await;
    let plan = Arc::new(Plan::default());
    call(&mut guest, "nested", b"", Some(plan.root()))
        .await
        .unwrap();
    assert_eq!(
        factories(&plan),
        0,
        "a never-started guest acquired a recovery lifecycle"
    );
    assert!(call(&mut guest, "start-identity", b"", None)
        .await
        .unwrap()
        .is_empty());
    poison(&mut guest).await;
    if asynchronous {
        guest.on_start_async(Some(plan.root())).await.unwrap();
    } else {
        guest.on_start_with_recording(plan.root()).unwrap();
    }
    assert_eq!(
        factories(&plan),
        0,
        "explicit startup invoked a second recovery startup"
    );
    let identity = call(&mut guest, "start-identity", b"", None).await.unwrap();
    assert_eq!(
        &identity[..24],
        [vec![1; 16], 2u64.to_be_bytes().to_vec()].concat()
    );
    call(&mut guest, "nested", b"", Some(plan.root()))
        .await
        .unwrap();
    assert_eq!(
        factories(&plan),
        0,
        "a healthy Store fabricated a recovery hook"
    );
}
#[tokio::test]
async fn sync_healthy_never_started_and_explicit_start_do_not_fabricate_recovery() {
    no_fabricated_recovery(false).await;
}
#[tokio::test]
async fn async_healthy_never_started_and_explicit_start_do_not_fabricate_recovery() {
    no_fabricated_recovery(true).await;
}

async fn tell_and_drain(asynchronous: bool) {
    for drain in [false, true] {
        let mut guest = guest(asynchronous, "unfinished").await;
        poison(&mut guest).await;
        let plan = Arc::new(Plan::default());
        if drain {
            if asynchronous {
                guest.on_drain_async(Some(plan.root())).await.unwrap();
            } else {
                guest.on_drain_with_recording(plan.root()).unwrap();
            }
        } else if asynchronous {
            guest
                .tell_async("unfinished", b"", Some(plan.root()))
                .await
                .unwrap();
        } else {
            guest
                .tell_with_recording("unfinished", b"", plan.root())
                .unwrap();
        }
        assert_eq!(factories(&plan), 1);
        assert_eq!(finishes(&plan, Kind::Recovery), [GuestSpanStatus::Ok]);
        assert_eq!(
            finishes(&plan, Kind::RecoveryChild),
            [GuestSpanStatus::Cancelled]
        );
        assert_eq!(
            finishes(&plan, Kind::BusinessChild),
            [if drain {
                GuestSpanStatus::Ok
            } else {
                GuestSpanStatus::Cancelled
            }]
        );
        let identity = call(&mut guest, "start-identity", b"", None).await.unwrap();
        assert_eq!(
            &identity[..24],
            [vec![1; 16], 10u64.to_be_bytes().to_vec()].concat()
        );
        if drain {
            let identity = call(&mut guest, "drain-identity", b"", None).await.unwrap();
            assert_eq!(
                &identity[..24],
                [vec![1; 16], 2u64.to_be_bytes().to_vec()].concat()
            );
        }
        assert_eq!(drops(&plan, Kind::Recovery), [true]);
    }
}
#[tokio::test]
async fn sync_tell_and_drain_keep_recovery_and_requested_phase_separate() {
    tell_and_drain(false).await;
}
#[tokio::test]
async fn async_tell_and_drain_keep_recovery_and_requested_phase_separate() {
    tell_and_drain(true).await;
}

#[derive(Debug)]
struct Bridge {
    entered: tokio::sync::Notify,
    permits: tokio::sync::Semaphore,
    retired: AtomicUsize,
}
struct Wait<'a>(&'a AtomicUsize);
impl Drop for Wait<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, SeqCst);
    }
}
impl GuestMessaging for Bridge {
    fn ask(
        &self,
        request: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Msg, MessagingError>> + Send + '_>> {
        Box::pin(async move {
            let _wait = Wait(&self.retired);
            self.entered.notify_one();
            self.permits.acquire().await.unwrap().forget();
            Ok(request.message)
        })
    }
    fn tell(
        &self,
        request: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessagingError>> + Send + '_>> {
        Box::pin(async move { self.ask(request).await.map(|_| ()) })
    }
}
#[tokio::test]
async fn cancelled_recovery_retires_actual_import_child_and_phase_before_next_rebuild() {
    let bridge = Arc::new(Bridge {
        entered: Default::default(),
        permits: tokio::sync::Semaphore::new(1),
        retired: AtomicUsize::new(0),
    });
    let mut host = HostCtx {
        self_path: "/user/recovery".into(),
        ..Default::default()
    };
    host.config_overlay.insert("start-value", "3");
    let runtime = runtime();
    let mut guest = runtime
        .prepare_bytes(include_bytes!("fixtures/messaging-component.wasm"))
        .unwrap()
        .instantiate_async(host, Some(bridge.clone()))
        .await
        .unwrap();
    guest.on_start_async(None).await.unwrap();
    bridge.entered.notified().await;
    assert!(matches!(
        guest.handle_async("trap", b"", None).await,
        Err(ComponentError::Trap(_))
    ));
    let plan = Arc::new(Plan::default());
    let mut call = Box::pin(guest.handle_async("echo", b"business", Some(plan.root())));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            result = &mut call => panic!("recovery did not suspend: {result:?}"),
            () = bridge.entered.notified() => {}
        }
    })
    .await
    .unwrap();
    assert!(finishes(&plan, Kind::Recovery).is_empty());
    assert!(drops(&plan, Kind::Invocation).is_empty());
    assert_eq!(bridge.retired.load(SeqCst), 1);
    drop(call);
    assert_eq!(bridge.retired.load(SeqCst), 2);
    assert_eq!(
        finishes(&plan, Kind::RecoveryChild),
        [GuestSpanStatus::Cancelled]
    );
    assert_eq!(
        finishes(&plan, Kind::Recovery),
        [GuestSpanStatus::Cancelled]
    );
    assert_eq!(drops(&plan, Kind::Recovery), [true]);
    assert_eq!(drops(&plan, Kind::Invocation), [false]);
    assert_eq!(starts(&plan, Kind::BusinessChild), 0);
    bridge.permits.add_permits(1);
    assert_eq!(
        guest
            .handle_async("echo", b"business", Some(plan.root()))
            .await
            .unwrap(),
        b"business"
    );
    assert_eq!(factories(&plan), 2);
    assert_eq!(
        finishes(&plan, Kind::Recovery),
        [GuestSpanStatus::Cancelled, GuestSpanStatus::Ok]
    );
    assert_eq!(bridge.retired.load(SeqCst), 3);
    assert!(guest
        .handle_async("parent", b"", None)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        guest.handle_async("count", b"", None).await.unwrap(),
        0u64.to_le_bytes()
    );
}
