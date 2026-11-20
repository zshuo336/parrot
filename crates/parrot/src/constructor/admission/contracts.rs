use super::*;
use crate::{
    children::recovery::{Recovery, Step},
    constructor::Factory,
};
use std::{
    sync::{
        Condvar,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(5);

fn factory(
    workers: &Arc<Semaphore>,
    admission: &Arc<Admission>,
    calls: &Arc<AtomicUsize>,
) -> Arc<Factory<usize>> {
    let calls = calls.clone();
    Factory::with_admission(
        move || calls.fetch_add(1, Ordering::SeqCst),
        workers.clone(),
        Arc::new(Semaphore::new(1)),
        admission.clone(),
    )
}

fn group(count: usize) -> (Arc<Recovery>, Vec<Uuid>) {
    let ids: Vec<_> = (0..count).map(|_| Uuid::new_v4()).collect();
    (
        Recovery::new(
            Uuid::new_v4(),
            ids.clone(),
            tokio::time::Instant::now() + WAIT,
            Box::new(|_| {}),
        ),
        ids,
    )
}

async fn phase(group: &Arc<Recovery>, ids: &[Uuid], step: Step) {
    let results = futures::future::join_all(ids.iter().map(|id| group.arrive(*id, step))).await;
    for result in results {
        result.unwrap();
    }
}

#[tokio::test]
async fn distinct_factories_share_staged_ownership_and_cancel_before_invocation() {
    let workers = Arc::new(Semaphore::new(1));
    let admission = Admission::new(2);
    let calls = Arc::new(AtomicUsize::new(0));
    let factories: Vec<_> = (0..3)
        .map(|_| factory(&workers, &admission, &calls))
        .collect();
    let first = factories[0].prepare::<()>(Arc::default()).await.unwrap();
    let second = factories[1].prepare::<()>(Arc::default()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(admission.available_permits(), 0);
    let control = Arc::new(ConstructionControl::default());
    let third = factories[2].prepare::<()>(control.clone());
    tokio::pin!(third);
    assert!(futures::poll!(&mut third).is_pending());
    assert_eq!(factories[2].state.lock().unwrap().active, 0);
    control.cancel();
    assert!(matches!(third.await, Err(ConstructionError::Cancelled)));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    first.discard().await.unwrap();
    let third = factories[2].prepare::<()>(Arc::default()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    second.discard().await.unwrap();
    third.discard().await.unwrap();
    for factory in &factories {
        factory.retire().await.unwrap();
    }
    assert_eq!(admission.available_permits(), 2);
}

#[tokio::test]
async fn competing_groups_reserve_atomically_and_finish_with_one_worker() {
    let workers = Arc::new(Semaphore::new(1));
    let admission = Admission::new(3);
    let calls = Arc::new(AtomicUsize::new(0));
    let factories: Vec<_> = (0..4)
        .map(|_| factory(&workers, &admission, &calls))
        .collect();
    let (first_group, first_ids) = group(2);
    let (second_group, second_ids) = group(2);
    phase(&first_group, &first_ids, Step::Retired).await;
    phase(&second_group, &second_ids, Step::Retired).await;
    let controls: Vec<_> = first_ids
        .iter()
        .map(|id| {
            first_group
                .creation_control(*id, first_group.deadline)
                .unwrap()
        })
        .collect();
    let later: Vec<_> = second_ids
        .iter()
        .map(|id| {
            second_group
                .creation_control(*id, second_group.deadline)
                .unwrap()
        })
        .collect();
    let first = factories[0]
        .prepare::<()>(controls[0].clone())
        .await
        .unwrap();
    assert_eq!(admission.available_permits(), 1);
    let blocked = factories[2].prepare::<()>(later[0].clone());
    tokio::pin!(blocked);
    assert!(futures::poll!(&mut blocked).is_pending());
    // Fair acquire_many earmarks the spare permit without admitting the batch.
    assert_eq!(admission.available_permits(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // A queued second group must not strand the first group's missing sibling.
    let sibling = tokio::time::timeout(WAIT, factories[1].prepare::<()>(controls[1].clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    phase(&first_group, &first_ids, Step::Prepared).await;
    assert_eq!((first.adopt(), sibling.adopt()), (0, 1));
    assert_eq!(admission.available_permits(), 0);
    phase(&first_group, &first_ids, Step::Published).await;
    let third = tokio::time::timeout(WAIT, blocked).await.unwrap().unwrap();
    let fourth = factories[3].prepare::<()>(later[1].clone()).await.unwrap();
    phase(&second_group, &second_ids, Step::Prepared).await;
    assert_eq!((third.adopt(), fourth.adopt()), (2, 3));
    phase(&second_group, &second_ids, Step::Published).await;
    for factory in &factories {
        factory.retire().await.unwrap();
    }
    assert_eq!(admission.available_permits(), 3);
}

#[tokio::test]
async fn cancelled_group_releases_queued_batch_without_constructing() {
    let workers = Arc::new(Semaphore::new(1));
    let admission = Admission::new(2);
    let calls = Arc::new(AtomicUsize::new(0));
    let first = factory(&workers, &admission, &calls);
    let second = factory(&workers, &admission, &calls);
    let held = first.prepare::<()>(Arc::default()).await.unwrap();
    let (group, ids) = group(2);
    phase(&group, &ids, Step::Retired).await;
    let control = group.creation_control(ids[0], group.deadline).unwrap();
    let waiting = second.prepare::<()>(control);
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    group.cancel("cancel batch waiting for shared capacity");
    assert!(matches!(waiting.await, Err(ConstructionError::Cancelled)));
    for id in ids {
        group.retired_after_abort(id);
    }
    let independent = second.prepare::<()>(Arc::default()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    independent.discard().await.unwrap();
    held.discard().await.unwrap();
    first.retire().await.unwrap();
    second.retire().await.unwrap();
    assert_eq!(admission.available_permits(), 2);
}

#[tokio::test]
async fn recovery_member_cannot_duplicate_its_reserved_invocation() {
    let workers = Arc::new(Semaphore::new(1));
    let admission = Admission::new(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = factory(&workers, &admission, &calls);
    let (group, ids) = group(1);
    phase(&group, &ids, Step::Retired).await;
    let control = group.creation_control(ids[0], group.deadline).unwrap();
    assert!(group.creation_control(ids[0], group.deadline).is_err());
    let output = factory.prepare::<()>(control.clone()).await.unwrap();
    let descendant = control.bounded(group.deadline);
    assert!(matches!(
        factory.prepare::<()>(descendant).await,
        Err(ConstructionError::WorkerFailed(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    output.discard().await.unwrap();
    group.cancel("end duplicate construction probe");
    group.retired_after_abort(ids[0]);
    factory.retire().await.unwrap();
    assert_eq!(admission.available_permits(), 1);
}

#[tokio::test]
async fn oversized_group_fails_before_factory_execution() {
    let workers = Arc::new(Semaphore::new(1));
    let admission = Admission::new(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = factory(&workers, &admission, &calls);
    let (group, ids) = group(2);
    phase(&group, &ids, Step::Retired).await;
    let control = group.creation_control(ids[0], group.deadline).unwrap();
    assert!(matches!(
        factory.prepare::<()>(control).await,
        Err(ConstructionError::WorkerFailed(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(admission.available_permits(), 1);
    group.abort("oversized group rejected");
    for id in ids {
        group.retired_after_abort(id);
    }
    factory.retire().await.unwrap();
}

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(*open, "test must release constructor or output destructor");
    }
}
struct Open(Arc<Gate>);
impl Drop for Open {
    fn drop(&mut self) {
        *self.0.open.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
struct Output(Arc<Gate>);
impl Drop for Output {
    fn drop(&mut self) {
        self.0.block();
    }
}

#[tokio::test]
async fn released_batch_cache_keeps_capacity_until_actual_output_disposal() {
    let admission = Admission::new(1);
    let batch = Batch::new(1);
    let parent = Arc::new(ConstructionControl::default());
    let control = parent.for_recovery_member(tokio::time::Instant::now() + WAIT, batch.clone());
    let gate = Arc::new(Gate::default());
    let release = Open(gate.clone());
    let captured = gate.clone();
    let factory = Factory::with_admission(
        move || Output(captured.clone()),
        Arc::new(Semaphore::new(1)),
        Arc::new(Semaphore::new(1)),
        admission.clone(),
    );
    let output = factory.prepare::<()>(control).await.unwrap();
    let disposal = tokio::spawn(output.discard());
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .unwrap();
    batch.release();
    assert_eq!(admission.available_permits(), 0);
    let next = factory.prepare::<()>(Arc::default());
    tokio::pin!(next);
    assert!(futures::poll!(&mut next).is_pending());
    drop(release);
    disposal.await.unwrap().unwrap();
    let output = tokio::time::timeout(WAIT, next).await.unwrap().unwrap();
    output.discard().await.unwrap();
    factory.retire().await.unwrap();
    assert_eq!(admission.available_permits(), 1);
}

#[derive(Clone)]
enum Engine {
    Thread(Arc<crate::thread::ThreadActorSystem>),
    Actix(Arc<crate::actix::ActixActorSystem>),
}
impl Engine {
    async fn dynamic(
        &self,
        gate: Arc<Gate>,
    ) -> parrot_api::types::ActorResult<parrot_api::types::BoxedActorRef> {
        match self {
            Self::Thread(system) => system
                .spawn_supervised_with_config(
                    move || {
                        gate.block();
                        ThreadEcho
                    },
                    "/shared-invocation-dynamic",
                    Default::default(),
                )
                .await
                .map(|reference| Box::new(reference) as parrot_api::types::BoxedActorRef)
                .map_err(|error| parrot_api::errors::ActorError::InternalError(error.to_string())),
            Self::Actix(system) => {
                system
                    .spawn_root_factory(move || {
                        gate.block();
                        ActixEcho
                    })
                    .await
            }
        }
    }
    async fn static_business(
        &self,
        calls: Arc<AtomicUsize>,
    ) -> parrot_api::types::ActorResult<usize> {
        let source = move || {
            calls.fetch_add(1, Ordering::SeqCst);
            StaticEcho
        };
        match self {
            Self::Thread(system) => {
                system
                    .spawn_typed_factory::<_, Request, _>(source, "/shared-invocation-static")
                    .await?
                    .ask(Request)
                    .await
            }
            Self::Actix(system) => {
                system
                    .spawn_typed_factory::<_, Request, _>(source, "/shared-invocation-static")
                    .await?
                    .ask(Request)
                    .await
            }
        }
    }
}

struct ThreadEcho;
struct ActixEcho;
macro_rules! echo {
    ($name:ident, $context:ty) => {
        impl parrot_api::actor::Actor for $name {
            type Config = parrot_api::actor::EmptyConfig;
            type Context = $context;
            fn state(&self) -> parrot_api::actor::ActorState {
                parrot_api::actor::ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                message: parrot_api::types::BoxedMessage,
                _: &'a mut Self::Context,
            ) -> parrot_api::types::BoxedFuture<
                'a,
                parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
            > {
                Box::pin(async move { Ok(message) })
            }
        }
    };
}
echo!(ThreadEcho, crate::thread::ThreadContext<Self>);
echo!(
    ActixEcho,
    crate::actix::ActixContext<crate::actix::ActixActor<Self>>
);
impl parrot_api::ActixEngineExt for ActixEcho {}
struct Request;
impl parrot_api::message::Message for Request {
    type Result = usize;
}
struct StaticEcho;
impl parrot_api::typed::ParrotTypedDispatch for StaticEcho {
    type Msg = Request;
    type Reply = usize;
    fn dispatch(
        &mut self,
        _: Request,
    ) -> parrot_api::types::BoxedFuture<'_, parrot_api::types::ActorResult<usize>> {
        Box::pin(async { Ok(42) })
    }
}
impl parrot_api::typed::ParrotMsgVariant<StaticEcho> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: usize) -> parrot_api::types::ActorResult<usize> {
        Ok(reply)
    }
}

#[test]
fn native_dynamic_and_static_roots_share_one_budget_on_both_engines() {
    actix::System::new().block_on(async {
        for actix in [false, true] {
            let admission = Admission::new(1);
            let engine = if actix {
                let mut system = crate::actix::ActixActorSystem::with_settings(
                    crate::actix::ActixSystemSettings {
                        arbiter_count: 2,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                system.factory_admission = admission.clone();
                Engine::Actix(Arc::new(system))
            } else {
                let mut system =
                    crate::thread::ThreadActorSystem::new(crate::thread::ThreadActorSystemConfig {
                        shared_pool_size: 2,
                        shared_burst_workers_max: 0,
                        ..Default::default()
                    });
                system.factory_admission = admission.clone();
                let system = Arc::new(system);
                system.set_self_weak(Arc::downgrade(&system));
                Engine::Thread(system)
            };
            let gate = Arc::new(Gate::default());
            let release = Open(gate.clone());
            let first_engine = engine.clone();
            let first_gate = gate.clone();
            let first = tokio::spawn(async move { first_engine.dynamic(first_gate).await });
            tokio::time::timeout(WAIT, gate.entered.notified())
                .await
                .unwrap();
            assert_eq!(admission.available_permits(), 0);
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let second_engine = engine.clone();
            let second = tokio::spawn(async move { second_engine.static_business(observed).await });
            tokio::time::sleep(Duration::from_millis(50)).await;
            let premature_calls = calls.load(Ordering::SeqCst);
            drop(release);
            let reference = tokio::time::timeout(WAIT, first)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                tokio::time::timeout(WAIT, second)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                42
            );
            let reply = tokio::time::timeout(WAIT, reference.send(Box::new(17usize)))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(*reply.downcast::<usize>().unwrap(), 17);
            assert_eq!(
                premature_calls, 0,
                "static root bypassed dynamic root's shared invocation budget; actix={actix}"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(admission.available_permits(), 1);
            match engine {
                Engine::Thread(system) => system.shutdown_internal().await.unwrap(),
                Engine::Actix(system) => Arc::try_unwrap(system)
                    .ok()
                    .expect("test owns the final Arc system handle")
                    .shutdown()
                    .await
                    .unwrap(),
            }
        }
    });
}

#[tokio::test]
async fn full_system_sized_group_prepares_with_one_worker_and_releases_on_commit() {
    let admission = Admission::new(SYSTEM_CAPACITY);
    let workers = Arc::new(Semaphore::new(1));
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = factory(&workers, &admission, &calls);
    let ids: Vec<_> = (0..SYSTEM_CAPACITY).map(|_| Uuid::new_v4()).collect();
    let group = Recovery::new(
        Uuid::new_v4(),
        ids.clone(),
        tokio::time::Instant::now() + Duration::from_secs(30),
        Box::new(|_| {}),
    );
    phase(&group, &ids, Step::Retired).await;
    let mut outputs = Vec::new();
    for id in &ids {
        let control = group.creation_control(*id, group.deadline).unwrap();
        outputs.push(factory.prepare::<()>(control).await.unwrap());
    }
    assert_eq!(calls.load(Ordering::SeqCst), SYSTEM_CAPACITY);
    assert_eq!(admission.available_permits(), 0);
    phase(&group, &ids, Step::Prepared).await;
    for (expected, output) in outputs.into_iter().enumerate() {
        assert_eq!(output.adopt(), expected);
    }
    assert_eq!(admission.available_permits(), 0);
    phase(&group, &ids, Step::Published).await;
    assert_eq!(admission.available_permits(), SYSTEM_CAPACITY);
    factory.retire().await.unwrap();
}

struct LocalEcho {
    value: std::rc::Rc<usize>,
    owner: std::thread::ThreadId,
}
impl parrot_api::local_typed::LocalTypedDispatch for LocalEcho {
    type Msg = Request;
    type Reply = usize;
    fn dispatch<'a>(
        &'a mut self,
        _: Request,
        _: &'a parrot_api::typed_context::TypedContext,
    ) -> parrot_api::local_typed::LocalBoxedFuture<'a, parrot_api::types::ActorResult<usize>> {
        assert_eq!(std::thread::current().id(), self.owner);
        Box::pin(async move { Ok(*self.value) })
    }
}
impl Drop for LocalEcho {
    fn drop(&mut self) {
        assert_eq!(std::thread::current().id(), self.owner);
    }
}
impl parrot_api::typed::ParrotMsgVariant<parrot_api::local_typed::LocalProtocol<LocalEcho>>
    for Request
{
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: usize) -> parrot_api::types::ActorResult<usize> {
        Ok(reply)
    }
}

async fn local_root_shares_native_budget(repeatable: bool) {
    let admission = Admission::new(1);
    let mut system =
        crate::actix::ActixActorSystem::with_settings(crate::actix::ActixSystemSettings {
            arbiter_count: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    system.factory_admission = admission.clone();
    let system = Arc::new(system);
    let native_calls = Arc::new(AtomicUsize::new(0));
    let workers = Arc::new(Semaphore::new(1));
    let native = factory(&workers, &admission, &native_calls);
    let held = native.prepare::<()>(Arc::default()).await.unwrap();
    assert_eq!(admission.available_permits(), 0);
    // A staged native output retains invocation capacity after its worker left.
    assert_eq!(workers.available_permits(), 1);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let constructor = move || {
        counted.fetch_add(1, Ordering::SeqCst);
        LocalEcho {
            value: std::rc::Rc::new(42),
            owner: std::thread::current().id(),
        }
    };
    let local = if repeatable {
        system
            .spawn_local_typed_factory::<_, Request, _>(constructor, "/shared-local")
            .await
    } else {
        system
            .spawn_local_typed::<_, Request, _>(constructor, "/shared-local")
            .await
    }
    .unwrap();
    let premature = tokio::time::timeout(Duration::from_millis(100), local.ask(Request)).await;
    let premature_calls = calls.load(Ordering::SeqCst);
    held.discard().await.unwrap();
    assert_eq!(
        tokio::time::timeout(WAIT, local.ask(Request))
            .await
            .unwrap()
            .unwrap(),
        42
    );
    local.stop().await.unwrap();
    native.retire().await.unwrap();
    Arc::try_unwrap(system)
        .ok()
        .expect("final system owner")
        .shutdown()
        .await
        .unwrap();
    assert!(
        premature.is_err(),
        "Local root bypassed native invocation budget"
    );
    assert_eq!(premature_calls, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.available_permits(), 1);
}

#[actix::test]
async fn local_factory_root_shares_native_invocation_budget() {
    local_root_shares_native_budget(true).await;
}

#[actix::test]
async fn local_one_shot_root_shares_native_invocation_budget() {
    local_root_shares_native_budget(false).await;
}
