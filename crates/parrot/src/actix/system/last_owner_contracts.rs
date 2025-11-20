use super::*;
use std::sync::atomic::AtomicBool;
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(8);

#[derive(Default)]
struct State {
    stopping: AtomicBool,
    open: AtomicBool,
    disposed: AtomicBool,
    changed: Notify,
}
impl State {
    fn release(&self) {
        self.open.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
    async fn wait(&self, condition: impl Fn() -> bool) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if condition() {
                return;
            }
            changed.await;
        }
    }
}
struct Release(Arc<State>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Actor(Arc<State>);
impl Drop for Actor {
    fn drop(&mut self) {
        self.0.disposed.store(true, Ordering::Release);
        self.0.changed.notify_waiters();
    }
}
impl ActixEngineExt for Actor {}
impl ParrotActor for Actor {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.stopping.store(true, Ordering::Release);
            self.0.changed.notify_waiters();
            self.0.wait(|| self.0.open.load(Ordering::Acquire)).await;
            Ok(())
        })
    }
}

async fn contract(own_worker: bool, concurrent: bool, facade_graph: u8) {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let state = Arc::new(State::default());
    let _release = Release(state.clone());
    let actor = system
        .spawn_root_typed(Actor(state.clone()), Default::default())
        .await
        .unwrap();
    actor.send(Box::new(41_u8)).await.unwrap();
    // Independent observations retain resources, never a public system lease.
    let pool = system.arbiters.clone();
    let registry = system.registry.clone();
    let weak = system.downgrade();
    if facade_graph != 0 {
        let facade = crate::system::ParrotActorSystem::new(Default::default())
            .await
            .unwrap();
        facade
            .register_actix_system("primary".into(), system.clone(), true)
            .await
            .unwrap();
        facade
            .register_actix_system("alias".into(), system.clone(), false)
            .await
            .unwrap();
        if facade_graph == 2 {
            facade
                .register_actix_system(
                    "primary".into(),
                    ActixActorSystem::with_arbiter_count(1).await.unwrap(),
                    true,
                )
                .await
                .unwrap();
        }
        drop(system);
        assert!(
            !registry.read().unwrap().shutting_down,
            "facade alias lost its engine"
        );
        actor.send(Box::new(43_u8)).await.unwrap();
        drop(facade);
    } else if own_worker {
        let (done, received) = tokio::sync::oneshot::channel();
        assert!(pool.inner.workers[0].spawn(async move {
            drop(system);
            let _ = done.send(());
        }));
        tokio::time::timeout(WAIT, received).await.unwrap().unwrap();
    } else if concurrent {
        let other = system.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let peer = barrier.clone();
        let thread = std::thread::spawn(move || {
            peer.wait();
            drop(other);
        });
        barrier.wait();
        drop(system);
        thread.join().unwrap();
    } else {
        let other = system.clone();
        drop(other);
        assert!(
            !registry.read().unwrap().shutting_down,
            "nonfinal lease closed admission"
        );
        drop(system);
    }
    observe(pool, registry, weak, actor, state).await;
}

async fn observe(
    pool: ArbiterPool,
    registry: Arc<RwLock<Registry>>,
    weak: WeakSystem,
    actor: BoxedActorRef,
    state: Arc<State>,
) {
    let closed = registry.read().unwrap().shutting_down;
    let resurrected = weak.upgrade().is_ok();
    let stop_entered =
        tokio::time::timeout(WAIT, state.wait(|| state.stopping.load(Ordering::Acquire)))
            .await
            .is_ok();
    let workers_during_hook = pool.inner.retirement.owned_threads(1);
    let disposed_early = state.disposed.load(Ordering::Acquire);
    state.release();
    let drained = tokio::time::timeout(WAIT, async {
        loop {
            if registry.read().unwrap().stopped {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let threads_after_drain = pool.inner.retirement.owned_threads(1);
    // Finish the old candidate before asserting, to keep baseline failures clean.
    let _ = tokio::time::timeout(WAIT, actor.stop()).await;
    let _ = tokio::time::timeout(WAIT, pool.stop_and_join()).await;
    assert!(closed, "last system owner left admission open");
    assert!(!resurrected, "weak context resurrected a released system");
    assert!(stop_entered, "last owner skipped graceful actor retirement");
    assert!(
        !disposed_early,
        "actor disposed before the stop hook released"
    );
    assert_eq!(
        workers_during_hook, 2,
        "workers retired before actor cleanup"
    );
    assert!(
        drained,
        "implicit shutdown never completed the shared drain"
    );
    assert_eq!(
        threads_after_drain, 0,
        "drain preceded actual worker/helper joins"
    );
    assert!(state.disposed.load(Ordering::Acquire));
    assert_eq!(pool.inner.retirement.owned_threads(1), 0);
}

#[actix::test]
async fn last_system_owner_drains_actor_before_stopping_workers() {
    contract(false, false, 0).await;
}

#[actix::test]
async fn last_system_owner_on_own_worker_hands_off_without_self_join() {
    contract(true, false, 0).await;
}

#[actix::test]
async fn concurrent_final_system_owners_close_one_shared_drain() {
    contract(false, true, 0).await;
}

#[actix::test]
async fn facade_final_alias_starts_one_native_drain() {
    contract(false, false, 1).await;
}

#[actix::test]
async fn facade_replacement_preserves_old_alias_until_last_owner() {
    contract(false, false, 2).await;
}

struct Local {
    value: std::rc::Rc<u8>,
    owner: std::thread::ThreadId,
    state: Arc<State>,
}
impl Drop for Local {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
        self.state.disposed.store(true, Ordering::Release);
        self.state.changed.notify_waiters();
    }
}
struct Query;
impl parrot_api::Message for Query {
    type Result = u8;
}
impl parrot_api::typed::ParrotMsgVariant<parrot_api::local_typed::LocalProtocol<Local>> for Query {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: u8) -> ActorResult<u8> {
        Ok(reply)
    }
}
impl parrot_api::local_typed::LocalTypedDispatch for Local {
    type Msg = Query;
    type Reply = u8;
    fn dispatch<'a>(
        &'a mut self,
        _: Query,
        _: &'a parrot_api::typed_context::TypedContext,
    ) -> parrot_api::local_typed::LocalBoxedFuture<'a, ActorResult<u8>> {
        Box::pin(async move {
            let value = self.value.clone();
            tokio::task::yield_now().await;
            assert_eq!(self.owner, std::thread::current().id());
            Ok(*value)
        })
    }
    fn before_stop<'a>(
        &'a mut self,
        _: &'a parrot_api::typed_context::TypedContext,
    ) -> parrot_api::local_typed::LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let value = self.value.clone();
            self.state.stopping.store(true, Ordering::Release);
            self.state.changed.notify_waiters();
            self.state
                .wait(|| self.state.open.load(Ordering::Acquire))
                .await;
            assert_eq!(self.owner, std::thread::current().id());
            assert!(std::rc::Rc::ptr_eq(&value, &self.value));
            Ok(())
        })
    }
}

async fn local_contract(facade_owner: bool) {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let state = Arc::new(State::default());
    let _release = Release(state.clone());
    let input = state.clone();
    let actor = system
        .spawn_local_typed::<Local, Query, _>(
            move || Local {
                value: std::rc::Rc::new(47),
                owner: std::thread::current().id(),
                state: input.clone(),
            },
            "/local",
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Query).await.unwrap(), 47);
    let pool = system.arbiters.clone();
    let registry = system.registry.clone();
    let weak = system.downgrade();
    if facade_owner {
        let facade = crate::system::ParrotActorSystem::new(Default::default())
            .await
            .unwrap();
        facade
            .register_actix_system("local".into(), system.as_ref().clone(), true)
            .await
            .unwrap();
        drop(system);
        assert_eq!(actor.ask(Query).await.unwrap(), 47);
        drop(facade);
    } else {
        drop(system);
    }
    observe(pool, registry, weak, actor.into_dyn(), state).await;
}

#[actix::test]
async fn local_last_owner_keeps_non_send_stop_future_on_worker() {
    local_contract(false).await;
}

#[actix::test]
async fn local_facade_last_owner_keeps_non_send_stop_future_on_worker() {
    local_contract(true).await;
}

#[actix::test]
async fn upgraded_weak_handle_is_a_lease_until_its_final_release() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let registry = system.registry.clone();
    let pool = system.arbiters.clone();
    let weak = system.downgrade();
    let upgraded = weak.upgrade().unwrap();
    drop(system);
    assert!(!registry.read().unwrap().shutting_down);
    drop(upgraded);
    assert!(registry.read().unwrap().shutting_down);
    assert!(matches!(weak.upgrade(), Err(ActorError::Stopped)));
    let receipt = registry.read().unwrap().shutdown.clone().unwrap();
    tokio::time::timeout(WAIT, await_completion(receipt))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pool.inner.retirement.owned_threads(1), 0);
}
