//! Default administrative paths must retire each application resource before replying.
use parrot_api::{
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    admin::{AdminCommand, AdminReply, SysEvent},
    admin_v2::{AdminCommandV2, AdminReplyV2, ComponentDeploy, ComponentExecutor},
    ingress::LocalLookup,
    transport::FrameSender,
    ErrCode,
};
use std::{
    any::Any,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock, Weak,
    },
    task::{Context, Poll},
    time::Duration,
};
#[derive(Debug, Default)]
struct State {
    starts: AtomicUsize,
    work_drops: AtomicUsize,
    lookup_drops: AtomicUsize,
    owners: AtomicUsize,
    errors: AtomicUsize,
    panics: AtomicUsize,
    payloads: AtomicUsize,
    secondaries: AtomicUsize,
}
static STATE: OnceLock<Arc<State>> = OnceLock::new();
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
        panic!("recursive payload destructor");
    }
}
fn fail(s: &Arc<State>) -> ! {
    s.panics.fetch_add(1, Ordering::SeqCst);
    std::panic::panic_any(Payload(s.clone()))
}
#[derive(Debug)]
struct BadError(Arc<State>);
impl std::fmt::Display for BadError {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fail(&self.0)
    }
}
impl std::error::Error for BadError {}
impl Drop for BadError {
    fn drop(&mut self) {
        self.0.errors.fetch_add(1, Ordering::SeqCst);
        fail(&self.0);
    }
}
struct Work {
    state: Arc<State>,
    mode: u8,
}
impl Work {
    fn new(state: Arc<State>, mode: u8) -> Self {
        state.starts.fetch_add(1, Ordering::SeqCst);
        if mode == 1 {
            fail(&state);
        }
        Self { state, mode }
    }
    fn step(&self) -> Poll<()> {
        match self.mode {
            2 | 3 => fail(&self.state),
            7 | 18 => Poll::Pending,
            _ => Poll::Ready(()),
        }
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        self.state.work_drops.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, 3 | 4 | 7 | 10) {
            fail(&self.state);
        }
    }
}
struct StopFuture(Work);
impl Future for StopFuture {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        std::task::ready!(self.0.step());
        Poll::Ready(match self.0.mode {
            6 => Err(ActorError::Other(BadError(self.0.state.clone()).into())),
            11 => Err(ActorError::Stopped),
            _ => Ok(()),
        })
    }
}
#[derive(Debug)]
struct Target {
    state: Arc<State>,
    mode: u8,
}
impl Drop for Target {
    fn drop(&mut self) {
        self.state.owners.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, 5 | 9..=11) {
            fail(&self.state);
        }
    }
}
#[async_trait::async_trait]
impl ActorRef for Target {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(StopFuture(Work::new(self.state.clone(), self.mode)))
    }
    fn path(&self) -> String {
        format!("/{}", self.mode)
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            state: self.state.clone(),
            mode: self.mode,
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct Lookup(Arc<State>);
struct LookupFuture {
    state: Arc<State>,
    mode: u8,
}
impl Future for LookupFuture {
    type Output = Option<BoxedActorRef>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if self.mode == 8 {
            fail(&self.state);
        }
        Poll::Ready(if self.mode == 15 {
            None
        } else {
            Some(Box::new(Target {
                state: self.state.clone(),
                mode: self.mode,
            }))
        })
    }
}
impl Drop for LookupFuture {
    fn drop(&mut self) {
        self.state.lookup_drops.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, 8 | 9) {
            fail(&self.state);
        }
    }
}
impl LocalLookup for Lookup {
    fn lookup<'a, 'b, 'f>(&'a self, path: &'b str) -> BoxedFuture<'f, Option<BoxedActorRef>>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        let mode = path.trim_start_matches('/').parse().unwrap();
        if mode == 17 {
            fail(&self.0);
        }
        Box::pin(LookupFuture {
            state: self.0.clone(),
            mode,
        })
    }
}
struct SpawnFuture(Work);
impl Future for SpawnFuture {
    type Output = ActorResult<BoxedActorRef>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        std::task::ready!(self.0.step());
        Poll::Ready(if self.0.mode == 6 {
            Err(ActorError::Other(BadError(self.0.state.clone()).into()))
        } else {
            Ok(Box::new(Target {
                state: self.0.state.clone(),
                mode: self.0.mode,
            }))
        })
    }
}
fn spawn(path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(SpawnFuture(Work::new(
        STATE.get().unwrap().clone(),
        path.trim_start_matches('/').parse().unwrap(),
    )))
}
inventory::submit! {parrot_remote::admin::PropsFactory{name:"admin-unwind-props",spawn}}
async fn bounded<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(3), f)
        .await
        .expect("administrative contract deadline")
}
fn check_payloads(s: &State) {
    assert_eq!(
        s.payloads.load(Ordering::SeqCst),
        s.panics.load(Ordering::SeqCst),
        "retire every original panic payload once"
    );
    assert_eq!(s.secondaries.load(Ordering::SeqCst), 0);
}
async fn v1(mode: u8, is_spawn: bool) {
    let state = STATE.get().unwrap().clone();
    let local: Arc<dyn LocalLookup> = Arc::new(Lookup(state.clone()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let back = FrameSender::new(tx, "source");
    let command = if is_spawn {
        AdminCommand::SpawnLocal {
            req_id: 71,
            props: if mode == 19 {
                "missing-props"
            } else {
                "admin-unwind-props"
            }
            .into(),
            path: format!("/{mode}"),
            reply_to: "/response".into(),
        }
    } else {
        AdminCommand::AdminStop {
            req_id: 71,
            target_path: format!("/{mode}"),
            reply_to: "/response".into(),
        }
    };
    if matches!(mode, 7 | 18) {
        let mut call = Box::pin(parrot_remote::admin::handle_admin_command(
            command, &local, &back,
        ));
        assert!(futures::poll!(&mut call).is_pending());
        drop(call);
        assert!(rx.try_recv().is_err());
        assert_eq!(back.is_closed(), mode == 7);
    } else {
        bounded(parrot_remote::admin::handle_admin_command(
            command, &local, &back,
        ))
        .await;
        let frame = rx.try_recv().expect("one terminal reply");
        assert_eq!(frame.header.correlation_id, 71);
        assert_eq!(frame.path, "/response");
        let SysEvent::AdminReply(reply) =
            parrot_remote::admin::decode_sys_event(&frame.payload).unwrap()
        else {
            panic!("wrong reply dialect")
        };
        assert_eq!(reply.req_id(), 71);
        if mode == 0 {
            assert!(matches!(
                reply,
                AdminReply::Stopped { .. } | AdminReply::Spawned { .. }
            ));
        } else {
            let AdminReply::Failed { code, .. } = reply else {
                panic!("cleanup failure acknowledged success")
            };
            assert_eq!(
                code,
                match mode {
                    15 => ErrCode::ActorNotFound as u16,
                    19 => ErrCode::NotRemotable as u16,
                    _ => ErrCode::ProtocolViolation as u16,
                }
            );
        }
        assert!(
            !back.is_closed(),
            "a complete failure reply should preserve a healthy connection"
        );
        assert!(rx.try_recv().is_err());
    }
    let invoked = if is_spawn {
        mode != 19
    } else {
        !matches!(mode, 8 | 9 | 15 | 17)
    };
    assert_eq!(state.starts.load(Ordering::SeqCst), usize::from(invoked));
    assert_eq!(
        state.work_drops.load(Ordering::SeqCst),
        usize::from(invoked && mode != 1)
    );
    assert_eq!(
        state.lookup_drops.load(Ordering::SeqCst),
        usize::from(!is_spawn && mode != 17)
    );
    let owner = if is_spawn {
        !matches!(mode, 1 | 2 | 3 | 6 | 7 | 18 | 19)
    } else {
        !matches!(mode, 8 | 15 | 17)
    };
    assert_eq!(state.owners.load(Ordering::SeqCst), usize::from(owner));
    assert_eq!(state.errors.load(Ordering::SeqCst), usize::from(mode == 6));
    check_payloads(&state);
}
struct Executor {
    state: Arc<State>,
    mode: u8,
    system: Weak<parrot_remote::RemoteActorSystem>,
}
impl Executor {
    fn begin(&self) -> Work {
        if let Some(system) = self.system.upgrade() {
            system.set_component_executor(None);
        }
        Work::new(self.state.clone(), self.mode)
    }
}
impl Drop for Executor {
    fn drop(&mut self) {
        self.state.owners.fetch_add(1, Ordering::SeqCst);
        if self.system.strong_count() > 0 && matches!(self.mode, 5 | 10 | 14) {
            fail(&self.state);
        }
    }
}
struct ExecuteFuture {
    work: Work,
    id: u64,
    operation: u8,
}
impl Future for ExecuteFuture {
    type Output = AdminReplyV2;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        std::task::ready!(self.work.step());
        let id = if self.work.mode == 13 {
            self.id + 1
        } else {
            self.id
        };
        Poll::Ready(if self.work.mode == 14 {
            AdminReplyV2::Failed {
                req_id: id,
                code: ErrCode::Overloaded as u16,
                detail: "business overloaded".into(),
            }
        } else {
            match self.operation {
                0 => AdminReplyV2::Deployed {
                    req_id: id,
                    instances: vec![],
                },
                1 => AdminReplyV2::Drained {
                    req_id: id,
                    drained: 0,
                    aborted: 0,
                },
                2 => AdminReplyV2::Stopped { req_id: id },
                3 => AdminReplyV2::Status {
                    req_id: id,
                    states: vec![],
                },
                _ => AdminReplyV2::Metrics {
                    req_id: id,
                    snapshot: Default::default(),
                },
            }
        })
    }
}
struct MetricsFuture(Work);
impl Future for MetricsFuture {
    type Output = parrot_remote::metrics::MeasuredMetrics;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        std::task::ready!(self.0.step());
        Poll::Ready(Default::default())
    }
}
impl Executor {
    fn execute(&self, id: u64, operation: u8) -> BoxedFuture<'_, AdminReplyV2> {
        Box::pin(ExecuteFuture {
            work: self.begin(),
            id,
            operation,
        })
    }
}
impl ComponentExecutor for Executor {
    fn deploy<'a, 'b, 'f>(
        &'a self,
        id: u64,
        _: &'b ComponentDeploy,
    ) -> BoxedFuture<'f, AdminReplyV2>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        self.execute(id, 0)
    }
    fn drain<'a, 'b, 'f>(&'a self, id: u64, _: &'b str, _: u64) -> BoxedFuture<'f, AdminReplyV2>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        self.execute(id, 1)
    }
    fn stop<'a, 'b, 'f>(&'a self, id: u64, _: &'b str) -> BoxedFuture<'f, AdminReplyV2>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        self.execute(id, 2)
    }
    fn status<'a, 'b, 'f>(&'a self, id: u64, _: &'b str) -> BoxedFuture<'f, AdminReplyV2>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        self.execute(id, 3)
    }
    fn metrics<'a, 'f>(&'a self, id: u64) -> BoxedFuture<'f, AdminReplyV2>
    where
        'a: 'f,
        Self: 'f,
    {
        self.execute(id, 4)
    }
    fn measured_metrics<'a, 'f>(
        &'a self,
    ) -> BoxedFuture<'f, parrot_remote::metrics::MeasuredMetrics>
    where
        'a: 'f,
        Self: 'f,
    {
        Box::pin(MetricsFuture(self.begin()))
    }
}
fn command(operation: u8) -> AdminCommandV2 {
    match operation {
        0 => AdminCommandV2::DeployComponent {
            req_id: 71,
            component: ComponentDeploy {
                name: "test".into(),
                version: "1".into(),
                artifact: parrot_remote::admin_v2::AdminArtifactRef::Props {
                    factory: "test".into(),
                },
                instances: parrot_remote::admin_v2::AdminInstancePolicy::Singleton,
                config: None,
            },
        },
        1 => AdminCommandV2::DrainComponent {
            req_id: 71,
            path_prefix: "/test".into(),
            timeout_ms: 100,
        },
        2 => AdminCommandV2::StopComponent {
            req_id: 71,
            path_prefix: "/test".into(),
        },
        3 => AdminCommandV2::ComponentStatus {
            req_id: 71,
            path_prefix: "/test".into(),
        },
        4 => AdminCommandV2::MetricsReport { req_id: 71 },
        _ => AdminCommandV2::MetricsReportV2 { req_id: 71 },
    }
}
async fn v2(mode: u8, operation: u8) {
    let state = STATE.get().unwrap().clone();
    let executor = Executor {
        state: state.clone(),
        mode,
        system: Weak::new(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let back = FrameSender::new(tx, "source");
    let future = parrot_remote::admin_v2::handle_admin_command_v2(
        command(operation),
        Some(&executor),
        &back,
        "/response",
    );
    if matches!(mode, 7 | 18) {
        let mut future = Box::pin(future);
        assert!(futures::poll!(&mut future).is_pending());
        drop(future);
        assert!(rx.try_recv().is_err());
        assert_eq!(back.is_closed(), mode == 7);
    } else {
        bounded(future).await;
        let frame = rx.try_recv().expect("one terminal reply");
        assert_eq!(frame.header.correlation_id, 71);
        assert_eq!(frame.path, "/response");
        let SysEvent::AdminReplyV2(reply) =
            parrot_remote::admin::decode_sys_event(&frame.payload).unwrap()
        else {
            panic!("wrong dialect")
        };
        assert_eq!(reply.req_id(), 71);
        if mode == 0 {
            assert!(!matches!(reply, AdminReplyV2::Failed { .. }));
        } else {
            let AdminReplyV2::Failed { code, detail, .. } = reply else {
                panic!("cleanup failure acknowledged success")
            };
            assert_eq!(
                code,
                if mode == 14 {
                    ErrCode::Overloaded
                } else {
                    ErrCode::ProtocolViolation
                } as u16
            );
            if mode == 14 {
                assert_eq!(detail, "business overloaded");
            }
        }
        assert!(!back.is_closed());
        assert!(rx.try_recv().is_err());
    }
    assert_eq!(state.starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.work_drops.load(Ordering::SeqCst),
        usize::from(mode != 1)
    );
    assert_eq!(
        state.owners.load(Ordering::SeqCst),
        0,
        "borrowed executor stays owned by caller"
    );
    check_payloads(&state);
}
async fn tcp_request(
    source: &Arc<parrot_remote::RemoteActorSystem>,
    mode: u8,
    operation: u8,
) -> Result<(), parrot_remote::RemoteError> {
    match operation {
        0 => {
            let AdminCommandV2::DeployComponent { component, .. } = command(0) else {
                unreachable!()
            };
            source
                .deploy_component("receiver", component)
                .await
                .map(|_| ())
        }
        1 => source
            .drain_component("receiver", "/test", Duration::from_millis(100))
            .await
            .map(|_| ()),
        2 => source.stop_component("receiver", "/test").await,
        3 => source
            .component_status("receiver", "/test")
            .await
            .map(|_| ()),
        4 => source.metrics_report("receiver").await.map(|_| ()),
        5 => source.measured_metrics("receiver").await.map(|_| ()),
        6 => source
            .spawn_named("receiver", &format!("/{mode}"), "admin-unwind-props")
            .await
            .map(|_| ()),
        _ => source.admin_stop("receiver", &format!("/{mode}")).await,
    }
}
async fn tcp(mode: u8, operation: u8) {
    use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig, RemoteError};
    let state = STATE.get().unwrap().clone();
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
    if operation < 6 {
        // Removing the registry owner inside the callback leaves the framework's
        // temporary Arc as the last owner. Its Drop must precede any success reply.
        target.set_component_executor(Some(Arc::new(Executor {
            state: state.clone(),
            mode,
            system: Arc::downgrade(&target),
        })));
    }
    if mode == 7 {
        let mut request = Box::pin(tcp_request(&source, mode, operation));
        bounded(async {
            loop {
                assert!(futures::poll!(&mut request).is_pending());
                if state.starts.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        // A caller dropping its wait is not a wire cancellation protocol.
        // Closing its physical connection is the cancellation event under test.
        drop(request);
        bounded(source.shutdown()).await.unwrap();
        bounded(async {
            while state.work_drops.load(Ordering::SeqCst) != 1
                || state.owners.load(Ordering::SeqCst) != 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(original.is_closed());
    } else {
        let result = bounded(tcp_request(&source, mode, operation)).await;
        if mode == 0 {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            if operation < 6 {
                let RemoteError::AdminRejected { code, detail, .. } = error else {
                    panic!("expected a truthful administrative failure reply, got {error:?}")
                };
                assert_eq!(
                    code,
                    if mode == 14 {
                        ErrCode::Overloaded
                    } else {
                        ErrCode::ProtocolViolation
                    } as u16
                );
                if mode == 14 {
                    assert_eq!(detail, "business overloaded");
                }
            } else {
                let code = ErrCode::ProtocolViolation as u16;
                assert!(
                    error.to_string().contains(&format!("failed: {code} ")),
                    "{error}"
                );
            }
        }
        let invoked = operation != 7 || !matches!(mode, 8 | 9);
        assert_eq!(state.starts.load(Ordering::SeqCst), usize::from(invoked));
        assert_eq!(
            state.work_drops.load(Ordering::SeqCst),
            usize::from(invoked && mode != 1)
        );
        let has_owner = operation < 6
            || (operation == 6 && !matches!(mode, 1 | 2 | 3 | 6))
            || (operation == 7 && mode != 8);
        assert_eq!(
            state.owners.load(Ordering::SeqCst),
            usize::from(has_owner),
            "all owned resources must retire before the reply"
        );
        assert_eq!(state.errors.load(Ordering::SeqCst), usize::from(mode == 6));
        assert_eq!(
            state.lookup_drops.load(Ordering::SeqCst),
            usize::from(operation == 7)
        );
        check_payloads(&state);
        assert!(!original.is_closed());
        bounded(source.admin_stop("receiver", "/0")).await.unwrap();
        assert!(source
            .links_snapshot()
            .await
            .into_iter()
            .find(|(id, _, _)| id == "receiver")
            .unwrap()
            .1
            .same_connection(&original));
    }
    bounded(other.admin_stop("receiver", "/0")).await.unwrap();
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
    check_payloads(&state);
}

#[derive(Clone, Copy)]
enum Case {
    Stop(u8),
    Spawn(u8),
    V2(u8, u8),
    Tcp(u8, u8),
}
async fn exercise(case: Case) {
    STATE.set(Arc::new(State::default())).unwrap();
    match case {
        Case::Stop(m) => v1(m, false).await,
        Case::Spawn(m) => v1(m, true).await,
        Case::V2(m, op) => v2(m, op).await,
        Case::Tcp(m, op) => tcp(m, op).await,
    }
}
fn run(name: &str, case: Case) {
    const CHILD: &str = "PARROT_ADMIN_UNWIND_CASE";
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
    let root = std::env::var_os("PARROT_ADMIN_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!(
        "admin-{}-{}-{name}.log",
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
contract!(v1_stop_healthy, Case::Stop(0));
contract!(v1_stop_constructor, Case::Stop(1));
contract!(v1_stop_poll_and_future_drop, Case::Stop(3));
contract!(v1_stop_reference_drop, Case::Stop(5));
contract!(v1_stop_error_format_and_drop, Case::Stop(6));
contract!(v1_stop_cancel_drop, Case::Stop(7));
contract!(v1_stop_lookup_poll_and_drop, Case::Stop(8));
contract!(v1_stop_lookup_cleanup_prevents_stop, Case::Stop(9));
contract!(v1_stop_future_and_reference_drop, Case::Stop(10));
contract!(v1_stop_business_error_and_reference, Case::Stop(11));
contract!(v1_stop_missing, Case::Stop(15));
contract!(v1_stop_lookup_constructor, Case::Stop(17));
contract!(v1_stop_healthy_cancel, Case::Stop(18));
contract!(v1_spawn_healthy, Case::Spawn(0));
contract!(v1_spawn_constructor, Case::Spawn(1));
contract!(v1_spawn_poll_and_future_drop, Case::Spawn(3));
contract!(v1_spawn_ready_future_drop, Case::Spawn(4));
contract!(v1_spawn_reference_drop, Case::Spawn(5));
contract!(v1_spawn_error_format_and_drop, Case::Spawn(6));
contract!(v1_spawn_cancel_drop, Case::Spawn(7));
contract!(v1_spawn_future_and_reference_drop, Case::Spawn(10));
contract!(v1_spawn_healthy_cancel, Case::Spawn(18));
contract!(v1_spawn_missing_factory, Case::Spawn(19));
contract!(v2_deploy_poll_and_drop, Case::V2(3, 0));
contract!(v2_drain_poll_and_drop, Case::V2(3, 1));
contract!(v2_stop_poll_and_drop, Case::V2(3, 2));
contract!(v2_status_poll_and_drop, Case::V2(3, 3));
contract!(v2_legacy_metrics_poll_and_drop, Case::V2(3, 4));
contract!(v2_measured_metrics_poll_and_drop, Case::V2(3, 5));
contract!(v2_stop_healthy, Case::V2(0, 2));
contract!(v2_stop_constructor, Case::V2(1, 2));
contract!(v2_stop_ready_future_drop, Case::V2(4, 2));
contract!(v2_stop_cancel_drop, Case::V2(7, 2));
contract!(v2_stop_wrong_id, Case::V2(13, 2));
contract!(v2_stop_stable_business_failure, Case::V2(14, 2));
contract!(v2_stop_healthy_cancel, Case::V2(18, 2));

contract!(tcp_deploy_healthy, Case::Tcp(0, 0));
contract!(tcp_deploy_poll_and_future_drop, Case::Tcp(3, 0));
contract!(tcp_deploy_executor_drop, Case::Tcp(5, 0));
contract!(tcp_deploy_future_and_executor_drop, Case::Tcp(10, 0));
contract!(tcp_drain_healthy, Case::Tcp(0, 1));
contract!(tcp_drain_poll_and_future_drop, Case::Tcp(3, 1));
contract!(tcp_drain_executor_drop, Case::Tcp(5, 1));
contract!(tcp_drain_future_and_executor_drop, Case::Tcp(10, 1));
contract!(tcp_stop_healthy, Case::Tcp(0, 2));
contract!(tcp_stop_poll_and_future_drop, Case::Tcp(3, 2));
contract!(tcp_stop_executor_drop, Case::Tcp(5, 2));
contract!(tcp_stop_future_and_executor_drop, Case::Tcp(10, 2));
contract!(tcp_status_healthy, Case::Tcp(0, 3));
contract!(tcp_status_poll_and_future_drop, Case::Tcp(3, 3));
contract!(tcp_status_executor_drop, Case::Tcp(5, 3));
contract!(tcp_status_future_and_executor_drop, Case::Tcp(10, 3));
contract!(tcp_metrics_healthy, Case::Tcp(0, 4));
contract!(tcp_metrics_poll_and_future_drop, Case::Tcp(3, 4));
contract!(tcp_metrics_executor_drop, Case::Tcp(5, 4));
contract!(tcp_metrics_future_and_executor_drop, Case::Tcp(10, 4));
contract!(tcp_measured_healthy, Case::Tcp(0, 5));
contract!(tcp_measured_poll_and_future_drop, Case::Tcp(3, 5));
contract!(tcp_measured_executor_drop, Case::Tcp(5, 5));
contract!(tcp_measured_future_and_executor_drop, Case::Tcp(10, 5));
contract!(tcp_stop_constructor, Case::Tcp(1, 2));
contract!(tcp_stop_wrong_id, Case::Tcp(13, 2));
contract!(
    tcp_stop_business_failure_and_executor_drop,
    Case::Tcp(14, 2)
);
contract!(tcp_stop_cancel, Case::Tcp(7, 2));
contract!(tcp_measured_cancel, Case::Tcp(7, 5));
contract!(tcp_spawn_mode_3, Case::Tcp(3, 6));
contract!(tcp_spawn_mode_5, Case::Tcp(5, 6));
contract!(tcp_spawn_mode_6, Case::Tcp(6, 6));
contract!(tcp_spawn_mode_10, Case::Tcp(10, 6));
contract!(tcp_admin_stop_mode_3, Case::Tcp(3, 7));
contract!(tcp_admin_stop_mode_5, Case::Tcp(5, 7));
contract!(tcp_admin_stop_mode_6, Case::Tcp(6, 7));
contract!(tcp_admin_stop_mode_10, Case::Tcp(10, 7));
contract!(tcp_admin_stop_mode_8, Case::Tcp(8, 7));
contract!(tcp_admin_stop_mode_9, Case::Tcp(9, 7));
