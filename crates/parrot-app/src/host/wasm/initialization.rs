//! Execute guest code only inside its admitted engine-owned actor lifetime.
use super::*;
use parrot_wasm::runtime::{GuestMessaging, PreparedComponent};

pub(super) struct PendingActor {
    prepared: Option<PreparedComponent>,
    host: HostCtx,
    messaging: Option<Arc<dyn GuestMessaging>>,
    service_grants: Vec<WasmServiceGrant>,
    actor: Option<WasmActor>,
    admission: Arc<Admission>,
    startup: Option<tokio::sync::oneshot::Sender<Result<(), DeployError>>>,
}

impl PendingActor {
    pub(super) fn new(
        prepared: PreparedComponent,
        host: HostCtx,
        messaging: Option<Arc<dyn GuestMessaging>>,
        service_grants: Vec<WasmServiceGrant>,
    ) -> (
        Self,
        tokio::sync::oneshot::Receiver<Result<(), DeployError>>,
    ) {
        let (startup, receipt) = tokio::sync::oneshot::channel();
        (
            Self {
                prepared: Some(prepared),
                host,
                messaging,
                service_grants,
                actor: None,
                admission: Arc::default(),
                startup: Some(startup),
            },
            receipt,
        )
    }

    pub(super) fn message_admission(&self) -> Arc<dyn MessageAdmission> {
        self.admission.clone()
    }
}

impl Actor for PendingActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn use_async_handler(&self) -> bool {
        true
    }

    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let result = async {
                let prepared = self.prepared.take().ok_or_else(|| {
                    DeployError::Engine("Wasm initialization already consumed".into())
                })?;
                // spawn_at reserves this exact identity before init is scheduled.
                // In particular, rejection must not run component core start code.
                let component = prepared
                    .instantiate_with_services(
                        std::mem::take(&mut self.host),
                        self.messaging.take(),
                        services::bind(ctx, std::mem::take(&mut self.service_grants)),
                    )
                    .await
                    .map_err(|error| DeployError::Artifact(error.to_string()))?;
                let mut actor = WasmActor::new_async(component)
                    .await
                    .map_err(|error| DeployError::Engine(error.to_string()))?;
                actor.admission = self.admission.clone();
                self.actor = Some(actor);
                Ok(())
            }
            .await;
            if let Some(startup) = self.startup.take() {
                let _ = startup.send(result.clone());
            }
            result.map_err(|error| ActorError::InitializationError(error.to_string()))
        })
    }

    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.actor
                .as_mut()
                .ok_or_else(|| ActorError::InitializationError("Wasm not initialized".into()))?
                .receive(message)
                .await
        })
    }

    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            match &mut self.actor {
                Some(actor) => actor.drain_async().await.map_err(guest_error),
                None => Ok(()),
            }
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
