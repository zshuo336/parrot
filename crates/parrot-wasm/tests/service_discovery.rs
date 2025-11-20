#![cfg(feature = "runtime")]

use parrot_wasm::runtime::{
    GuestServices, GuestSubscription, ServiceError, ServiceEvent, ServiceFuture, WasmComponent,
};
use parrot_wasm::{runtime::WasmRuntime, HostCtx, WasmConfig};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[tokio::test]
async fn optional_service_world_links_and_denies_without_authority() {
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
        epoch_deadline: 1000,
        ..Default::default()
    })
    .unwrap();
    let prepared = runtime
        .prepare_bytes(include_bytes!("fixtures/services-component.wasm"))
        .unwrap();
    let mut component = prepared
        .instantiate_async(HostCtx::default(), None)
        .await
        .unwrap();
    component.on_start_async(None).await.unwrap();
    assert_eq!(
        component
            .handle_async("register", b"edge/guest-service", None)
            .await
            .unwrap(),
        b"error:Disabled"
    );
    assert_eq!(
        component
            .handle_async("subscribe", b"edge/guest-service", None)
            .await
            .unwrap(),
        b"error:Disabled"
    );
}

#[derive(Debug, Default)]
struct Provider {
    calls: AtomicUsize,
    drops: Arc<AtomicUsize>,
    panic_drop: AtomicBool,
    hold_subscribe: AtomicBool,
    entered: tokio::sync::Notify,
    waiting: Arc<tokio::sync::Notify>,
    senders: Mutex<Vec<tokio::sync::mpsc::Sender<ServiceEvent>>>,
}
#[derive(Debug)]
struct Queue {
    receiver: tokio::sync::mpsc::Receiver<ServiceEvent>,
    drops: Arc<AtomicUsize>,
    panic_drop: bool,
    waiting: Arc<tokio::sync::Notify>,
}
impl Drop for Queue {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "host subscription destructor panic");
    }
}
impl GuestSubscription for Queue {
    fn next(&mut self, timeout_ms: u32) -> ServiceFuture<'_, Option<ServiceEvent>> {
        Box::pin(async move {
            self.waiting.notify_one();
            tokio::time::timeout(
                Duration::from_millis(timeout_ms.into()),
                self.receiver.recv(),
            )
            .await
            .map_err(|_| ServiceError::Timeout)
        })
    }
}
impl GuestServices for Provider {
    fn register(&self, key: String, _: u32) -> ServiceFuture<'_, ()> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if key == "edge/forbidden" {
                Err(ServiceError::Denied)
            } else {
                Ok(())
            }
        })
    }
    fn deregister(&self, key: String, wait: u32) -> ServiceFuture<'_, ()> {
        self.register(key, wait)
    }
    fn subscribe(&self, _: String, _: u32) -> ServiceFuture<'_, Box<dyn GuestSubscription>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (sender, receiver) = tokio::sync::mpsc::channel(2);
            let stream = Queue {
                receiver,
                drops: self.drops.clone(),
                panic_drop: self.panic_drop.load(Ordering::SeqCst),
                waiting: self.waiting.clone(),
            };
            self.senders.lock().unwrap().push(sender);
            self.entered.notify_one();
            if self.hold_subscribe.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            Ok(Box::new(stream) as Box<dyn GuestSubscription>)
        })
    }
}
async fn open(provider: Arc<Provider>) -> WasmComponent {
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 20_000_000,
        epoch_deadline: 10_000,
        ..Default::default()
    })
    .unwrap();
    let mut component = runtime
        .prepare_bytes(include_bytes!("fixtures/services-component.wasm"))
        .unwrap()
        .instantiate_with_services(HostCtx::default(), None, Some(provider))
        .await
        .unwrap();
    component.on_start_async(None).await.unwrap();
    component
}
async fn call(component: &mut WasmComponent, command: &str, payload: &[u8]) -> Vec<u8> {
    component
        .handle_async(command, payload, None)
        .await
        .unwrap()
}
async fn guarded(future: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("subscription resource deadline");
}

#[tokio::test]
async fn cancellation_retires_receivers_immediately_and_rebuild_cannot_reuse_a_published_handle() {
    guarded(async {
        let provider = Arc::new(Provider::default());
        let mut component = open(provider.clone()).await;
        let first = call(&mut component, "subscribe", b"").await;
        assert_eq!(first, 1u64.to_le_bytes());
        {
            let mut pending = Box::pin(component.handle_async("next", &first, None));
            tokio::select! {
                result = &mut pending => panic!("empty receiver must suspend: {result:?}"),
                () = provider.waiting.notified() => {},
            }
        }
        assert_eq!(provider.drops.load(Ordering::SeqCst), 1);
        assert!(provider.senders.lock().unwrap()[0].is_closed());
        let second = call(&mut component, "subscribe", b"").await;
        assert_eq!(second, 2u64.to_le_bytes());
        assert_eq!(
            call(&mut component, "next", &first).await,
            b"error:InvalidHandle"
        );
        assert_eq!(call(&mut component, "close", &second).await, b"ok");
        assert_eq!(provider.drops.load(Ordering::SeqCst), 2);
    })
    .await;
}

#[tokio::test]
async fn trap_retires_all_receivers_before_return_and_preserves_monotonic_identity() {
    guarded(async {
        let provider = Arc::new(Provider::default());
        let mut component = open(provider.clone()).await;
        let first = call(&mut component, "subscribe", b"").await;
        let second = call(&mut component, "subscribe", b"").await;
        assert!(component.handle_async("trap", b"", None).await.is_err());
        assert_eq!(provider.drops.load(Ordering::SeqCst), 2);
        let third = call(&mut component, "subscribe", b"").await;
        assert_eq!(third, 3u64.to_le_bytes());
        for old in [first, second] {
            assert_eq!(
                call(&mut component, "next", &old).await,
                b"error:InvalidHandle"
            );
        }
        drop(component);
        assert_eq!(provider.drops.load(Ordering::SeqCst), 3);
    })
    .await;
}

#[tokio::test]
async fn timeout_keeps_queued_event_semantics_and_eof_retires_the_handle() {
    guarded(async {
        let provider = Arc::new(Provider::default());
        let mut component = open(provider.clone()).await;
        let handle = call(&mut component, "subscribe", b"").await;
        assert_eq!(
            call(&mut component, "poll", &handle).await,
            b"error:Timeout"
        );
        let sender = provider.senders.lock().unwrap()[0].clone();
        sender
            .send(ServiceEvent {
                registered: true,
                path: "/user/real".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            call(&mut component, "next", &handle).await,
            b"registered:/user/real"
        );
        assert_eq!(provider.drops.load(Ordering::SeqCst), 0);
        drop(sender);
        provider.senders.lock().unwrap().clear();
        assert_eq!(call(&mut component, "next", &handle).await, b"closed");
        assert_eq!(provider.drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            call(&mut component, "next", &handle).await,
            b"error:InvalidHandle"
        );
    })
    .await;
}

#[tokio::test]
async fn explicit_close_and_store_drop_isolate_each_host_receiver_destructor() {
    guarded(async {
        let provider = Arc::new(Provider::default());
        provider.panic_drop.store(true, Ordering::SeqCst);
        let mut component = open(provider.clone()).await;
        let first = call(&mut component, "subscribe", b"").await;
        for _ in 0..2 {
            call(&mut component, "subscribe", b"").await;
        }
        for command in ["close", "echo", "close"] {
            assert!(matches!(
                component.handle_async(command, &first, None).await,
                Err(parrot_wasm::ComponentError::Panic(ref message))
                    if message.contains("host subscription destructor panic")
            ));
        }
        assert_eq!(provider.drops.load(Ordering::SeqCst), 3);
        drop(component);
        assert_eq!(provider.drops.load(Ordering::SeqCst), 3);
        assert!(provider
            .senders
            .lock()
            .unwrap()
            .iter()
            .all(|sender| sender.is_closed()));
    })
    .await;
}

#[tokio::test]
async fn invalid_inputs_and_shared_messaging_discovery_budget_do_not_admit_extra_host_work() {
    guarded(async {
        let provider = Arc::new(Provider::default());
        let mut component = open(provider.clone()).await;
        for command in ["zero-timeout", "big-timeout", "oversize-key"] {
            assert_eq!(
                call(&mut component, command, b"").await,
                b"error:InvalidRequest"
            );
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(call(&mut component, "quota", b"").await, b"error:Limit");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 64);
        assert_eq!(
            call(&mut component, "mixed-quota", b"").await,
            b"error:Limit"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 96);
        assert_eq!(call(&mut component, "register", b"").await, b"ok");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 97);
    })
    .await;
}

#[tokio::test]
async fn oversized_provider_event_retires_its_stream_and_keeps_other_handles_usable() {
    guarded(async {
        let provider = Arc::new(Provider::default());
        let mut component = open(provider.clone()).await;
        let first = call(&mut component, "subscribe", b"").await;
        let sender = provider.senders.lock().unwrap()[0].clone();
        sender
            .send(ServiceEvent {
                registered: true,
                path: "x".repeat(4097),
            })
            .await
            .unwrap();
        assert_eq!(call(&mut component, "next", &first).await, b"error:Limit");
        assert!(sender.is_closed());
        assert_eq!(provider.drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            call(&mut component, "next", &first).await,
            b"error:InvalidHandle"
        );
        let second = call(&mut component, "subscribe", b"").await;
        assert_eq!(call(&mut component, "close", &second).await, b"ok");
        assert_eq!(provider.drops.load(Ordering::SeqCst), 2);
    })
    .await;
}

#[tokio::test]
async fn cancelling_a_pending_subscription_retires_unpublished_resources() {
    guarded(async {
        let provider = Arc::new(Provider::default());
        provider.hold_subscribe.store(true, Ordering::SeqCst);
        let mut component = open(provider.clone()).await;
        {
            let mut pending = Box::pin(component.handle_async("subscribe", b"", None));
            tokio::select! {
                result = &mut pending => panic!("held subscription returned: {result:?}"),
                () = provider.entered.notified() => {},
            }
        }
        assert_eq!(provider.drops.load(Ordering::SeqCst), 1);
        assert!(provider.senders.lock().unwrap()[0].is_closed());
        provider.hold_subscribe.store(false, Ordering::SeqCst);
        let handle = call(&mut component, "subscribe", b"").await;
        assert_eq!(call(&mut component, "close", &handle).await, b"ok");
        assert_eq!(provider.drops.load(Ordering::SeqCst), 2);
    })
    .await;
}
