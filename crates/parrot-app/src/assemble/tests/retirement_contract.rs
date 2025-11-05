//! Controlled retirement/hook interleavings; no timing-based winner selection.
use super::*;
use parrot_api::errors::ActorError;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug)]
struct State {
    live: AtomicBool,
    stops: AtomicUsize,
    sends: AtomicUsize,
    stop_result: AtomicUsize,
    send_result: AtomicUsize,
    release: tokio::sync::Semaphore,
}
impl State {
    fn new(live: bool) -> Arc<Self> {
        Arc::new(Self {
            live: AtomicBool::new(live),
            stops: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            stop_result: AtomicUsize::new(0),
            send_result: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
        })
    }
}
#[derive(Debug, Clone)]
struct RetiringRef(Arc<State>);
impl ActorRef for RetiringRef {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0.sends.fetch_add(1, Ordering::SeqCst);
            let mode = self.0.send_result.load(Ordering::SeqCst);
            if mode != 0 {
                self.0.live.store(false, Ordering::SeqCst);
            }
            match mode {
                0 => Ok(message),
                1 => Err(ActorError::ReplyChannelError(
                    "closed after liveness observation".into(),
                )),
                2 => Err(ActorError::MessageHandlingError(
                    "hook actually failed".into(),
                )),
                3 => Ok(Box::new(ParrotAppHook(99)) as BoxedMessage),
                _ => panic!("hook callback panic"),
            }
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Err(ActorError::Stopped) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.stops.fetch_add(1, Ordering::SeqCst);
            let _permit = self.0.release.acquire().await.unwrap();
            match self.0.stop_result.load(Ordering::SeqCst) {
                0 => Ok(()),
                1 => Err(ActorError::InternalError("owned destructor failed".into())),
                2 => panic!("owned stop callback panic"),
                _ => Err(ActorError::ActorNotFound(
                    "same path is not a retirement receipt".into(),
                )),
            }
        })
    }
    fn path(&self) -> String {
        "/user/a".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { self.0.live.load(Ordering::SeqCst) })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
async fn prepared(state: &Arc<State>, host: &TestHost) -> AssemblingContext {
    let mut component = comp("a", &[]);
    component.hooks.on_drain = Some(".".into());
    component.hooks.on_stop = Some(".".into());
    let manifest = manifest(vec![component]);
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &host.deployer(),
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap();
    context.registry.get_mut("a").unwrap()[0] = Box::new(RetiringRef(state.clone()));
    context.hook_targets.insert(
        ("a".into(), 0, "/user/a".into()),
        Box::new(RetiringRef(state.clone())),
    );
    context
}
#[tokio::test]
async fn closed_mailbox_drain_joins_real_retirement_before_success() {
    let state = State::new(false);
    let host = make_deployer(&Journal::new());
    let deployer = host.deployer();
    let mut context = prepared(&state, &host).await;
    let mut work = Box::pin(context.drain(&deployer));
    assert!(
        work.as_mut().now_or_never().is_none(),
        "closed mailbox is not retired"
    );
    assert_eq!(state.stops.load(Ordering::SeqCst), 1);
    state.release.add_permits(1);
    work.await.unwrap();
    assert!(context.completed_terminations.contains(&("a".into(), 0)));
    assert!(!context
        .completed_hooks
        .contains(&("a".into(), 0, hook_phase::DRAIN)));
    context.teardown(&deployer).await.unwrap();
    assert_eq!(state.stops.load(Ordering::SeqCst), 1);
    assert_eq!(state.sends.load(Ordering::SeqCst), 0);
}
async fn rejected_retirement(mode: usize, message: &str) {
    let state = State::new(false);
    state.stop_result.store(mode, Ordering::SeqCst);
    state.release.add_permits(1);
    let host = make_deployer(&Journal::new());
    let deployer = host.deployer();
    let mut context = prepared(&state, &host).await;
    let error = context.drain(&deployer).await.unwrap_err();
    assert!(error.to_string().contains(message), "{error}");
    assert!(context.completed_terminations.is_empty());
    assert!(context.completed_hooks.is_empty());
    assert_eq!(context.started(), &["a"]);
    state.stop_result.store(0, Ordering::SeqCst);
    context.drain(&deployer).await.unwrap();
    context.teardown(&deployer).await.unwrap();
    assert_eq!(state.stops.load(Ordering::SeqCst), 2);
    assert_eq!(state.sends.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn cleanup_failure_is_not_a_successful_drain_and_can_be_retried() {
    rejected_retirement(1, "owned destructor failed").await;
}
#[tokio::test]
async fn stop_callback_panic_is_not_a_successful_drain() {
    rejected_retirement(2, "hook panic").await;
}
#[tokio::test]
async fn missing_path_is_not_proof_of_owned_retirement() {
    rejected_retirement(3, "same path is not a retirement receipt").await;
}
#[tokio::test]
async fn cancelled_drain_does_not_record_unfinished_retirement() {
    let state = State::new(false);
    let host = make_deployer(&Journal::new());
    let deployer = host.deployer();
    let mut context = prepared(&state, &host).await;
    let mut work = Box::pin(context.drain(&deployer));
    assert!(work.as_mut().now_or_never().is_none());
    drop(work);
    assert!(context.completed_terminations.is_empty());
    assert!(context.completed_hooks.is_empty());
    state.release.add_permits(1);
    context.drain(&deployer).await.unwrap();
    context.teardown(&deployer).await.unwrap();
    assert_eq!(state.stops.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn retirement_deadline_is_not_acknowledged_and_retry_still_joins() {
    let state = State::new(false);
    let host = make_deployer(&Journal::new());
    let deployer = host.deployer();
    let mut context = prepared(&state, &host).await;
    context.hooks_timeout = Duration::from_millis(5);
    assert!(matches!(
        context.drain(&deployer).await,
        Err(DeployError::HookTimeout { .. })
    ));
    assert!(context.completed_terminations.is_empty());
    assert!(context.completed_hooks.is_empty());
    state.release.add_permits(1);
    context.drain(&deployer).await.unwrap();
    context.teardown(&deployer).await.unwrap();
    assert_eq!(state.stops.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn acknowledged_live_hook_keeps_actor_alive_until_teardown() {
    let state = State::new(true);
    let host = make_deployer(&Journal::new());
    let deployer = host.deployer();
    let mut context = prepared(&state, &host).await;
    context.drain(&deployer).await.unwrap();
    assert_eq!(state.stops.load(Ordering::SeqCst), 0);
    assert!(context.completed_terminations.is_empty());
    assert!(context
        .completed_hooks
        .contains(&("a".into(), 0, hook_phase::DRAIN)));
    state.release.add_permits(1);
    context.teardown(&deployer).await.unwrap();
    assert_eq!(state.sends.load(Ordering::SeqCst), 2);
    assert_eq!(state.stops.load(Ordering::SeqCst), 1);
}
async fn rejected_hook(mode: usize, message: &str) {
    let state = State::new(true);
    state.send_result.store(mode, Ordering::SeqCst);
    let host = make_deployer(&Journal::new());
    let deployer = host.deployer();
    let mut context = prepared(&state, &host).await;
    let error = context.drain(&deployer).await.unwrap_err();
    assert!(error.to_string().contains(message), "{error}");
    assert!(!state.live.load(Ordering::SeqCst));
    assert_eq!(state.stops.load(Ordering::SeqCst), 0);
    assert!(context.completed_hooks.is_empty());
    assert!(context.completed_terminations.is_empty());
    state.release.add_permits(1);
    context.teardown(&deployer).await.unwrap();
}
#[tokio::test]
async fn reply_loss_after_liveness_check_is_not_a_hook_acknowledgement() {
    rejected_hook(1, "closed after liveness observation").await;
}
#[tokio::test]
async fn handler_error_is_preserved_even_when_instance_then_closes() {
    rejected_hook(2, "hook actually failed").await;
}
#[tokio::test]
async fn invalid_ack_is_preserved_even_when_instance_then_closes() {
    rejected_hook(3, "expected phase").await;
}
#[tokio::test]
async fn hook_panic_is_preserved_even_when_instance_then_closes() {
    rejected_hook(4, "hook panic").await;
}
#[tokio::test]
async fn unreachable_gateway_is_not_joined_as_a_local_retirement() {
    let state = State::new(false);
    state.send_result.store(1, Ordering::SeqCst);
    let host = make_deployer(&Journal::new());
    let deployer = host.deployer();
    let mut context = prepared(&state, &host).await;
    context.specs.get_mut("a").unwrap().engine = EngineKind::Akka;
    assert!(context.drain(&deployer).await.is_err());
    assert_eq!(state.stops.load(Ordering::SeqCst), 0);
    assert_eq!(state.sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn start_hook_never_substitutes_retirement_for_initialization() {
    let state = State::new(false);
    state.send_result.store(1, Ordering::SeqCst);
    let host = make_deployer(&Journal::new());
    let deployer = host.deployer();
    let mut context = prepared(&state, &host).await;
    context.specs.get_mut("a").unwrap().hooks.on_start = Some(".".into());
    assert!(context
        .run_hooks("a", hook_phase::START, &deployer)
        .await
        .is_err());
    assert_eq!(state.stops.load(Ordering::SeqCst), 0);
    assert_eq!(state.sends.load(Ordering::SeqCst), 1);
}
