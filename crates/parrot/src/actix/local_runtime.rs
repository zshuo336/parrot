//! Type-erased local behavior on the existing managed owner-thread runtime.
use super::{ActixActorSystem, local_typed::LocalSource};
use crate::owned_payload::OwnedPayload;
use parrot_api::{
    address::ActorRef,
    engine::{AsyncLocalRuntime, FactoryOptions, LocalRuntimeActor, LocalRuntimeFactory},
    local_typed::{LocalBoxedFuture, LocalMsgVariant, LocalTypedActorRef, LocalTypedDispatch},
    supervisor::{DefaultSupervisorStrategyFactory, SupervisorStrategyType},
    system::SystemError,
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{sync::Arc, time::Duration};

struct LocalBehavior(LocalRuntimeActor);
struct LocalMessage(BoxedMessage);
impl parrot_api::Message for LocalMessage {
    type Result = BoxedMessage;
}
impl LocalMsgVariant<LocalMessage> for LocalBehavior {
    fn inject(message: LocalMessage) -> BoxedMessage {
        message.0
    }
    fn extract(reply: BoxedMessage) -> ActorResult<BoxedMessage> {
        Ok(reply)
    }
}
impl LocalTypedDispatch for LocalBehavior {
    type Msg = BoxedMessage;
    type Reply = BoxedMessage;
    fn termination_protocol() -> Option<parrot_api::typed::TerminationProtocol<Self::Msg>> {
        Some(|event| Ok(Box::new(event)))
    }
    fn init<'a>(&'a mut self, context: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.0.init(context)
    }
    fn before_stop<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.0.before_stop(context)
    }
    fn init_local_streams<'a>(
        &'a mut self,
        _context: &'a TypedContext,
        streams: &'a mut parrot_api::local_typed::LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.0.init_local_streams(streams)
    }
    fn local_stream_started<'a>(
        &'a mut self,
        id: u64,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.0.local_stream_started(id, context)
    }
    fn local_stream_item<'a>(
        &'a mut self,
        id: u64,
        item: Box<dyn std::any::Any + 'static>,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.0.local_stream_item(id, item, context)
    }
    fn local_stream_finished<'a>(
        &'a mut self,
        id: u64,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.0.local_stream_finished(id, context)
    }
    fn local_stream_error<'a>(
        &'a mut self,
        id: u64,
        error: parrot_api::errors::ActorError,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.0.local_stream_error(id, error, context)
    }
    fn dispatch<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.receive(message, context)
    }
}

// Only the protocol reference is Send. The adapter forwards raw dynamic values
// and returns raw replies; it must not downcast callers' messages to LocalMessage
// or add a second Box around responses. Registry, self-ref, and callers all use
// this view of the SAME channel/control, including its death-watch identity.
#[derive(Clone)]
struct LocalRef(LocalTypedActorRef<LocalBehavior, LocalMessage>);
impl std::fmt::Debug for LocalRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRef")
            .field("path", &self.0.path())
            .finish()
    }
}
impl ActorRef for LocalRef {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(self.0.ask_unbounded(LocalMessage(message)))
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        match timeout {
            Some(timeout) => Box::pin(self.0.ask_with_timeout(LocalMessage(message), timeout)),
            None => self.send(message),
        }
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(self.0.tell(LocalMessage(message)))
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(self.0.stop())
    }
    fn path(&self) -> String {
        self.0.path().to_owned()
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        Box::pin(async { self.0.is_alive() })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        self.0.clone().into_dyn().death_watch()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl AsyncLocalRuntime for ActixActorSystem {
    fn spawn_local_factory<'a>(
        &'a self,
        factory: LocalRuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        let factory = OwnedPayload::new(factory, &self.runtime, &self.cleanup_slots);
        Box::pin(parrot_api::trace::capture(async move {
            if let Err(error) = options.validate() {
                factory.retire().await.map_err(SystemError::ActorError)?;
                return Err(error);
            }
            let constructor = factory.take();
            let factory = OwnedPayload::new(
                move || LocalBehavior(constructor()),
                &self.runtime,
                &self.cleanup_slots,
            );
            let policy = OwnedPayload::new(
                Some(SupervisorStrategyType::OneForOne(
                    DefaultSupervisorStrategyFactory::one_for_one(
                        options.max_restarts,
                        options.within,
                    ),
                )),
                &self.runtime,
                &self.cleanup_slots,
            );
            let path = options
                .spawn
                .path
                .unwrap_or_else(|| format!("actix://local/{}", uuid::Uuid::new_v4().simple()));
            let reference = Arc::new(self.clone())
                .spawn_local_source(
                    factory,
                    policy,
                    LocalSource {
                        path: &path,
                        parent: None,
                        repeatable: true,
                        erase: |reference| Box::new(LocalRef(reference)),
                    },
                )
                .await
                .map_err(SystemError::ActorError)?;
            Ok(Box::new(LocalRef(reference)) as BoxedActorRef)
        }))
    }
}

impl ActixActorSystem {
    pub(crate) async fn spawn_neutral_local_child(
        self: &Arc<Self>,
        factory: OwnedPayload<LocalRuntimeFactory>,
        policy: OwnedPayload<Option<SupervisorStrategyType>>,
        path: &str,
        parent: super::local_typed::LocalParent<'_>,
    ) -> ActorResult<BoxedActorRef> {
        let constructor = factory.take();
        let factory = OwnedPayload::new(
            move || LocalBehavior(constructor()),
            &self.runtime,
            &self.cleanup_slots,
        );
        let reference = self
            .spawn_local_source(
                factory,
                policy,
                LocalSource {
                    path,
                    parent: Some(parent),
                    repeatable: true,
                    erase: |reference| Box::new(LocalRef(reference)),
                },
            )
            .await?;
        Ok(Box::new(LocalRef(reference)))
    }
}
