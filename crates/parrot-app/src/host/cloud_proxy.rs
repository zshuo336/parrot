//! A sharded Thread actor which owns one durable outbox.
use crate::{assemble::DeployError, durable::*, ParrotAppHook};
use parrot::thread::{
    BackpressureStrategy, SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadContext,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{codec_registry::CodecRegistry, durable::DurableOutbox, RemoteActorSystem};
use std::{sync::Arc, time::Duration};

mod admission;
use admission::{Admission, Admitted};

pub struct CloudProxy {
    outbox: DurableOutbox,
    draining: bool,
    drain_timeout: Duration,
    batch_bytes: usize,
    admission: Arc<Admission>,
}
impl CloudProxy {
    pub async fn spawn(
        engine: &Arc<ThreadActorSystem>,
        remote: &Arc<RemoteActorSystem>,
        path: &str,
        config: CloudProxyConfig,
    ) -> Result<BoxedActorRef, DeployError> {
        let receiver = config.validate().map_err(DeployError::Config)?;
        let affinity = receiver.node_id.clone();
        let admission = Arc::new(Admission::new(&config));
        let outbox = DurableOutbox::open(
            &config.directory,
            remote,
            receiver,
            &config.path,
            &config.session,
            config.outbox_config(),
        )
        .await
        .map_err(|e| DeployError::Engine(e.to_string()))?;
        let actor = engine
            .spawn_at(
                Self {
                    outbox,
                    draining: false,
                    drain_timeout: Duration::from_millis(config.drain_timeout_ms),
                    batch_bytes: config.outbox_config().command_bytes as usize,
                    admission: admission.clone(),
                },
                path,
                None,
                ThreadActorConfig {
                    // Admission/drain/storage errors are request results. Keep
                    // the outbox owner available for inspection and retry; its
                    // durable state machine independently seals terminal faults.
                    supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
                    message_admission: Some(admission),
                    message_batch_size: Some(config.group_commit_max_records.min(256)),
                    scheduling_mode: Some(SchedulingMode::Sharded {
                        affinity_key: affinity,
                    }),
                    mailbox_capacity: Some(config.mailbox_capacity),
                    backpressure_strategy: Some(BackpressureStrategy::Block),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| DeployError::Engine(e.to_string()))?;
        Ok(Box::new(actor))
    }
    fn snapshot(&self) -> CloudProxyStatus {
        let s = self.outbox.handle().status();
        CloudProxyStatus {
            pending: s.pending as u64,
            last_enqueued: s.last_enqueued,
            last_acked: s.last_acked,
            attempts: s.attempts,
            draining: self.draining,
            closed: s.closed,
            last_error: s.last_error,
            terminal_error: s.terminal_error.map(|e| e.to_string()),
        }
    }
    async fn drain(&mut self) -> ActorResult<()> {
        self.draining = true;
        let handle = self.outbox.handle();
        let status = handle.status();
        if status.closed {
            return Err(error(
                status
                    .terminal_error
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "outbox closed".into()),
            ));
        }
        let last = status.last_enqueued;
        if last != 0 {
            tokio::time::timeout(self.drain_timeout, handle.wait_acked(last))
                .await
                .map_err(|_| ActorError::Timeout)?
                .map_err(error)?;
        }
        Ok(())
    }
}
fn error(e: impl std::fmt::Display) -> ActorError {
    ActorError::MessageHandlingError(e.to_string())
}
impl Actor for CloudProxy {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        let message = Admitted::message(message);
        !message.is::<CloudProxyInspect>()
            && !message.is::<CloudProxyDrain>()
            && !message.is::<ParrotAppHook>()
            && !message.is::<CloudProxySubmitBatch>()
            && !message.is::<CloudProxyInspectStorage>()
            && !message.is::<CloudProxyInspectAdmission>()
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        Box::pin(async move {
            if self.draining {
                return messages
                    .into_iter()
                    .map(|_| {
                        Err(error(
                            "cloud proxy is draining; business admission is closed",
                        ))
                    })
                    .collect();
            }
            let handle = self.outbox.handle();
            // Admission normally supplies encoded submits. Initial enqueue
            // polling follows dequeue order. Each
            // result stays tied to its original request; one rejection cannot
            // discard successful neighbours or turn their errors into success.
            futures::future::join_all(messages.into_iter().map(|message| {
                let (message, permit) = Admitted::unpack(message);
                let encoded = match message.downcast::<CloudProxySubmitIdempotent>() {
                    Ok(submit) => Ok((Some(submit.request_id), submit.type_key, submit.payload)),
                    Err(message) => match message.downcast::<CloudProxySubmit>() {
                        Ok(submit) => Ok((None, submit.type_key, submit.payload)),
                        Err(message) => CodecRegistry::global()
                            .encode_outgoing(&message)
                            .map(|(key, payload)| (None, key, bytes::Bytes::from(payload)))
                            .map_err(|e| {
                                e.to_actor_error("cloud proxy business message codec".into())
                            }),
                    },
                };
                let handle = &handle;
                async move {
                    let _permit = permit;
                    let (id, key, payload) = encoded?;
                    let sequence = match id {
                        Some(id) => handle.enqueue_idempotent(&id, &key, &payload).await,
                        None => handle.enqueue(&key, &payload).await,
                    }
                    .map_err(error)?;
                    Ok(Box::new(CloudProxyAccepted { sequence }) as BoxedMessage)
                }
            }))
            .await
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let (msg, _permit) = Admitted::unpack(msg);
            if msg.is::<CloudProxyInspectAdmission>() {
                return Ok(Box::new(self.admission.snapshot()) as BoxedMessage);
            }
            if msg.is::<CloudProxyInspectStorage>() {
                let status = self.outbox.handle().status();
                return Ok(Box::new(CloudProxyStorageStats {
                    acceptance_syncs: status.acceptance_syncs,
                    last_group_records: status.last_group_records as u64,
                }) as BoxedMessage);
            }
            if msg.is::<CloudProxyInspect>() {
                return Ok(Box::new(self.snapshot()) as BoxedMessage);
            }
            if msg.is::<CloudProxyDrain>() {
                self.drain().await?;
                return Ok(Box::new(self.snapshot()) as BoxedMessage);
            }
            if let Some(hook) = msg.downcast_ref::<ParrotAppHook>() {
                match hook.0 {
                    crate::hook_phase::DRAIN => self.drain().await?,
                    crate::hook_phase::START | crate::hook_phase::STOP => {}
                    _ => return Err(error("invalid cloud proxy lifecycle phase")),
                }
                return Ok(Box::new(*hook) as BoxedMessage);
            }
            if self.draining {
                return Err(error(
                    "cloud proxy is draining; business admission is closed",
                ));
            }
            if let Some(batch) = msg.downcast_ref::<CloudProxySubmitBatch>() {
                if batch.messages.is_empty()
                    || batch.messages.len() > CLOUD_PROXY_MAX_BATCH_RECORDS
                    || batch
                        .messages
                        .iter()
                        .try_fold(0usize, |size, message| {
                            size.checked_add(message.type_key.len())?
                                .checked_add(message.payload.len())
                        })
                        .is_none_or(|bytes| bytes > self.batch_bytes)
                {
                    return Err(error(
                        "cloud proxy batch exceeds record/byte limits or is empty",
                    ));
                }
                let handle = self.outbox.handle();
                let outcomes =
                    futures::future::join_all(batch.messages.iter().map(|message| async {
                        handle
                            .enqueue(&message.type_key, &message.payload)
                            .await
                            .map(|sequence| CloudProxyAccepted { sequence })
                            .map_err(|e| e.to_string())
                    }))
                    .await;
                return Ok(Box::new(CloudProxyBatchResult { outcomes }) as BoxedMessage);
            }
            let sequence = if let Some(submit) = msg.downcast_ref::<CloudProxySubmitIdempotent>() {
                self.outbox
                    .handle()
                    .enqueue_idempotent(&submit.request_id, &submit.type_key, &submit.payload)
                    .await
                    .map_err(error)?
            } else if let Some(submit) = msg.downcast_ref::<CloudProxySubmit>() {
                self.outbox
                    .handle()
                    .enqueue(&submit.type_key, &submit.payload)
                    .await
                    .map_err(error)?
            } else {
                let (key, payload) = CodecRegistry::global()
                    .encode_outgoing(&msg)
                    .map_err(|e| e.to_actor_error("cloud proxy business message codec".into()))?;
                self.outbox
                    .handle()
                    .enqueue(&key, &payload)
                    .await
                    .map_err(error)?
            };
            Ok(Box::new(CloudProxyAccepted { sequence }) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.draining = true;
            self.outbox.close().await.map_err(error)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
