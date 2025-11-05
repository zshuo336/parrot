//! Application control callbacks must not unwind through independently owned resources.
use parrot_api::types::{BoxedActorRef, BoxedFuture};
use parrot_remote::{
    admin::SysEvent,
    error::{decode_err_payload, ErrCode},
    frame::{frame_type, FrameHeader, DEFAULT_HOP_LIMIT, SEQ_NONE},
    ingress::{Ingress, LocalLookup, SysEventHook},
    registry::ReplyPayload,
    resolve::{self, Query},
    roles::directory::DirEntry,
    transport::FrameSender,
    CallbackRegistry, ControlPolicy, Frame,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Weak,
    },
    task::{Context, Poll},
    time::Duration,
};
#[derive(Default)]
struct State {
    calls: AtomicUsize,
    polls: AtomicUsize,
    futures: AtomicUsize,
    owners: AtomicUsize,
    payloads: AtomicUsize,
    secondaries: AtomicUsize,
}
struct Payload(Arc<State>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Secondary(self.0.clone()));
    }
}
struct Secondary(Arc<State>);
impl Drop for Secondary {
    fn drop(&mut self) {
        self.0.secondaries.fetch_add(1, Ordering::SeqCst);
        panic!("secondary destructor must not run");
    }
}
fn fail(s: &Arc<State>) -> ! {
    std::panic::panic_any(Payload(s.clone()))
}
fn entry(version: u64) -> DirEntry {
    DirEntry {
        endpoints: vec!["tcp://127.0.0.1:7001".into()],
        version,
        ttl_s: 60,
    }
}
struct Lookup(Arc<State>);
struct ResolveFuture {
    state: Arc<State>,
    mode: u8,
}
impl Future for ResolveFuture {
    type Output = Result<Option<DirEntry>, String>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.state.polls.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            2 | 4 => fail(&self.state),
            5 | 6 => Poll::Pending,
            7 => Poll::Ready(Err("backend stale".into())),
            8 => Poll::Ready(Ok(Some(entry(0)))),
            _ => Poll::Ready(Ok(Some(entry(7)))),
        }
    }
}
impl Drop for ResolveFuture {
    fn drop(&mut self) {
        self.state.futures.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, 3..=7) {
            fail(&self.state);
        }
    }
}
impl LocalLookup for Lookup {
    fn lookup<'a, 'b, 'f>(&'a self, _: &'b str) -> BoxedFuture<'f, Option<BoxedActorRef>>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async { None })
    }
    fn resolve_directory<'a, 'b, 'f>(
        &'a self,
        key: &'b str,
        _: u64,
    ) -> BoxedFuture<'f, Result<Option<DirEntry>, String>>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        let mode = key.parse().unwrap();
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        if mode == 1 {
            fail(&self.0);
        }
        Box::pin(ResolveFuture {
            state: self.0.clone(),
            mode,
        })
    }
}
fn query(mode: u8, cid: u64) -> Frame {
    let key = mode.to_string();
    Frame {
        header: FrameHeader {
            frame_len: 0,
            version: 1,
            frame_type: frame_type::RESOLVE_Q,
            flags: 0,
            correlation_id: cid,
            hop_count: 0,
            hop_limit: DEFAULT_HOP_LIMIT,
            seq: SEQ_NONE,
        },
        path: key.clone(),
        type_key: resolve::RESPONSE_KEY.into(),
        payload: resolve::encode_query(&Query {
            key,
            min_version: 1,
        })
        .unwrap()
        .into(),
    }
}
fn setup(
    secure: bool,
) -> (
    Arc<Ingress>,
    Arc<State>,
    FrameSender,
    tokio::sync::mpsc::Receiver<Frame>,
) {
    let state = Arc::new(State::default());
    let ingress = Ingress::new(
        Arc::new(Lookup(state.clone())),
        Arc::new(CallbackRegistry::default()),
    );
    let ingress = if secure {
        ingress
    } else {
        ingress.with_control_policy(ControlPolicy::InsecureDevelopment)
    };
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    (Arc::new(ingress), state, FrameSender::new(tx, "peer"), rx)
}
async fn bounded<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(3), f)
        .await
        .expect("control operation deadline")
}
async fn resolver(mode: u8) {
    tokio::time::pause();
    let (ingress, state, back, mut replies) = setup(mode == 9);
    let mut request = query(mode, 71);
    if mode == 10 {
        request.path = "unbound".into();
    }
    if mode == 6 {
        // Exhaust the entire admission budget if cancellation leaks one permit.
        for _ in 0..64 {
            let mut work = Box::pin(ingress.dispatch(request.clone(), &back, "peer"));
            assert!(futures::poll!(&mut work).is_pending());
            drop(work);
        }
        assert!(replies.try_recv().is_err());
    } else {
        bounded(ingress.dispatch(request, &back, "peer")).await;
        let response = bounded(replies.recv()).await.unwrap();
        assert_eq!(response.header.correlation_id, 71);
        if mode == 0 {
            assert_eq!(response.header.frame_type, frame_type::RESOLVE_R);
            assert_eq!(
                resolve::decode_response(&response.payload).unwrap().entry,
                Some(entry(7))
            );
        } else {
            assert_eq!(response.header.frame_type, frame_type::REPLY_ERR);
            let (code, detail) = decode_err_payload(&response.payload).unwrap();
            assert_eq!(
                code,
                match mode {
                    5 => ErrCode::Timeout,
                    7 | 8 => ErrCode::DirectoryStale,
                    9 => ErrCode::Forbidden,
                    _ => ErrCode::ProtocolViolation,
                }
            );
            if mode == 7 {
                assert_eq!(detail, "backend stale");
            }
        }
    }
    assert_eq!(
        state.futures.load(Ordering::SeqCst),
        match mode {
            1 | 9 | 10 => 0,
            6 => 64,
            _ => 1,
        }
    );
    assert_eq!(
        state.calls.load(Ordering::SeqCst),
        match mode {
            9 | 10 => 0,
            6 => 64,
            _ => 1,
        }
    );
    assert_eq!(
        state.payloads.load(Ordering::SeqCst),
        match mode {
            0 | 8..=10 => 0,
            4 => 2,
            6 => 64,
            _ => 1,
        }
    );
    assert_eq!(state.secondaries.load(Ordering::SeqCst), 0);
    assert!(!back.is_closed());
    if mode != 9 {
        bounded(ingress.dispatch(query(0, 72), &back, "peer")).await;
        let response = bounded(replies.recv()).await.unwrap();
        assert_eq!(response.header.correlation_id, 72);
        assert_eq!(response.header.frame_type, frame_type::RESOLVE_R);
    }
    assert!(replies.try_recv().is_err());
    ingress.close();
    bounded(ingress.wait_closed()).await;
}
struct Hook {
    legacy_only: bool,
    state: Arc<State>,
    ingress: Weak<Ingress>,
    mode: u8,
}
impl Drop for Hook {
    fn drop(&mut self) {
        self.state.owners.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, 3..=6 | 8 | 10) && self.state.calls.load(Ordering::SeqCst) > 0 {
            fail(&self.state);
        }
    }
}
struct HookFuture {
    state: Arc<State>,
    mode: u8,
    back: FrameSender,
}
impl Future for HookFuture {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        self.state.polls.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            2 | 4 => fail(&self.state),
            6 => Poll::Pending,
            5 => {
                self.back
                    .send_frame_sync(Frame::reply(71, "", "hook-success", bytes::Bytes::new()))
                    .unwrap();
                Poll::Ready(())
            }
            _ => Poll::Ready(()),
        }
    }
}
impl Drop for HookFuture {
    fn drop(&mut self) {
        self.state.futures.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, 3..=6) {
            fail(&self.state);
        }
    }
}
impl Hook {
    fn start(&self) {
        self.state.calls.fetch_add(1, Ordering::SeqCst);
        *self.ingress.upgrade().unwrap().sys_event_slot() = None;
        if self.mode == 1 {
            fail(&self.state);
        }
    }
}
impl SysEventHook for Hook {
    fn on_event<'a, 'b, 'c, 'f>(
        &'a self,
        event: SysEvent,
        back: &'b FrameSender,
        _: &'c str,
    ) -> BoxedFuture<'f, ()>
    where
        'a: 'f,
        'b: 'f,
        'c: 'f,
        Self: 'f,
    {
        // Membership traffic can arrive between connect and the injected case.
        // Only the uniquely marked event on the selected hook channel is ours.
        if self.legacy_only
            || !matches!(event, SysEvent::MembershipGossip(body) if body.as_ref() == b"control-unwind-test")
        {
            return Box::pin(async {});
        }
        self.start();
        Box::pin(HookFuture {
            state: self.state.clone(),
            mode: self.mode,
            back: back.clone(),
        })
    }
    fn on_reply_error(&self, id: u64, _: ErrCode, _: &str, _: &FrameSender) -> bool {
        if !self.legacy_only || id != 71 {
            return false;
        }
        self.start();
        if matches!(self.mode, 2 | 4) {
            fail(&self.state);
        }
        matches!(self.mode, 7 | 8)
    }
}
fn event() -> Frame {
    Frame {
        header: FrameHeader {
            frame_len: 0,
            version: 1,
            frame_type: frame_type::SYSTEM_EVENT,
            flags: 0,
            correlation_id: 71,
            hop_count: 0,
            hop_limit: DEFAULT_HOP_LIMIT,
            seq: SEQ_NONE,
        },
        path: String::new(),
        type_key: String::new(),
        payload: bytes::Bytes::from_static(b"\x10control-unwind-test"),
    }
}
async fn hook(mode: u8, legacy: bool) {
    let (ingress, state, back, mut replies) = setup(mode == 9);
    *ingress.sys_event_slot() = Some(Arc::new(Hook {
        legacy_only: legacy,
        state: state.clone(),
        ingress: Arc::downgrade(&ingress),
        mode,
    }));
    let (tx, mut callback) = tokio::sync::oneshot::channel();
    if legacy {
        ingress.callbacks.insert_on(71, "peer", tx, &back).unwrap();
    }
    let frame = if legacy {
        Frame::reply_err(71, "", ErrCode::Stopped, "remote stopped")
    } else {
        event()
    };
    if mode == 6 {
        let mut work = Box::pin(ingress.dispatch(frame, &back, "peer"));
        assert!(futures::poll!(&mut work).is_pending());
        drop(work);
    } else {
        bounded(ingress.dispatch(frame, &back, "peer")).await;
    }
    let failed = if legacy {
        matches!(mode, 1..=4 | 8 | 10)
    } else {
        matches!(mode, 1..=6 | 9 | 10)
    };
    assert_eq!(
        back.is_closed(),
        failed,
        "close only the affected connection on uncertain hook outcome"
    );
    assert_eq!(state.calls.load(Ordering::SeqCst), usize::from(mode != 9));
    assert_eq!(state.owners.load(Ordering::SeqCst), usize::from(mode != 9));
    assert_eq!(
        state.futures.load(Ordering::SeqCst),
        usize::from(!legacy && !matches!(mode, 1 | 9))
    );
    assert_eq!(state.secondaries.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.payloads.load(Ordering::SeqCst),
        if legacy {
            match mode {
                0 | 7 => 0,
                4 => 2,
                _ => 1,
            }
        } else {
            match mode {
                0 | 9 => 0,
                4 => 3,
                3 | 5 | 6 => 2,
                _ => 1,
            }
        }
    );
    if mode == 5 {
        assert_eq!(replies.try_recv().unwrap().type_key, "hook-success");
    }
    assert!(
        replies.try_recv().is_err(),
        "do not fabricate a duplicate control reply after a callback failure"
    );
    if legacy && mode == 0 {
        assert!(matches!(
            callback.try_recv(),
            Ok(ReplyPayload::Err(ErrCode::Stopped, _))
        ));
    }
    if legacy && (failed || mode == 7) {
        assert!(
            callback.try_recv().is_err(),
            "failed or consuming hooks cannot fall through to actor callbacks"
        );
        assert_eq!(ingress.callbacks.len(), 1);
    }
    if legacy {
        ingress
            .callbacks
            .fail_connection(&back, ErrCode::ConnectionLost, "closed");
        assert!(ingress.callbacks.is_empty());
    }
    // The registry lock is released during the callback: replacement remains usable.
    let (next, mut rx) = tokio::sync::mpsc::channel(8);
    let next = FrameSender::new(next, "other");
    bounded(ingress.dispatch(query(0, 72), &next, "other")).await;
    let reply = rx.recv().await.unwrap();
    assert_eq!(reply.header.correlation_id, 72);
    assert_eq!(
        reply.header.frame_type,
        if mode == 9 {
            frame_type::REPLY_ERR
        } else {
            frame_type::RESOLVE_R
        }
    );
    assert!(!next.is_closed());
    ingress.close();
    bounded(ingress.wait_closed()).await;
}
#[derive(Clone, Copy)]
enum Case {
    Resolve(u8),
    Hook(u8, bool),
    Tcp(u8),
}
async fn exercise(case: Case) {
    match case {
        Case::Resolve(mode) => resolver(mode).await,
        Case::Hook(mode, legacy) => hook(mode, legacy).await,
        Case::Tcp(mode) => tcp(mode).await,
    }
}
fn run(name: &str, case: Case) {
    const CHILD: &str = "PARROT_CONTROL_UNWIND_CASE";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(exercise(case));
        return;
    }
    use std::{
        io::{Read, Seek},
        process::{Command, Stdio},
        time::Instant,
    };
    let root = std::env::var_os("PARROT_CONTROL_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!(
        "control-{}-{}-{name}.log",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut log = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .stdout(log.try_clone().unwrap())
        .stderr(Stdio::from(log.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "bounded ingress child failed: {result:?}; log={}",
                    path.display()
                );
            }
        }
    };
    log.rewind().unwrap();
    let mut text = String::new();
    log.read_to_string(&mut text).unwrap();
    assert!(
        status.success(),
        "{name}: {status}; log={}\n{text}",
        path.display()
    );
    assert!(
        text.contains("test result: ok. 1 passed; 0 failed;"),
        "{text}"
    );
}
macro_rules! contract {
    ($name:ident,$case:expr) => {
        #[test]
        fn $name() {
            run(stringify!($name), $case);
        }
    };
}
contract!(healthy_resolve, Case::Resolve(0));
contract!(resolve_constructor_panic, Case::Resolve(1));
contract!(resolve_poll_panic, Case::Resolve(2));
contract!(resolve_future_drop, Case::Resolve(3));
contract!(resolve_poll_and_drop, Case::Resolve(4));
contract!(resolve_timeout_drop, Case::Resolve(5));
contract!(resolve_cancel_releases_all_slots, Case::Resolve(6));
contract!(resolve_business_error_survives_drop, Case::Resolve(7));
contract!(resolve_stale_version, Case::Resolve(8));
contract!(resolve_unauthorized, Case::Resolve(9));
contract!(resolve_malformed_binding, Case::Resolve(10));
contract!(healthy_hook, Case::Hook(0, false));
contract!(hook_constructor, Case::Hook(1, false));
contract!(hook_poll, Case::Hook(2, false));
contract!(hook_future_and_owner_drop, Case::Hook(3, false));
contract!(hook_poll_future_owner_drop, Case::Hook(4, false));
contract!(hook_sent_reply_then_failed_cleanup, Case::Hook(5, false));
contract!(hook_cancel_cleanup, Case::Hook(6, false));
contract!(hook_unauthorized, Case::Hook(9, false));
contract!(hook_owner_only_drop, Case::Hook(10, false));
contract!(legacy_fallback, Case::Hook(0, true));
contract!(legacy_callback_panics_before_result, Case::Hook(1, true));
contract!(legacy_call_and_owner_drop, Case::Hook(4, true));
contract!(legacy_consumed, Case::Hook(7, true));
contract!(legacy_consumed_owner_drop, Case::Hook(8, true));
contract!(legacy_fallback_owner_drop, Case::Hook(10, true));

struct FailingHint(Arc<State>);
impl parrot_remote::ingress::RelayRouter for FailingHint {
    fn sender_of(&self, _: &str) -> Option<FrameSender> {
        None
    }
    fn next_cid(&self) -> u64 {
        1
    }
    fn self_node(&self) -> &str {
        "receiver"
    }
    fn dial_addr_of(&self, _: &str) -> Option<String> {
        None
    }
    fn on_hint(&self, node: &str, _: &str) {
        if node != "hint-node" {
            return;
        }
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        fail(&self.0);
    }
}
async fn tcp(mode: u8) {
    use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig};
    let state = Arc::new(State::default());
    let target = RemoteActorSystem::new(
        RemoteConfig::tcp("receiver", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Lookup(state.clone())),
    )
    .unwrap();
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp("source", None).with_insecure_development(),
        Arc::new(Lookup(Arc::default())),
    )
    .unwrap();
    let other = RemoteActorSystem::new(
        RemoteConfig::tcp("bystander", None).with_insecure_development(),
        Arc::new(Lookup(Arc::default())),
    )
    .unwrap();
    bounded(target.start()).await.unwrap();
    bounded(source.start()).await.unwrap();
    bounded(other.start()).await.unwrap();
    let address = NodeAddr::tcp("receiver", target.local_addr().unwrap());
    bounded(source.connect(&address)).await.unwrap();
    bounded(other.connect(&address)).await.unwrap();
    let original = source
        .links_snapshot()
        .await
        .into_iter()
        .find(|(id, _, _)| id == "receiver")
        .unwrap()
        .1;
    let unaffected = other
        .links_snapshot()
        .await
        .into_iter()
        .find(|(id, _, _)| id == "receiver")
        .unwrap()
        .1;
    let (tx, callback) = tokio::sync::oneshot::channel();
    if mode == 0 {
        let error = bounded(source.ingress.resolve_remote(
            &original,
            "4",
            1,
            Duration::from_secs(2),
        ))
        .await
        .unwrap_err();
        assert!(error.to_string().contains("ProtocolViolation"), "{error}");
        assert!(!original.is_closed());
        assert_eq!(source.ingress.pending_resolves(), 0);
        assert_eq!(state.futures.load(Ordering::SeqCst), 1);
        assert_eq!(state.payloads.load(Ordering::SeqCst), 2);
        let next = bounded(source.ingress.resolve_remote(
            &original,
            "0",
            1,
            Duration::from_secs(2),
        ))
        .await
        .unwrap();
        assert_eq!(next, Some(entry(7)));
        assert!(source
            .links_snapshot()
            .await
            .into_iter()
            .find(|(id, _, _)| id == "receiver")
            .unwrap()
            .1
            .same_connection(&original));
    } else {
        if mode == 3 {
            *target.ingress.relay_slot() = Some(Arc::new(FailingHint(state.clone())));
            bounded(original.send(Frame::try_route_hint("hint-node", "127.0.0.1:7001").unwrap()))
                .await
                .unwrap();
        } else {
            *target.ingress.sys_event_slot() = Some(Arc::new(Hook {
                legacy_only: mode == 2,
                state: state.clone(),
                ingress: Arc::downgrade(&target.ingress),
                mode: 4,
            }));
            if mode == 2 {
                let receiving = target
                    .links_snapshot()
                    .await
                    .into_iter()
                    .find(|(id, _, _)| id == "source")
                    .unwrap()
                    .1;
                target
                    .ingress
                    .callbacks
                    .insert_on(71, "source", tx, &receiving)
                    .unwrap();
                bounded(original.send(Frame::reply_err(
                    71,
                    "",
                    ErrCode::Stopped,
                    "remote stopped",
                )))
                .await
                .unwrap();
                assert!(
                    matches!(
                        bounded(callback).await.unwrap(),
                        ReplyPayload::Err(ErrCode::ConnectionLost, _)
                    ),
                    "failed hook must not fall through as Stopped"
                );
                assert!(target.ingress.callbacks.is_empty());
            } else {
                bounded(original.send(event())).await.unwrap();
            }
        }
        bounded(original.closed()).await;
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.owners.load(Ordering::SeqCst), usize::from(mode != 3));
        assert_eq!(state.futures.load(Ordering::SeqCst), usize::from(mode == 1));
        assert_eq!(
            state.payloads.load(Ordering::SeqCst),
            match mode {
                1 => 3,
                2 => 2,
                _ => 1,
            }
        );
    }
    assert_eq!(state.secondaries.load(Ordering::SeqCst), 0);
    let next = bounded(
        other
            .ingress
            .resolve_remote(&unaffected, "0", 1, Duration::from_secs(2)),
    )
    .await
    .unwrap();
    assert_eq!(next, Some(entry(7)));
    assert!(!unaffected.is_closed());
    assert!(other
        .links_snapshot()
        .await
        .into_iter()
        .find(|(id, _, _)| id == "receiver")
        .unwrap()
        .1
        .same_connection(&unaffected));
    bounded(source.shutdown()).await.unwrap();
    bounded(other.shutdown()).await.unwrap();
    bounded(target.shutdown()).await.unwrap();
}
contract!(tcp_resolver_panic_keeps_same_connection, Case::Tcp(0));
contract!(tcp_event_failure_closes_only_its_connection, Case::Tcp(1));
contract!(
    tcp_legacy_error_failure_releases_connection_callbacks,
    Case::Tcp(2)
);
contract!(
    tcp_outer_dispatch_retires_throwing_panic_payload,
    Case::Tcp(3)
);
