#![cfg(feature = "runtime")]

use parrot_wasm::{
    runtime::{
        GuestMessaging, GuestServices, GuestSubscription, MessagingError, MessagingRequest, Msg,
        ServiceError, ServiceEvent, ServiceFuture, WasmComponent, WasmRuntime,
    },
    ComponentError, HostCtx, WasmConfig,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
};

#[derive(Clone, Copy, Debug)]
enum Response {
    Eof,
    Closed,
    Oversized,
    Control,
    Event,
    Timeout,
    Failed,
}
impl Response {
    fn value(self) -> Result<Option<ServiceEvent>, ServiceError> {
        match self {
            Self::Eof => Ok(None),
            Self::Closed => Err(ServiceError::Closed),
            Self::Oversized => Ok(Some(ServiceEvent {
                registered: true,
                path: "x".repeat(4097),
            })),
            Self::Control => Ok(Some(ServiceEvent {
                registered: false,
                path: "/user/bad\npath".into(),
            })),
            Self::Event => Ok(Some(ServiceEvent {
                registered: true,
                path: "\u{e9}".repeat(2048),
            })),
            Self::Timeout => Err(ServiceError::Timeout),
            Self::Failed => Err(ServiceError::Failed),
        }
    }
    fn reply(self) -> Vec<u8> {
        match self {
            Self::Eof => b"closed".to_vec(),
            Self::Closed => b"error:Closed".to_vec(),
            Self::Oversized | Self::Control => b"error:Limit".to_vec(),
            Self::Event => format!("registered:{}", "\u{e9}".repeat(2048)).into_bytes(),
            Self::Timeout => b"error:Timeout".to_vec(),
            Self::Failed => b"error:Failed".to_vec(),
        }
    }
}
const TERMINAL: [Response; 4] = [
    Response::Eof,
    Response::Closed,
    Response::Oversized,
    Response::Control,
];

#[derive(Debug)]
struct Probe {
    response: Mutex<Response>,
    panic_drop: AtomicBool,
    subscribed: AtomicUsize,
    next: AtomicUsize,
    registrations: AtomicUsize,
    messages: AtomicUsize,
    drops: Mutex<Vec<usize>>,
}
impl Probe {
    fn new(response: Response, panic_drop: bool) -> Arc<Self> {
        Arc::new(Self {
            response: Mutex::new(response),
            panic_drop: AtomicBool::new(panic_drop),
            subscribed: AtomicUsize::new(0),
            next: AtomicUsize::new(0),
            registrations: AtomicUsize::new(0),
            messages: AtomicUsize::new(0),
            drops: Mutex::new(Vec::new()),
        })
    }
    fn writes(&self) -> (usize, usize) {
        (self.registrations.load(SeqCst), self.messages.load(SeqCst))
    }
    fn dropped(&self) -> Vec<usize> {
        let mut ids = self.drops.lock().unwrap().clone();
        ids.sort_unstable();
        ids
    }
}
#[derive(Debug)]
struct Receiver {
    probe: Arc<Probe>,
    id: usize,
    panic_drop: bool,
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.probe.drops.lock().unwrap().push(self.id);
        assert!(!self.panic_drop, "next receiver disposal failed");
    }
}
impl GuestSubscription for Receiver {
    fn next(&mut self, _: u32) -> ServiceFuture<'_, Option<ServiceEvent>> {
        self.probe.next.fetch_add(1, SeqCst);
        let result = self.probe.response.lock().unwrap().value();
        Box::pin(async move { result })
    }
}

#[derive(Debug)]
struct Authority(Arc<Probe>);
impl GuestServices for Authority {
    fn register(&self, key: String, _: u32) -> ServiceFuture<'_, ()> {
        assert_eq!(key, "edge/after-next");
        self.0.registrations.fetch_add(1, SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn deregister(&self, _: String, _: u32) -> ServiceFuture<'_, ()> {
        panic!("fixture must not deregister")
    }
    fn subscribe(&self, _: String, _: u32) -> ServiceFuture<'_, Box<dyn GuestSubscription>> {
        let receiver = Receiver {
            probe: self.0.clone(),
            id: self.0.subscribed.fetch_add(1, SeqCst) + 1,
            panic_drop: self.0.panic_drop.load(SeqCst),
        };
        Box::pin(async move { Ok(Box::new(receiver) as Box<dyn GuestSubscription>) })
    }
}
impl GuestMessaging for Authority {
    fn ask(
        &self,
        _: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Msg, MessagingError>> + Send + '_>> {
        panic!("fixture must not ASK")
    }
    fn tell(
        &self,
        request: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessagingError>> + Send + '_>> {
        assert_eq!(request.destination, "/user/after-next");
        assert_eq!(request.message.type_key, "bin:after-next#v1");
        assert_eq!(request.message.payload, [42]);
        self.0.messages.fetch_add(1, SeqCst);
        Box::pin(async { Ok(()) })
    }
}

async fn open(probe: &Arc<Probe>, settings: &[(&str, &str)]) -> WasmComponent {
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 20_000_000,
        epoch_deadline: 10_000,
        ..Default::default()
    })
    .unwrap();
    let mut host = HostCtx::default();
    for (key, value) in settings {
        host.config_overlay.insert(*key, *value);
    }
    let authority = Arc::new(Authority(probe.clone()));
    runtime
        .prepare_bytes(include_bytes!("fixtures/services-component.wasm"))
        .unwrap()
        .instantiate_with_services(host, Some(authority.clone()), Some(authority))
        .await
        .unwrap()
}
async fn subscribe(component: &mut WasmComponent) -> Vec<u8> {
    component
        .handle_async("subscribe", b"", None)
        .await
        .unwrap()
}
fn failure() -> ComponentError {
    ComponentError::Panic("next receiver disposal failed".into())
}
async fn rejected(component: &mut WasmComponent, probe: &Probe) {
    assert_eq!(component.cleanup_error(), Some(failure()));
    assert!(!component.is_alive());
    let before = (
        probe.subscribed.load(SeqCst),
        probe.next.load(SeqCst),
        probe.writes(),
        probe.dropped(),
    );
    assert_eq!(
        component.handle_async("echo", b"", None).await,
        Err(failure())
    );
    assert_eq!(
        component.tell_async("echo", b"", None).await,
        Err(failure())
    );
    assert_eq!(component.on_start_async(None).await, Err(failure()));
    assert_eq!(component.on_drain_async(None).await, Err(failure()));
    let synchronous = ComponentError::InvalidConfig(
        "messaging-enabled components require asynchronous invocation".into(),
    );
    assert_eq!(component.handle("echo", b""), Err(synchronous.clone()));
    assert_eq!(component.tell("echo", b""), Err(synchronous.clone()));
    assert_eq!(component.on_start(), Err(synchronous.clone()));
    assert_eq!(component.on_drain(), Err(synchronous));
    assert_eq!(
        (
            probe.subscribed.load(SeqCst),
            probe.next.load(SeqCst),
            probe.writes(),
            probe.dropped(),
        ),
        before
    );
}
async fn ask_failure(response: Response) {
    let probe = Probe::new(response, true);
    let mut component = open(&probe, &[]).await;
    component.on_start_async(None).await.unwrap();
    let handle = subscribe(&mut component).await;
    subscribe(&mut component).await;
    subscribe(&mut component).await;
    assert_eq!(
        component
            .handle_async("next-then-write", &handle, None)
            .await,
        Err(failure())
    );
    assert_eq!(
        probe.writes(),
        (0, 0),
        "guest resumed after failed retirement"
    );
    assert_eq!(probe.next.load(SeqCst), 1);
    assert_eq!(probe.dropped(), [1, 2, 3]);
    rejected(&mut component, &probe).await;
    drop(component);
    assert_eq!(probe.dropped(), [1, 2, 3]);
}

#[tokio::test]
async fn eof_disposal_failure_stops_guest_before_followup_writes() {
    ask_failure(Response::Eof).await;
}
#[tokio::test]
async fn closed_disposal_failure_stops_guest_before_followup_writes() {
    ask_failure(Response::Closed).await;
}
#[tokio::test]
async fn oversized_event_disposal_failure_stops_guest_before_followup_writes() {
    ask_failure(Response::Oversized).await;
}
#[tokio::test]
async fn control_event_disposal_failure_stops_guest_before_followup_writes() {
    ask_failure(Response::Control).await;
}
#[tokio::test]
async fn tell_does_not_resume_after_terminal_receiver_disposal_failure() {
    for response in TERMINAL {
        let probe = Probe::new(response, true);
        let mut component = open(&probe, &[]).await;
        component.on_start_async(None).await.unwrap();
        let handle = subscribe(&mut component).await;
        subscribe(&mut component).await;
        assert_eq!(
            component.tell_async("next-then-write", &handle, None).await,
            Err(failure())
        );
        assert_eq!(probe.writes(), (0, 0));
        assert_eq!(probe.dropped(), [1, 2]);
        rejected(&mut component, &probe).await;
    }
}
#[tokio::test]
async fn startup_does_not_resume_after_terminal_receiver_disposal_failure() {
    for response in TERMINAL {
        let probe = Probe::new(response, true);
        let mut component = open(
            &probe,
            &[
                ("subscribe-on-start", "edge/source"),
                ("next-on-start", "true"),
            ],
        )
        .await;
        assert_eq!(component.on_start_async(None).await, Err(failure()));
        assert_eq!(probe.writes(), (0, 0));
        assert_eq!(probe.dropped(), [1]);
        rejected(&mut component, &probe).await;
    }
}
#[tokio::test]
async fn drain_does_not_resume_after_terminal_receiver_disposal_failure() {
    for response in TERMINAL {
        let probe = Probe::new(response, true);
        let mut component = open(&probe, &[("next-on-drain", "true")]).await;
        component.on_start_async(None).await.unwrap();
        subscribe(&mut component).await;
        subscribe(&mut component).await;
        assert_eq!(component.on_drain_async(None).await, Err(failure()));
        assert_eq!(probe.writes(), (0, 0));
        assert_eq!(probe.dropped(), [1, 2]);
        rejected(&mut component, &probe).await;
    }
}
#[tokio::test]
async fn recovery_does_not_resume_startup_or_enter_business_after_disposal_failure() {
    for response in TERMINAL {
        let probe = Probe::new(Response::Timeout, false);
        let mut component = open(
            &probe,
            &[
                ("subscribe-on-start", "edge/source"),
                ("next-on-start", "true"),
            ],
        )
        .await;
        component.on_start_async(None).await.unwrap();
        assert_eq!(probe.writes(), (1, 1));
        assert!(component.handle_async("trap", b"", None).await.is_err());
        assert_eq!(probe.dropped(), [1]);
        *probe.response.lock().unwrap() = response;
        probe.panic_drop.store(true, SeqCst);
        assert_eq!(
            component
                .handle_async("register", b"edge/after-next", None)
                .await,
            Err(failure())
        );
        assert_eq!(probe.writes(), (1, 1));
        assert_eq!(probe.subscribed.load(SeqCst), 2);
        assert_eq!(probe.next.load(SeqCst), 2);
        assert_eq!(probe.dropped(), [1, 2]);
        rejected(&mut component, &probe).await;
    }
}
#[tokio::test]
async fn clean_terminal_retirement_returns_guest_result_and_preserves_siblings() {
    for response in TERMINAL {
        let probe = Probe::new(response, false);
        let mut component = open(&probe, &[]).await;
        component.on_start_async(None).await.unwrap();
        let handle = subscribe(&mut component).await;
        let sibling = subscribe(&mut component).await;
        assert_eq!(
            component
                .handle_async("next-then-write", &handle, None)
                .await
                .unwrap(),
            response.reply()
        );
        assert_eq!(probe.writes(), (1, 1));
        assert_eq!(probe.dropped(), [1]);
        assert_eq!(component.cleanup_error(), None);
        assert!(component.is_alive());
        assert_eq!(
            component.handle_async("next", &handle, None).await.unwrap(),
            b"error:InvalidHandle"
        );
        assert_eq!(
            component
                .handle_async("close", &sibling, None)
                .await
                .unwrap(),
            b"ok"
        );
        let later = subscribe(&mut component).await;
        assert_eq!(later, 3u64.to_le_bytes());
        drop(component);
        assert_eq!(probe.dropped(), [1, 2, 3]);
    }
}
#[tokio::test]
async fn valid_boundary_event_and_nonterminal_errors_do_not_retire_receiver() {
    let probe = Probe::new(Response::Event, false);
    let mut component = open(&probe, &[]).await;
    component.on_start_async(None).await.unwrap();
    let handle = subscribe(&mut component).await;
    for (index, response) in [
        Response::Event,
        Response::Timeout,
        Response::Failed,
        Response::Event,
    ]
    .into_iter()
    .enumerate()
    {
        *probe.response.lock().unwrap() = response;
        assert_eq!(
            component
                .handle_async("next-then-write", &handle, None)
                .await
                .unwrap(),
            response.reply()
        );
        assert_eq!(probe.writes(), (index + 1, index + 1));
        assert_eq!(probe.next.load(SeqCst), index + 1);
        assert!(probe.dropped().is_empty());
        assert_eq!(component.cleanup_error(), None);
        assert!(component.is_alive());
    }
    drop(component);
    assert_eq!(probe.dropped(), [1]);
}
