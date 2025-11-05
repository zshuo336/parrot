use futures::FutureExt;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorSpawner},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Hostile(Arc<AtomicUsize>);
impl Drop for Hostile {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("factory input disposal failed");
    }
}
struct ThreadProbe;
impl Actor for ThreadProbe {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(m) })
    }
}
struct Native;
impl actix::Actor for Native {
    type Context = actix::Context<Self>;
}
async fn check(spawner: &dyn ActorSpawner, unpolled: bool) {
    let count = Arc::new(AtomicUsize::new(0));
    let future = spawner.spawn_factory_erased(Box::new(Hostile(count.clone())), None);
    if unpolled {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future)));
        assert!(
            result.is_ok(),
            "unpolled factory inputs unwound through caller"
        );
    } else {
        let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
        assert!(
            matches!(result, Ok(Err(_))),
            "rejected factory inputs unwound through caller"
        );
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while count.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn thread_unbound_factory_retirement_is_owned() {
    let ctx = parrot::thread::ThreadContext::<ThreadProbe>::new_for_test("/unbound");
    check(&ctx, false).await;
}
#[tokio::test]
async fn thread_unpolled_factory_retirement_is_owned() {
    let ctx = parrot::thread::ThreadContext::<ThreadProbe>::new_for_test("/unbound");
    check(&ctx, true).await;
}
#[actix::test]
async fn actix_unbound_factory_retirement_is_owned() {
    use actix::Actor as _;
    let mut ctx = parrot::actix::ActixContext::new(
        Native.start(),
        parrot_api::ActorPath::placeholder("/unbound"),
    );
    check(ctx.spawner(), false).await;
}
#[actix::test]
async fn actix_unpolled_factory_retirement_is_owned() {
    use actix::Actor as _;
    let mut ctx = parrot::actix::ActixContext::new(
        Native.start(),
        parrot_api::ActorPath::placeholder("/unbound"),
    );
    check(ctx.spawner(), true).await;
}

fn hostile_policy(count: Arc<AtomicUsize>) -> parrot_api::supervisor::SupervisorStrategyType {
    use parrot_api::supervisor::{
        BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType,
    };
    let capture = Hostile(count);
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 1,
        within: std::time::Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    })
}

async fn check_instance(spawner: &dyn ActorSpawner, unpolled: bool) {
    let count = Arc::new(AtomicUsize::new(0));
    let future = spawner.spawn_with_strategy(
        Box::new(Hostile(count.clone())),
        Box::new(Hostile(count.clone())),
        hostile_policy(count.clone()),
    );
    if unpolled {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok());
    } else {
        assert!(matches!(
            std::panic::AssertUnwindSafe(future).catch_unwind().await,
            Ok(Err(_))
        ));
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while count.load(Ordering::SeqCst) != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[actix::test]
async fn actix_unpolled_instance_inputs_are_separately_owned() {
    use actix::Actor as _;
    let mut ctx = parrot::actix::ActixContext::new(
        Native.start(),
        parrot_api::ActorPath::placeholder("/unbound"),
    );
    check_instance(ctx.spawner(), true).await;
}
#[actix::test]
async fn actix_unbound_instance_inputs_are_separately_retired() {
    use actix::Actor as _;
    let mut ctx = parrot::actix::ActixContext::new(
        Native.start(),
        parrot_api::ActorPath::placeholder("/unbound"),
    );
    check_instance(ctx.spawner(), false).await;
}
#[test]
fn actix_unbound_rejection_requires_no_executor() {
    let (sender, _receiver) = actix::dev::channel::channel::<Native>(1);
    let mut ctx = parrot::actix::ActixContext::new(
        actix::Addr::new(sender),
        parrot_api::ActorPath::placeholder("/unbound"),
    );
    let count = Arc::new(AtomicUsize::new(0));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let future = ctx.spawner().spawn_factory_erased(
            Box::new(Hostile(count.clone())),
            Some(hostile_policy(count.clone())),
        );
        assert!(futures::executor::block_on(future).is_err());
        drop(ctx.spawner().spawn_with_strategy(
            Box::new(Hostile(count.clone())),
            Box::new(Hostile(count.clone())),
            hostile_policy(count.clone()),
        ));
    }));
    assert!(outcome.is_ok());
    assert_eq!(count.load(Ordering::SeqCst), 5);
}
