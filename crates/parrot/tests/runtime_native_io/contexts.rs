use super::*;
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::ActorContext,
    message::Message,
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};

type Work = BoxedFuture<'static, ActorResult<BoxedMessage>>;
enum Request {
    Tcp,
    Capture(PendingInput),
    CaptureWork(Work),
}

fn execute(ctx: &dyn ActorContext, message: BoxedMessage) -> Work {
    let io = ctx
        .io_runtime()
        .expect("native actor context exposes configured I/O");
    match *message.downcast::<Request>().unwrap() {
        Request::Tcp => {
            let actor = std::thread::current().id();
            let work = io.run_io_typed(tcp());
            Box::pin(async move { Ok(Box::new((actor, work.await?)) as BoxedMessage) })
        }
        Request::Capture(input) => {
            let work = io.run_io(Box::pin(input));
            Box::pin(async move { Ok(Box::new(work) as BoxedMessage) })
        }
        Request::CaptureWork(input) => {
            let work = io.run_io(input);
            Box::pin(async move { Ok(Box::new(work) as BoxedMessage) })
        }
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
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        execute(ctx, message)
    }
}
struct ActixProbe;
impl Actor for ActixProbe {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        execute(ctx, message)
    }
}
impl ActixEngineExt for ActixProbe {}

pub(super) async fn facade(actix: bool) -> ParrotActorSystem {
    if actix {
        ParrotActorSystem::with_actix(config(), Default::default())
            .await
            .unwrap()
    } else {
        ParrotActorSystem::start(config()).await.unwrap()
    }
}

pub(super) async fn capture_work(
    facade: &ParrotActorSystem,
    actix: bool,
    input: Work,
) -> (BoxedActorRef, Work) {
    let actor: BoxedActorRef = if actix {
        bounded(facade.spawn_root_actix(ActixProbe, EmptyConfig))
            .await
            .unwrap()
    } else {
        bounded(facade.spawn_root_thread(ThreadProbe, EmptyConfig))
            .await
            .unwrap()
    };
    let work = *bounded(actor.send(Box::new(Request::CaptureWork(input))))
        .await
        .unwrap()
        .downcast::<Work>()
        .unwrap();
    (actor, work)
}

async fn dynamic_context(actix: bool) {
    let facade = facade(actix).await;
    let actor: BoxedActorRef = if actix {
        bounded(facade.spawn_root_actix(ActixProbe, EmptyConfig))
            .await
            .unwrap()
    } else {
        bounded(facade.spawn_root_thread(ThreadProbe, EmptyConfig))
            .await
            .unwrap()
    };
    let ids = *bounded(actor.send(Box::new(Request::Tcp)))
        .await
        .unwrap()
        .downcast::<(ThreadId, ThreadId)>()
        .unwrap();
    assert_ne!(ids.0, ids.1, "I/O must leave the actor worker");
    assert_ne!(ids.1, std::thread::current().id());
    let (input, entered, retired) = pending();
    let work = *bounded(actor.send(Box::new(Request::Capture(input))))
        .await
        .unwrap()
        .downcast::<Work>()
        .unwrap();
    let work = tokio::spawn(work);
    bounded(entered).await.unwrap();
    // Capture a second invocation while the generation is live, but poll it after stop.
    let (input, not_entered, late_retired) = pending();
    let late = *bounded(actor.send(Box::new(Request::Capture(input))))
        .await
        .unwrap()
        .downcast::<Work>()
        .unwrap();
    bounded(actor.stop()).await.unwrap();
    bounded(retired).await.unwrap();
    assert!(matches!(
        bounded(work).await.unwrap(),
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(matches!(
        bounded(late).await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    bounded(late_retired).await.unwrap();
    assert!(not_entered.await.is_err());
    bounded(facade.shutdown_runtime()).await.unwrap();
}

enum TypedRequest {
    Tcp,
    Context,
}
enum TypedReply {
    Threads(ThreadId, ThreadId),
    Context(TypedContext),
}
impl Message for TypedRequest {
    type Result = TypedReply;
}
struct TypedProbe;
impl ParrotTypedDispatch for TypedProbe {
    type Msg = TypedRequest;
    type Reply = TypedReply;
    fn dispatch(&mut self, _: TypedRequest) -> BoxedFuture<'_, ActorResult<TypedReply>> {
        Box::pin(async { panic!("managed context dispatch required") })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        msg: TypedRequest,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<TypedReply>> {
        Box::pin(async move {
            match msg {
                TypedRequest::Context => Ok(TypedReply::Context(ctx.clone())),
                TypedRequest::Tcp => {
                    let actor = std::thread::current().id();
                    let io = ctx
                        .io_runtime()
                        .expect("typed context exposes configured I/O");
                    Ok(TypedReply::Threads(actor, io.run_io_typed(tcp()).await?))
                }
            }
        })
    }
}
impl ParrotMsgVariant<TypedProbe> for TypedRequest {
    fn inject(msg: Self) -> TypedRequest {
        msg
    }
    fn extract(reply: TypedReply) -> ActorResult<TypedReply> {
        Ok(reply)
    }
}

async fn typed_context(actix: bool) {
    let facade = facade(actix).await;
    let actor = if actix {
        let runtime = facade.runtime("default").unwrap();
        let runtime: Arc<dyn std::any::Any + Send + Sync> = runtime;
        let system = runtime
            .downcast::<parrot::actix::ActixActorSystem>()
            .ok()
            .unwrap();
        bounded(system.spawn_typed::<_, TypedRequest>(TypedProbe, "/io/typed"))
            .await
            .unwrap()
    } else {
        let system = facade.get_thread_system("default").unwrap();
        bounded(system.spawn_typed::<_, TypedRequest>(TypedProbe, "/io/typed"))
            .await
            .unwrap()
    };
    let TypedReply::Threads(actor_id, io_id) = bounded(actor.ask(TypedRequest::Tcp)).await.unwrap()
    else {
        panic!("thread identities required")
    };
    assert_ne!(actor_id, io_id);
    assert_ne!(io_id, std::thread::current().id());
    let TypedReply::Context(context) = bounded(actor.ask(TypedRequest::Context)).await.unwrap()
    else {
        panic!("retained context required")
    };
    let (input, entered, retired) = pending();
    let work = tokio::spawn(context.io_runtime().unwrap().run_io(Box::pin(input)));
    bounded(entered).await.unwrap();
    let (input, not_entered, late_retired) = pending();
    let late = context.io_runtime().unwrap().run_io(Box::pin(input));
    bounded(actor.stop()).await.unwrap();
    bounded(retired).await.unwrap();
    assert!(matches!(
        bounded(work).await.unwrap(),
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(context.io_runtime().is_none());
    assert!(matches!(
        bounded(late).await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    bounded(late_retired).await.unwrap();
    assert!(not_entered.await.is_err());
    bounded(facade.shutdown_runtime()).await.unwrap();
}

#[tokio::test]
async fn thread_dynamic_io_belongs_to_the_actor_generation() {
    dynamic_context(false).await;
}
#[actix::test]
async fn actix_dynamic_io_belongs_to_the_actor_generation() {
    dynamic_context(true).await;
}
#[tokio::test]
async fn thread_typed_io_belongs_to_the_actor_generation() {
    typed_context(false).await;
}
#[actix::test]
async fn actix_typed_io_belongs_to_the_actor_generation() {
    typed_context(true).await;
}
