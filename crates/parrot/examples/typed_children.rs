//! A static parent creates a static child and owns its actual retirement.
use parrot::{
    thread::ThreadActorSystem,
    typed_context::{TypedContext, TypedContextExt},
};
use parrot_api::{
    Message, ParrotTypedActor,
    typed::{TypedLifecycle, TypedReceive},
    typed_channel::TypedActorRef,
    types::{ActorResult, BoxedFuture},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub struct Start;
impl Message for Start {
    type Result = u64;
}
pub struct Add(u64);
impl Message for Add {
    type Result = u64;
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Add))]
struct Child(Arc<AtomicUsize>);
impl TypedReceive<Add> for Child {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        message: Add,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move {
            assert_eq!(context.parent().unwrap().path(), "/parent");
            Ok(message.0 + 1)
        })
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Start), lifecycle)]
struct Parent {
    retired: Arc<AtomicUsize>,
    child: Option<TypedActorRef<Child, Add>>,
}
impl Parent {
    fn new(retired: Arc<AtomicUsize>) -> Self {
        Self {
            retired,
            child: None,
        }
    }
}
impl TypedLifecycle for Parent {
    fn init_with_context<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.child = Some(
                context
                    .spawn_typed_child::<_, Add>(Child(self.retired.clone()))
                    .await?,
            );
            Ok(())
        })
    }
    fn before_stop_with_context<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert!(context.children().is_empty());
            assert_eq!(self.retired.load(Ordering::SeqCst), 1);
            Ok(())
        })
    }
}
impl TypedReceive<Start> for Parent {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Start,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move {
            assert_eq!(context.children().len(), 1);
            self.child
                .as_ref()
                .expect("initialized child")
                .ask(Add(41))
                .await
        })
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    actix::System::new().block_on(async {
    let system = ThreadActorSystem::shared(Default::default());
    let retired = Arc::new(AtomicUsize::new(0));
    let parent = system
        .spawn_typed::<_, Start>(Parent::new(retired.clone()), "/parent")
        .await?;
    assert_eq!(parent.ask(Start).await?, 42);
    assert_eq!(system.actor_count(), 2);
    parent.stop().await?;
    assert_eq!(retired.load(Ordering::SeqCst), 1);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await?;
    println!(
        "Thread typed parent/child: concrete reply 42, real parent identity, complete child retirement"
    );
    let system = Arc::new(parrot::actix::ActixActorSystem::with_arbiter_count(2).await?);
    let retired = Arc::new(AtomicUsize::new(0));
    let parent = system.spawn_typed::<_, Start>(Parent::new(retired.clone()), "/parent").await?;
    assert_eq!(parent.ask(Start).await?, 42);
    assert_eq!(system.status().active_actors, 2);
    parent.stop().await?;
    assert_eq!(retired.load(Ordering::SeqCst), 1);
    assert_eq!(system.status().active_actors, 0);
    system.as_ref().clone().shutdown().await?;
    println!("Actix typed parent/child: concrete reply 42, real parent identity, complete child retirement");
    Ok(())
    })
}
