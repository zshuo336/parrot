#![cfg(feature = "runtime")]

use parrot_wasm::{
    runtime::{
        GuestMessaging, GuestSpan, GuestSpanStatus, GuestTrace, MessagingError, MessagingRequest,
        Msg, WasmComponent, WasmRuntime,
    },
    ComponentError, HostCtx, TraceParent, WasmConfig,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
};

#[derive(Debug)]
struct Bridge {
    entered: tokio::sync::Notify,
    permits: tokio::sync::Semaphore,
    waits: AtomicUsize,
    retired: AtomicUsize,
    panic: AtomicBool,
}
impl Default for Bridge {
    fn default() -> Self {
        Self {
            entered: Default::default(),
            permits: tokio::sync::Semaphore::new(0),
            waits: AtomicUsize::new(0),
            retired: AtomicUsize::new(0),
            panic: AtomicBool::new(false),
        }
    }
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
            self.waits.fetch_add(1, SeqCst);
            let _wait = Wait(&self.retired);
            assert!(!self.panic.load(SeqCst), "host messaging panic");
            self.entered.notify_one();
            self.permits.acquire().await.unwrap().forget();
            Ok(Msg {
                type_key: "bin:runtime.Reply#v1".into(),
                payload: request.message.payload,
            })
        })
    }
    fn tell(
        &self,
        request: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessagingError>> + Send + '_>> {
        Box::pin(async move { self.ask(request).await.map(|_| ()) })
    }
}

#[derive(Debug, Default)]
struct Observed {
    roots: AtomicUsize,
    children: AtomicUsize,
    statuses: Mutex<Vec<GuestSpanStatus>>,
}
#[derive(Debug)]
struct Root(Arc<Observed>);
#[derive(Debug)]
struct Span {
    observed: Arc<Observed>,
    span_id: [u8; 8],
}
fn child(observed: &Arc<Observed>) -> Box<dyn GuestSpan> {
    let id = observed.children.fetch_add(1, SeqCst) as u64 + 3;
    Box::new(Span {
        observed: observed.clone(),
        span_id: id.to_le_bytes(),
    })
}
fn identity() -> TraceParent {
    TraceParent {
        trace_id: [1; 16],
        span_id: [2; 8],
    }
}
impl GuestTrace for Root {
    fn parent(&self) -> TraceParent {
        identity()
    }
    fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        Some(child(&self.0))
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        self.0.roots.fetch_add(1, SeqCst);
    }
}
impl GuestTrace for Span {
    fn parent(&self) -> TraceParent {
        TraceParent {
            span_id: self.span_id,
            ..identity()
        }
    }
    fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        Some(child(&self.observed))
    }
}
impl GuestSpan for Span {
    fn finish(self: Box<Self>, status: GuestSpanStatus) {
        self.observed.statuses.lock().unwrap().push(status);
    }
}
async fn component(bridge: Option<Arc<Bridge>>) -> WasmComponent {
    component_with(
        bridge,
        HostCtx {
            self_path: "/user/guest".into(),
            ..Default::default()
        },
    )
    .await
}
async fn component_with(bridge: Option<Arc<Bridge>>, host: HostCtx) -> WasmComponent {
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 20_000_000,
        epoch_deadline: 10_000,
        ..Default::default()
    })
    .unwrap();
    runtime
        .prepare_bytes(include_bytes!("fixtures/messaging-component.wasm"))
        .unwrap()
        .instantiate_async(host, bridge.map(|bridge| bridge as Arc<dyn GuestMessaging>))
        .await
        .unwrap()
}

#[tokio::test]
async fn cancellation_retires_wait_and_recording_before_rebuild_and_restores_start() {
    let bridge = Arc::new(Bridge::default());
    let mut component = component(Some(bridge.clone())).await;
    component.on_start_async(None).await.unwrap();
    let observed = Arc::new(Observed::default());
    let mut call =
        Box::pin(component.handle_async("ask", b"payload", Some(Box::new(Root(observed.clone())))));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            result = &mut call => panic!("call completed before release: {result:?}"),
            () = bridge.entered.notified() => {}
        }
    })
    .await
    .expect("host import never entered");
    assert_eq!(bridge.waits.load(SeqCst), 1);
    assert_eq!(bridge.retired.load(SeqCst), 0);
    drop(call);
    assert_eq!(bridge.retired.load(SeqCst), 1);
    assert_eq!(observed.roots.load(SeqCst), 1);
    assert_eq!(
        *observed.statuses.lock().unwrap(),
        [GuestSpanStatus::Cancelled]
    );
    assert_eq!(
        component.handle_async("count", b"", None).await.unwrap(),
        0u64.to_le_bytes()
    );
    assert_eq!(
        component.handle_async("started", b"", None).await.unwrap(),
        [1]
    );
    assert!(component
        .handle_async("parent", b"", None)
        .await
        .unwrap()
        .is_empty());
    bridge.permits.add_permits(1);
    assert_eq!(
        component.handle_async("ask", b"next", None).await.unwrap(),
        b"bin:runtime.Reply#v1\0next"
    );
    assert_eq!(bridge.waits.load(SeqCst), 2);
    assert_eq!(bridge.retired.load(SeqCst), 2);
}

#[tokio::test]
async fn host_panic_is_classified_and_poisoned_store_is_not_reentered() {
    let bridge = Arc::new(Bridge::default());
    bridge.panic.store(true, SeqCst);
    let mut component = component(Some(bridge.clone())).await;
    component.on_start_async(None).await.unwrap();
    let observed = Arc::new(Observed::default());
    let error = component
        .handle_async("ask", b"", Some(Box::new(Root(observed.clone()))))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ComponentError::Panic(ref message) if message.contains("host messaging panic")),
        "{error}"
    );
    assert_eq!(bridge.retired.load(SeqCst), 1);
    assert_eq!(
        *observed.statuses.lock().unwrap(),
        [GuestSpanStatus::Panicked]
    );
    assert_eq!(observed.roots.load(SeqCst), 1);
    bridge.panic.store(false, SeqCst);
    assert_eq!(
        component.handle_async("count", b"", None).await.unwrap(),
        0u64.to_le_bytes()
    );
    assert_eq!(
        component.handle_async("started", b"", None).await.unwrap(),
        [1]
    );
}

#[tokio::test]
async fn optional_import_without_authority_is_disabled_and_sync_call_is_rejected() {
    let mut component = component(None).await;
    assert!(matches!(
        component.handle("ask", b""),
        Err(ComponentError::InvalidConfig(_))
    ));
    assert_eq!(
        component.handle_async("ask", b"", None).await.unwrap(),
        b"error:Disabled"
    );
    assert_eq!(
        component.handle_async("count", b"", None).await.unwrap(),
        1u64.to_le_bytes()
    );
    assert_eq!(component.take_metrics().messages, 2);
    assert_eq!(
        component
            .handle_async("disabled-quota", b"", None)
            .await
            .unwrap(),
        b"error:Limit"
    );
}

#[tokio::test]
async fn exactly_one_mebibyte_payload_is_allowed_in_both_directions() {
    let bridge = Arc::new(Bridge::default());
    bridge.permits.add_permits(1);
    let mut component = component(Some(bridge.clone())).await;
    let payload = vec![7; 1024 * 1024];
    let result = component.handle_async("ask", &payload, None).await.unwrap();
    let split = result.iter().position(|&byte| byte == 0).unwrap();
    assert_eq!(&result[..split], b"bin:runtime.Reply#v1");
    assert_eq!(&result[split + 1..], &payload);
    assert_eq!(bridge.waits.load(SeqCst), 1);
    assert_eq!(bridge.retired.load(SeqCst), 1);
}

#[tokio::test]
async fn dropping_unpolled_call_does_not_enter_guest_or_acquire_authority() {
    let bridge = Arc::new(Bridge::default());
    let mut component = component(Some(bridge.clone())).await;
    drop(component.handle_async("ask", b"", None));
    assert_eq!(bridge.waits.load(SeqCst), 0);
    assert_eq!(
        component.handle_async("count", b"", None).await.unwrap(),
        0u64.to_le_bytes()
    );
}

#[tokio::test]
async fn async_trace_and_recording_are_scoped_to_successful_call() {
    let bridge = Arc::new(Bridge::default());
    bridge.permits.add_permits(1);
    let mut component = component(Some(bridge)).await;
    let observed = Arc::new(Observed::default());
    assert_eq!(
        component
            .handle_async("ask", b"ok", Some(Box::new(Root(observed.clone()))))
            .await
            .unwrap(),
        b"bin:runtime.Reply#v1\0ok"
    );
    assert_eq!(observed.roots.load(SeqCst), 1);
    assert_eq!(
        *observed.statuses.lock().unwrap(),
        [GuestSpanStatus::Cancelled]
    );
    assert!(component
        .handle_async("parent", b"", None)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn frozen_guest_world_and_sync_runtime_remain_usable() {
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
    let bytes = include_bytes!("fixtures/fixture-component.wasm");
    let mut sync = runtime
        .instantiate_bytes(bytes, HostCtx::default())
        .unwrap();
    assert_eq!(sync.handle("echo", b"sync").unwrap(), b"sync");
    let mut asynchronous = runtime
        .prepare_bytes(bytes)
        .unwrap()
        .instantiate_async(HostCtx::default(), None)
        .await
        .unwrap();
    asynchronous.on_start_async(None).await.unwrap();
    assert_eq!(
        asynchronous
            .handle_async("started", b"", None)
            .await
            .unwrap(),
        [1]
    );
    asynchronous.tell_async("bump", &[7], None).await.unwrap();
    assert_eq!(
        asynchronous
            .handle_async("counter", b"", None)
            .await
            .unwrap(),
        7u64.to_le_bytes()
    );
    asynchronous.on_drain_async(None).await.unwrap();
    assert_eq!(
        asynchronous
            .handle_async("drained", b"", None)
            .await
            .unwrap(),
        [1]
    );
}

#[tokio::test]
async fn cancelling_tell_and_lifecycle_calls_retires_their_actual_imports() {
    for phase in ["tell", "start", "drain"] {
        let bridge = Arc::new(Bridge::default());
        let mut host = HostCtx {
            self_path: "/user/guest".into(),
            ..Default::default()
        };
        if phase != "tell" {
            host.config_overlay.insert(format!("{phase}-value"), "3");
        }
        let mut component = component_with(Some(bridge.clone()), host).await;
        if phase != "start" {
            component.on_start_async(None).await.unwrap();
        }
        let observed = Arc::new(Observed::default());
        let recording = Some(Box::new(Root(observed.clone())) as Box<dyn GuestTrace>);
        let mut call: Pin<Box<dyn Future<Output = Result<(), ComponentError>> + Send + '_>> =
            match phase {
                "start" => Box::pin(component.on_start_async(recording)),
                "drain" => Box::pin(component.on_drain_async(recording)),
                _ => Box::pin(component.tell_async("ask", b"", recording)),
            };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::select! {
                result = &mut call => panic!("{phase} unexpectedly completed: {result:?}"),
                () = bridge.entered.notified() => {}
            }
        })
        .await
        .unwrap();
        drop(call);
        assert_eq!(bridge.retired.load(SeqCst), 1, "{phase}");
        assert_eq!(observed.roots.load(SeqCst), 1, "{phase}");
        assert_eq!(
            *observed.statuses.lock().unwrap(),
            [GuestSpanStatus::Cancelled],
            "{phase}"
        );
        assert_eq!(
            component.handle_async("count", b"", None).await.unwrap(),
            0u64.to_le_bytes()
        );
        assert_eq!(
            component.handle_async("started", b"", None).await.unwrap(),
            [u8::from(phase != "start")]
        );
        assert_eq!(
            component.handle_async("drained", b"", None).await.unwrap(),
            [0]
        );
    }
}

#[tokio::test]
async fn cancellation_during_recovery_start_keeps_store_poisoned_and_does_not_inherit_recording() {
    let bridge = Arc::new(Bridge::default());
    bridge.permits.add_permits(1);
    let mut host = HostCtx {
        self_path: "/user/guest".into(),
        ..Default::default()
    };
    host.config_overlay.insert("start-value", "3");
    let mut component = component_with(Some(bridge.clone()), host).await;
    component.on_start_async(None).await.unwrap();
    bridge.entered.notified().await;
    assert!(matches!(
        component.handle_async("trap", b"", None).await,
        Err(ComponentError::Trap(_))
    ));
    let observed = Arc::new(Observed::default());
    let mut call =
        Box::pin(component.handle_async("echo", b"", Some(Box::new(Root(observed.clone())))));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            result = &mut call => panic!("recovery did not suspend: {result:?}"),
            () = bridge.entered.notified() => {}
        }
    })
    .await
    .unwrap();
    drop(call);
    assert_eq!(bridge.waits.load(SeqCst), 2);
    assert_eq!(bridge.retired.load(SeqCst), 2);
    assert_eq!(observed.roots.load(SeqCst), 1);
    assert!(
        observed.statuses.lock().unwrap().is_empty(),
        "recovery startup acquired the interrupted call's recording"
    );
    bridge.permits.add_permits(1);
    assert_eq!(
        component.handle_async("started", b"", None).await.unwrap(),
        [1]
    );
    assert_eq!(bridge.waits.load(SeqCst), 3);
    assert_eq!(bridge.retired.load(SeqCst), 3);
    assert!(component
        .handle_async("parent", b"", None)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn async_guest_fuel_and_epoch_failures_preserve_resource_classification() {
    for epoch in [false, true] {
        let runtime = Arc::new(
            WasmRuntime::new_with_manual_epoch(WasmConfig {
                fuel_per_message: 20_000_000,
                epoch_deadline: 1,
                ..Default::default()
            })
            .unwrap(),
        );
        let bridge = Arc::new(Bridge::default());
        let mut component = runtime
            .prepare_bytes(include_bytes!("fixtures/messaging-component.wasm"))
            .unwrap()
            .instantiate_async(HostCtx::default(), Some(bridge.clone()))
            .await
            .unwrap();
        component.on_start_async(None).await.unwrap();
        // Consume almost all fuel via a fresh store budget in the fuel case.
        // The base fixture provides a real infinite guest loop.
        if !epoch {
            let mut guest = runtime
                .prepare_bytes(include_bytes!("fixtures/fixture-component.wasm"))
                .unwrap()
                .instantiate_async(HostCtx::default(), None)
                .await
                .unwrap();
            assert_eq!(
                guest.handle_async("spin", b"", None).await.unwrap_err(),
                ComponentError::OutOfFuel
            );
            assert_eq!(
                guest.handle_async("echo", b"ok", None).await.unwrap(),
                b"ok"
            );
            assert_eq!(guest.take_metrics().out_of_fuel_count, 1);
            continue;
        }
        let mut call = Box::pin(component.handle_async("ask", b"", None));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::select! {
                result = &mut call => panic!("epoch probe did not suspend: {result:?}"),
                () = bridge.entered.notified() => {}
            }
        })
        .await
        .unwrap();
        runtime.tick_epoch();
        bridge.permits.add_permits(1);
        assert_eq!(call.await.unwrap_err(), ComponentError::EpochDeadline);
        assert_eq!(
            component.handle_async("started", b"", None).await.unwrap(),
            [1]
        );
    }
}
