#![cfg(feature = "runtime")]

use parrot_wasm::{
    runtime::{
        GuestMessaging, GuestScope, GuestServices, GuestSpan, GuestSpanStatus, GuestSubscription,
        GuestTrace, MessagingError, MessagingRequest, Msg, ServiceEvent, ServiceFuture,
        WasmComponent, WasmRuntime,
    },
    ComponentError, HostCtx, TraceParent, WasmConfig,
};
use std::{
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
    task::{Context, Poll},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum Fault {
    Healthy,
    Construct,
    Poll,
    ReadyDrop,
    PendingDrop,
    PollDrop,
    ReceiverDrop,
    Pending,
    ReadyReceiverDrop,
}
#[derive(Debug, Default)]
struct Probe {
    fault: AtomicUsize,
    created: AtomicUsize,
    polled: AtomicUsize,
    dropped: AtomicUsize,
    receivers: AtomicUsize,
    entered: tokio::sync::Notify,
}
impl Probe {
    fn set(&self, fault: Fault) {
        self.fault.store(fault as usize, SeqCst);
    }
    fn future<T: Send + Unpin + 'static>(
        &self,
        output: T,
    ) -> Pin<Box<impl Future<Output = T> + Send + '_>> {
        self.created.fetch_add(1, SeqCst);
        let fault = self.fault.load(SeqCst);
        assert_ne!(fault, Fault::Construct as usize, "construction failed");
        Box::pin(HostFuture {
            probe: self,
            fault,
            output: Some(output),
        })
    }
}
struct HostFuture<'a, T> {
    probe: &'a Probe,
    fault: usize,
    output: Option<T>,
}
impl<T: Unpin> Future for HostFuture<'_, T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<T> {
        self.probe.polled.fetch_add(1, SeqCst);
        self.probe.entered.notify_one();
        assert!(
            self.fault != Fault::Poll as usize && self.fault != Fault::PollDrop as usize,
            "poll failed"
        );
        if self.fault == Fault::PendingDrop as usize || self.fault == Fault::Pending as usize {
            Poll::Pending
        } else {
            Poll::Ready(self.output.take().expect("polled after completion"))
        }
    }
}
impl<T> Drop for HostFuture<'_, T> {
    fn drop(&mut self) {
        self.probe.dropped.fetch_add(1, SeqCst);
        assert!(
            self.fault != Fault::ReadyDrop as usize
                && self.fault != Fault::PendingDrop as usize
                && self.fault != Fault::PollDrop as usize
                && self.fault != Fault::ReadyReceiverDrop as usize,
            "future disposal failed"
        );
    }
}
impl GuestMessaging for Probe {
    fn ask(
        &self,
        request: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Msg, MessagingError>> + Send + '_>> {
        self.future(Ok(request.message))
    }
    fn tell(
        &self,
        _: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessagingError>> + Send + '_>> {
        self.future(Ok(()))
    }
}
#[derive(Debug)]
struct Receiver {
    probe: Arc<Probe>,
    panic_drop: bool,
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.probe.receivers.fetch_add(1, SeqCst);
        assert!(!self.panic_drop, "receiver disposal failed");
    }
}
impl GuestSubscription for Receiver {
    fn next(&mut self, _: u32) -> ServiceFuture<'_, Option<ServiceEvent>> {
        self.probe.future(Ok(None))
    }
}
#[derive(Debug)]
struct Services(Arc<Probe>);
impl GuestServices for Services {
    fn register(&self, _: String, _: u32) -> ServiceFuture<'_, ()> {
        self.0.future(Ok(()))
    }
    fn deregister(&self, _: String, _: u32) -> ServiceFuture<'_, ()> {
        self.0.future(Ok(()))
    }
    fn subscribe(&self, _: String, _: u32) -> ServiceFuture<'_, Box<dyn GuestSubscription>> {
        self.0.future(Ok(Box::new(Receiver {
            probe: self.0.clone(),
            panic_drop: matches!(self.0.fault.load(SeqCst), value if value == Fault::ReceiverDrop as usize || value == Fault::ReadyReceiverDrop as usize),
        }) as Box<dyn GuestSubscription>))
    }
}

#[derive(Debug, Default)]
struct Recorded {
    next: AtomicUsize,
    statuses: Mutex<Vec<(&'static str, GuestSpanStatus)>>,
    scope_panic: AtomicBool,
    scope_drops: AtomicUsize,
}
#[derive(Debug)]
struct Scope(Arc<Recorded>);
impl GuestScope for Scope {
    fn in_scope(&self, operation: &mut dyn FnMut()) {
        operation();
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        self.0.scope_drops.fetch_add(1, SeqCst);
        assert!(!self.0.scope_panic.load(SeqCst), "scope disposal failed");
    }
}
#[derive(Debug)]
struct Recording {
    observed: Arc<Recorded>,
    id: u64,
    tag: &'static str,
}
impl Recording {
    fn new(observed: &Arc<Recorded>, tag: &'static str) -> Self {
        Self {
            observed: observed.clone(),
            id: observed.next.fetch_add(1, SeqCst) as u64 + 1,
            tag,
        }
    }
}
impl GuestTrace for Recording {
    fn parent(&self) -> TraceParent {
        TraceParent {
            trace_id: [1; 16],
            span_id: self.id.to_le_bytes(),
        }
    }
    fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        Some(Box::new(Self::new(&self.observed, "child")))
    }
    fn start_recovery(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        Some(Box::new(Self::new(&self.observed, "recovery")))
    }
    fn execution_scope(&self) -> Option<Arc<dyn GuestScope>> {
        self.observed
            .scope_panic
            .load(SeqCst)
            .then(|| Arc::new(Scope(self.observed.clone())) as Arc<dyn GuestScope>)
    }
}
impl GuestSpan for Recording {
    fn finish(self: Box<Self>, status: GuestSpanStatus) {
        self.observed
            .statuses
            .lock()
            .unwrap()
            .push((self.tag, status));
    }
}
async fn open(probe: &Arc<Probe>, services: bool, lifecycle: Option<&str>) -> WasmComponent {
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 20_000_000,
        epoch_deadline: 10_000,
        ..Default::default()
    })
    .unwrap();
    let mut host = HostCtx {
        self_path: "/user/guest".into(),
        ..Default::default()
    };
    if let Some(phase) = lifecycle {
        if phase == "tell" {
            host.config_overlay.insert("mode", "tell");
        } else {
            host.config_overlay.insert(format!("{phase}-value"), "3");
        }
    }
    let bytes: &[u8] = if services {
        include_bytes!("fixtures/services-component.wasm")
    } else {
        include_bytes!("fixtures/messaging-component.wasm")
    };
    runtime
        .prepare_bytes(bytes)
        .unwrap()
        .instantiate_with_services(
            host,
            Some(probe.clone()),
            services.then(|| Arc::new(Services(probe.clone())) as Arc<dyn GuestServices>),
        )
        .await
        .unwrap()
}
fn assert_panic<T: std::fmt::Debug>(result: Result<T, ComponentError>, message: &str) {
    assert!(
        matches!(result, Err(ComponentError::Panic(ref text)) if text.contains(message)),
        "{result:?}"
    );
}
async fn denied_after_cleanup(component: &mut WasmComponent, probe: &Probe, message: &str) {
    assert!(!component.is_alive());
    assert_panic::<()>(
        Err(component
            .cleanup_error()
            .expect("lost cancellation receipt")),
        message,
    );
    let created = probe.created.load(SeqCst);
    probe.set(Fault::Healthy);
    for _ in 0..2 {
        assert_panic(
            component.handle_async("echo", b"must not run", None).await,
            message,
        );
        assert_panic(component.on_start_async(None).await, message);
    }
    assert_eq!(probe.created.load(SeqCst), created);
}

async fn scenario(operation: &str, fault: Fault) {
    let probe = Arc::new(Probe::default());
    let service = matches!(operation, "register" | "deregister" | "subscribe" | "next");
    let lifecycle = match operation {
        "start" | "recovery" => Some("start"),
        "drain" => Some("drain"),
        "tell" => Some("tell"),
        _ => None,
    };
    let mut component = open(&probe, service, lifecycle).await;
    let mut payload = Vec::new();
    if operation == "next" {
        payload = component
            .handle_async("subscribe", b"", None)
            .await
            .unwrap();
    }
    if operation == "recovery" {
        component.on_start_async(None).await.unwrap();
        assert!(matches!(
            component.handle_async("trap", b"", None).await,
            Err(ComponentError::Trap(_))
        ));
        // Consume the healthy startup notification before waiting on recovery.
        probe.entered.notified().await;
    }
    let before_created = probe.created.load(SeqCst);
    let before_polled = probe.polled.load(SeqCst);
    let before_dropped = probe.dropped.load(SeqCst);
    probe.set(fault);
    let observed = Arc::new(Recorded::default());
    let recording = Some(Box::new(Recording::new(&observed, "invocation")) as Box<dyn GuestTrace>);
    let call: Pin<Box<dyn Future<Output = Result<(), ComponentError>> + Send + '_>> =
        match operation {
            "start" => Box::pin(component.on_start_async(recording)),
            "drain" => Box::pin(component.on_drain_async(recording)),
            "tell" => Box::pin(component.tell_async("ask", &payload, recording)),
            _ => Box::pin(async {
                component
                    .handle_async(
                        if operation == "recovery" {
                            "echo"
                        } else {
                            operation
                        },
                        &payload,
                        recording,
                    )
                    .await
                    .map(|_| ())
            }),
        };
    if fault == Fault::PendingDrop {
        let mut call = call;
        std::future::poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(probe.polled.load(SeqCst), before_polled + 1);
        assert!(
            catch_unwind(AssertUnwindSafe(|| drop(call))).is_ok(),
            "cancellation leaked cleanup panic"
        );
    } else {
        let result = call.await;
        assert_panic(
            result,
            match fault {
                Fault::Construct => "construction failed",
                Fault::Poll => "poll failed",
                _ => "future disposal failed",
            },
        );
    }
    assert_eq!(probe.created.load(SeqCst), before_created + 1);
    assert_eq!(
        probe.dropped.load(SeqCst),
        before_dropped + usize::from(fault != Fault::Construct)
    );
    let statuses = observed.statuses.lock().unwrap().clone();
    assert!(
        statuses
            .iter()
            .all(|(_, status)| *status == GuestSpanStatus::Panicked),
        "{statuses:?}"
    );
    if operation == "recovery" {
        assert!(
            statuses.contains(&("recovery", GuestSpanStatus::Panicked)),
            "{statuses:?}"
        );
    }
    if matches!(fault, Fault::Construct | Fault::Poll) {
        assert!(component.cleanup_error().is_none());
        assert!(component.is_alive());
        probe.set(Fault::Healthy);
        assert_eq!(
            component
                .handle_async("echo", b"recovered", None)
                .await
                .unwrap(),
            b"recovered"
        );
    } else {
        denied_after_cleanup(&mut component, &probe, "future disposal failed").await;
    }
}

#[tokio::test]
async fn construction_and_poll_failures_remain_recoverable_after_clean_disposal() {
    for operation in [
        "ask",
        "tell",
        "start",
        "drain",
        "recovery",
        "register",
        "deregister",
        "subscribe",
        "next",
    ] {
        for fault in [Fault::Construct, Fault::Poll] {
            scenario(operation, fault).await;
        }
    }
}

#[tokio::test]
async fn cancellation_keeps_disposal_failure_and_panicked_recording() {
    for operation in [
        "ask",
        "tell",
        "start",
        "drain",
        "recovery",
        "register",
        "deregister",
        "subscribe",
        "next",
    ] {
        scenario(operation, Fault::PendingDrop).await;
    }
}

#[tokio::test]
async fn completed_future_disposal_failure_vetoes_reuse() {
    for operation in [
        "ask",
        "tell",
        "start",
        "drain",
        "recovery",
        "register",
        "deregister",
        "subscribe",
        "next",
    ] {
        scenario(operation, Fault::ReadyDrop).await;
    }
}

#[test]
fn poll_and_disposal_panics_are_isolated_in_an_owned_process() {
    run_worker("poll_and_disposal_worker");
}
fn run_worker(name: &str) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--ignored", "--nocapture"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut timed_out = false;
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!timed_out, "worker exceeded deadline: {name}");
    assert!(
        output.status.success(),
        "worker {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"));
}

#[tokio::test]
#[ignore = "owned by the subprocess contract"]
async fn poll_and_disposal_worker() {
    for operation in [
        "ask",
        "tell",
        "start",
        "drain",
        "recovery",
        "register",
        "deregister",
        "subscribe",
        "next",
    ] {
        scenario(operation, Fault::PollDrop).await;
    }
}

#[tokio::test]
async fn receiver_disposal_failure_retires_siblings_and_vetoes_reuse() {
    for operation in ["close", "next", "trap", "cancel"] {
        let probe = Arc::new(Probe::default());
        let mut component = open(&probe, true, None).await;
        probe.set(Fault::ReceiverDrop);
        let handle = component
            .handle_async("subscribe", b"", None)
            .await
            .unwrap();
        component
            .handle_async("subscribe", b"", None)
            .await
            .unwrap();
        probe.set(Fault::Healthy);
        if operation == "cancel" {
            probe.set(Fault::PendingDrop);
            let mut call = Box::pin(component.handle_async("register", b"", None));
            // Earlier subscribe imports notified this probe; verify the new poll directly.
            std::future::poll_fn(|cx| {
                assert!(call.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(catch_unwind(AssertUnwindSafe(|| drop(call))).is_ok());
        } else {
            assert_panic(
                component.handle_async(operation, &handle, None).await,
                "receiver disposal failed",
            );
        }
        assert_eq!(probe.receivers.load(SeqCst), 2);
        denied_after_cleanup(
            &mut component,
            &probe,
            if operation == "cancel" {
                "future disposal failed"
            } else {
                "receiver disposal failed"
            },
        )
        .await;
    }
}

#[tokio::test]
async fn unpublished_subscription_is_retired_even_if_future_and_receiver_disposal_fail() {
    let probe = Arc::new(Probe::default());
    let mut component = open(&probe, true, None).await;
    probe.set(Fault::ReadyReceiverDrop);
    assert_panic(
        component.handle_async("subscribe", b"", None).await,
        "future disposal failed",
    );
    assert_eq!(probe.dropped.load(SeqCst), 1);
    assert_eq!(probe.receivers.load(SeqCst), 1);
    denied_after_cleanup(&mut component, &probe, "future disposal failed").await;
}

#[tokio::test]
async fn cancelled_recovery_scope_disposal_is_panicked_and_retains_its_failure() {
    let probe = Arc::new(Probe::default());
    let mut component = open(&probe, false, Some("start")).await;
    component.on_start_async(None).await.unwrap();
    assert!(component.handle_async("trap", b"", None).await.is_err());
    probe.set(Fault::Pending);
    let observed = Arc::new(Recorded::default());
    observed.scope_panic.store(true, SeqCst);
    let mut call = Box::pin(component.handle_async(
        "echo",
        b"",
        Some(Box::new(Recording::new(&observed, "invocation"))),
    ));
    std::future::poll_fn(|cx| {
        assert!(call.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(probe.polled.load(SeqCst), 2);
    assert!(catch_unwind(AssertUnwindSafe(|| drop(call))).is_ok());
    assert_eq!(probe.dropped.load(SeqCst), 2);
    assert_eq!(observed.scope_drops.load(SeqCst), 1);
    assert_eq!(
        *observed.statuses.lock().unwrap(),
        [
            ("child", GuestSpanStatus::Panicked),
            ("recovery", GuestSpanStatus::Panicked)
        ]
    );
    denied_after_cleanup(&mut component, &probe, "scope disposal failed").await;
}

#[derive(Debug)]
struct Capability(Arc<AtomicUsize>);
impl Drop for Capability {
    fn drop(&mut self) {
        self.0.fetch_add(1, SeqCst);
        panic!("authority disposal failed");
    }
}
impl GuestMessaging for Capability {
    fn ask(
        &self,
        _: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Msg, MessagingError>> + Send + '_>> {
        unreachable!("this capability is never invoked")
    }
    fn tell(
        &self,
        _: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessagingError>> + Send + '_>> {
        unreachable!("this capability is never invoked")
    }
}
impl GuestServices for Capability {
    fn register(&self, _: String, _: u32) -> ServiceFuture<'_, ()> {
        unreachable!()
    }
    fn deregister(&self, _: String, _: u32) -> ServiceFuture<'_, ()> {
        unreachable!()
    }
    fn subscribe(&self, _: String, _: u32) -> ServiceFuture<'_, Box<dyn GuestSubscription>> {
        unreachable!()
    }
}
impl GuestTrace for Capability {
    fn parent(&self) -> TraceParent {
        unreachable!("unpolled recording must not be queried")
    }
    fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        unreachable!()
    }
}

#[test]
fn unpolled_public_async_apis_protect_their_owned_capabilities() {
    run_worker("unpolled_capability_worker");
}

#[tokio::test]
#[ignore = "owned by the subprocess contract"]
async fn unpolled_capability_worker() {
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
    let bytes = include_bytes!("fixtures/messaging-component.wasm");
    for services in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let prepared = runtime.prepare_bytes(bytes).unwrap();
        let messaging = Some(Arc::new(Capability(drops.clone())) as Arc<dyn GuestMessaging>);
        if services {
            let call = prepared.instantiate_with_services(
                HostCtx::default(),
                messaging,
                Some(Arc::new(Capability(drops.clone()))),
            );
            assert!(catch_unwind(AssertUnwindSafe(|| drop(call))).is_ok());
        } else {
            let call = prepared.instantiate_async(HostCtx::default(), messaging);
            assert!(catch_unwind(AssertUnwindSafe(|| drop(call))).is_ok());
        }
        assert_eq!(drops.load(SeqCst), 1 + usize::from(services));
    }
    let probe = Arc::new(Probe::default());
    let mut component = open(&probe, false, None).await;
    for operation in ["ask", "tell", "start", "drain"] {
        let drops = Arc::new(AtomicUsize::new(0));
        let recording = Some(Box::new(Capability(drops.clone())) as Box<dyn GuestTrace>);
        let call: Pin<Box<dyn Future<Output = Result<(), ComponentError>> + Send + '_>> =
            match operation {
                "tell" => Box::pin(component.tell_async("ask", b"", recording)),
                "start" => Box::pin(component.on_start_async(recording)),
                "drain" => Box::pin(component.on_drain_async(recording)),
                _ => {
                    let invocation = component.handle_async("ask", b"", recording);
                    Box::pin(async move { invocation.await.map(|_| ()) })
                }
            };
        assert!(catch_unwind(AssertUnwindSafe(|| drop(call))).is_ok());
        assert_eq!(drops.load(SeqCst), 1, "{operation}");
        assert!(component.cleanup_error().is_none());
        assert!(component.is_alive());
    }
    assert_eq!(probe.created.load(SeqCst), 0);
    assert_eq!(component.take_metrics().messages, 0);
    assert_eq!(
        component.handle_async("count", b"", None).await.unwrap(),
        0u64.to_le_bytes()
    );
    assert_eq!(
        component.handle_async("started", b"", None).await.unwrap(),
        [0]
    );
}

#[test]
fn unpublished_and_final_store_disposal_isolates_each_authority() {
    run_worker("authority_disposal_worker");
}

#[tokio::test]
#[ignore = "owned by the subprocess contract"]
async fn authority_disposal_worker() {
    for valid in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
        let bytes: &[u8] = if valid {
            include_bytes!("fixtures/messaging-component.wasm")
        } else {
            b"(component)"
        };
        let prepared = runtime.prepare_bytes(bytes).unwrap();
        let mut call = Box::pin(prepared.instantiate_with_services(
            HostCtx::default(),
            Some(Arc::new(Capability(drops.clone()))),
            Some(Arc::new(Capability(drops.clone()))),
        ));
        let result = std::future::poll_fn(|cx| {
            let result = catch_unwind(AssertUnwindSafe(|| call.as_mut().poll(cx)));
            match result.expect("initializer cleanup must not unwind") {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => Poll::Ready(result),
            }
        })
        .await;
        if valid {
            assert!(catch_unwind(AssertUnwindSafe(|| drop(result.unwrap()))).is_ok());
        } else {
            assert!(matches!(result, Err(ComponentError::AbiVersion(_))));
        }
        drop(call);
        assert_eq!(drops.load(SeqCst), 2);
    }
}
