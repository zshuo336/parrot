//! Owned MQTT ingress for one explicitly authorized producer and receiver.
use super::{key_to_topic, TOPIC_PREFIX};
use crate::{
    durable::{
        DurableEnvelope, DurableOutbox, OutboxConfig, OutboxHandle, DURABLE_KEY,
        DURABLE_RECEIPT_KEY,
    },
    frame::{flags, frame_type, Frame, MAX_FRAME_LEN},
    NodeAddr, RemoteActorSystem,
};
use bytes::{Bytes, BytesMut};
use futures::FutureExt;
use rumqttc::{AsyncClient, Event, MqttOptions, Outgoing, Packet, Publish, QoS};
use sha2::{Digest, Sha256};
use std::{net::IpAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, thiserror::Error)]
pub enum BridgeError {
    #[error("invalid MQTT bridge configuration or message: {0}")]
    Invalid(String),
    #[error("MQTT bridge broker error: {0}")]
    Broker(String),
    #[error("MQTT bridge storage error: {0}")]
    Storage(String),
    #[error("MQTT bridge closed")]
    Closed,
    #[error("MQTT bridge deadline exceeded; retirement remains owned")]
    Deadline,
    #[error("MQTT bridge owner failed")]
    Worker,
}

/// One exact topic, producer session and target, bound permanently to its WAL.
/// Broker ACLs must authorize that producer for this topic. MQTT 3.1.1 does not
/// deliver the publisher's authenticated identity to subscribers.
#[derive(Clone)]
pub struct BridgeConfig {
    pub mqtt: MqttOptions,
    pub key: String,
    pub producer: String,
    pub session: String,
    pub receiver: NodeAddr,
    pub path: String,
    pub directory: PathBuf,
    pub outbox: OutboxConfig,
    pub max_frame_bytes: usize,
    pub queue_capacity: usize,
    pub queue_bytes: u32,
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// Permits plaintext only to a literal loopback IP. Otherwise TLS is required.
    pub insecure_development: bool,
}
impl BridgeConfig {
    pub fn validate(&self) -> Result<(), BridgeError> {
        let (host, port) = self.mqtt.broker_address();
        let id = self.mqtt.client_id();
        let plaintext_loopback =
            self.insecure_development && host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if !matches!(self.mqtt.transport(), rumqttc::Transport::Tls(_))
            && !(plaintext_loopback && matches!(self.mqtt.transport(), rumqttc::Transport::Tcp))
        {
            return Err(BridgeError::Invalid(
                "broker TLS is required (explicit loopback development exception only)".into(),
            ));
        }
        if port == 0
            || host.is_empty()
            || host.chars().any(char::is_control)
            || id.is_empty()
            || id.len() > 128
            || id.chars().any(char::is_control)
            || self.mqtt.keep_alive().is_zero()
            || self.key.is_empty()
            || self.key.len() > 1024
            || self
                .key
                .chars()
                .any(|c| c.is_control() || c == '+' || c == '#')
            || self.key.split('/').any(str::is_empty)
            || self.session.is_empty()
            || self.session.len() > 128
            || self.session.chars().any(char::is_control)
            || !self.directory.is_absolute()
            || !self.path.starts_with('/')
            || self.path.len() > 4096
            || self.path.chars().any(char::is_control)
            || !(64..=MAX_FRAME_LEN as usize + 4).contains(&self.max_frame_bytes)
            || !(1..=4096).contains(&self.queue_capacity)
            || u64::from(self.queue_bytes) < self.max_frame_bytes as u64 * 3 + 2048
            || self.queue_bytes > 256 * 1024 * 1024
            || self.retry_min.is_zero()
            || self.retry_min > self.retry_max
            || self.retry_max > Duration::from_secs(30)
            || !self.outbox.idempotent_submissions
            || !self.outbox.transactional
        {
            return Err(BridgeError::Invalid(
                "invalid identity, route, durable mode or resource limits".into(),
            ));
        }
        parrot_config::endpoint::validate_node_id(&self.producer).map_err(BridgeError::Invalid)?;
        Ok(())
    }
    pub fn topic(&self) -> String {
        key_to_topic(&self.key)
    }
    pub fn receipt_topic(&self) -> String {
        format!("$parrot-receipts/{}", self.key)
    }
    fn target(&self) -> String {
        format!("parrot://{}{}", self.receiver.node_id, self.path)
    }
    fn binding(&self) -> String {
        let mut hash = Sha256::new();
        let (host, port) = self.mqtt.broker_address();
        for field in [
            host,
            port.to_string(),
            self.mqtt.client_id(),
            self.topic(),
            self.producer.clone(),
            self.session.clone(),
        ] {
            hash.update((field.len() as u64).to_le_bytes());
            hash.update(field.as_bytes());
        }
        format!("mqtt-v1:{:x}", hash.finalize())
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BridgeStatus {
    pub ready: bool,
    pub draining: bool,
    pub closed: bool,
    pub generation: u64,
    pub reconnects: u64,
    pub qos0_forwarded: u64,
    pub durable_accepted: u64,
    pub completed_receipts: u64,
    pub pending_durable: usize,
    pub last_enqueued: u64,
    pub last_acked: u64,
    pub last_error: Option<String>,
    pub terminal_error: Option<String>,
}
#[derive(Clone)]
pub struct BridgeHandle {
    stop: CancellationToken,
    drain: CancellationToken,
    status: watch::Receiver<BridgeStatus>,
    done: watch::Receiver<Option<Result<(), BridgeError>>>,
    outbox: OutboxHandle,
}
impl BridgeHandle {
    pub fn status(&self) -> BridgeStatus {
        let mut status = self.status.borrow().clone();
        let durable = self.outbox.status();
        status.pending_durable = durable.pending;
        status.last_enqueued = durable.last_enqueued;
        status.last_acked = durable.last_acked;
        status
    }
    pub async fn ready(&self) -> Result<(), BridgeError> {
        let mut status = self.status.clone();
        loop {
            let state = status.borrow_and_update().clone();
            if state.closed {
                return self.finished().await.and(Err(BridgeError::Closed));
            }
            if state.ready {
                return Ok(());
            }
            status.changed().await.map_err(|_| BridgeError::Worker)?;
        }
    }
    pub fn request_close(&self) {
        self.stop.cancel();
    }
    pub async fn close(&self) -> Result<(), BridgeError> {
        self.stop.cancel();
        self.finished().await
    }
    /// Fence new MQTT admission, finish admitted jobs and persist business ACKs.
    /// Expiry/cancellation does not abandon the drain. close() can interrupt it;
    /// accepted but unfinished durable messages remain in the WAL for reopen.
    pub async fn drain(&self, timeout: Duration) -> Result<(), BridgeError> {
        if timeout.is_zero() {
            return Err(BridgeError::Invalid("zero drain deadline".into()));
        }
        self.drain.cancel();
        tokio::time::timeout(timeout, self.finished())
            .await
            .map_err(|_| BridgeError::Deadline)?
    }
    async fn finished(&self) -> Result<(), BridgeError> {
        let mut done = self.done.clone();
        loop {
            if let Some(result) = done.borrow_and_update().clone() {
                return result;
            }
            done.changed().await.map_err(|_| BridgeError::Worker)?;
        }
    }
}
/// Owner of the MQTT connection, bounded delivery pipeline and durable outbox.
/// Drop fences admission and requests cleanup. Keep the runtime alive and await
/// close()/drain() for a retirement receipt. Cloned handles do not extend ownership.
pub struct MqttBridge {
    handle: BridgeHandle,
}
impl MqttBridge {
    pub async fn start(
        mut config: BridgeConfig,
        remote: Arc<RemoteActorSystem>,
    ) -> Result<Self, BridgeError> {
        config.validate()?;
        config.mqtt.set_clean_session(false).set_manual_acks(true);
        config.mqtt.set_max_packet_size(
            config.max_frame_bytes + config.topic().len() + 16,
            config.max_frame_bytes,
        );
        config.mqtt.set_inflight(1);
        let outbox = DurableOutbox::open(
            &config.directory,
            &remote,
            config.receiver.clone(),
            &config.path,
            &config.binding(),
            config.outbox.clone(),
        )
        .await
        .map_err(|e| BridgeError::Storage(e.to_string()))?;
        let stop = CancellationToken::new();
        let drain = CancellationToken::new();
        let (status, status_rx) = watch::channel(BridgeStatus::default());
        let (done, done_rx) = watch::channel(None);
        let handle = BridgeHandle {
            stop: stop.clone(),
            drain: drain.clone(),
            status: status_rx,
            done: done_rx,
            outbox: outbox.handle(),
        };
        let task_status = status.clone();
        tokio::spawn(async move {
            let result =
                std::panic::AssertUnwindSafe(run(config, remote, outbox, stop, drain, task_status))
                    .catch_unwind()
                    .await
                    .unwrap_or(Err(BridgeError::Worker));
            status.send_modify(|s| {
                s.ready = false;
                s.closed = true;
                s.terminal_error = result.as_ref().err().map(ToString::to_string);
            });
            done.send_replace(Some(result));
        });
        Ok(Self { handle })
    }
    pub fn handle(&self) -> BridgeHandle {
        self.handle.clone()
    }
    pub async fn close(&self) -> Result<(), BridgeError> {
        self.handle.close().await
    }
}
impl Drop for MqttBridge {
    fn drop(&mut self) {
        self.handle.request_close();
    }
}

struct Job {
    generation: u64,
    ack: Publish,
    work: Work,
    _bytes: OwnedSemaphorePermit,
}
enum Work {
    Tell(Frame),
    Durable { envelope: DurableEnvelope, cid: u64 },
}
struct Completed {
    job: Job,
    receipt: Option<Bytes>,
}
fn decode(config: &BridgeConfig, publish: &Publish) -> Result<Work, BridgeError> {
    let invalid = |reason: &str| BridgeError::Invalid(reason.into());
    if publish.topic != config.topic()
        || !publish.topic.starts_with(TOPIC_PREFIX)
        || publish.retain
        || publish.payload.len() > config.max_frame_bytes
        || publish.qos == QoS::ExactlyOnce
    {
        return Err(invalid(
            "unauthorized topic, retained publication, QoS2 or packet budget",
        ));
    }
    let mut bytes = BytesMut::from(publish.payload.as_ref());
    let mut frame = Frame::decode(&mut bytes)
        .map_err(|_| invalid("invalid Wire frame"))?
        .ok_or_else(|| invalid("truncated Wire frame"))?;
    if !bytes.is_empty()
        || frame.header.frame_type != frame_type::TELL
        || frame.path != config.target()
        || frame.header.seq != 0
        || frame.header.hop_count.saturating_add(1) >= frame.header.hop_limit
    {
        return Err(invalid(
            "one TELL to the configured target with a remaining hop is required",
        ));
    }
    frame
        .validate_payload_flags()
        .map_err(|_| invalid("unsupported Wire payload flags"))?;
    if publish.qos == QoS::AtMostOnce {
        if frame.header.flags != 0
            || frame.header.correlation_id != 0
            || frame.type_key == DURABLE_KEY
        {
            return Err(invalid("QoS0 requires an ordinary unreceipted TELL"));
        }
        frame.header.hop_count += 1;
        Ok(Work::Tell(frame))
    } else {
        if frame.header.flags != flags::TELL_ACK
            || frame.header.correlation_id == 0
            || frame.type_key != DURABLE_KEY
        {
            return Err(invalid(
                "QoS1 requires a receipted durable envelope with a stable producer sequence",
            ));
        }
        let envelope = DurableEnvelope::decode(frame.payload).map_err(BridgeError::Invalid)?;
        if envelope.source != config.producer
            || envelope.session != config.session
            || envelope.receiver != config.receiver.node_id
            || envelope.path != config.path
        {
            return Err(invalid(
                "durable producer/session/receiver does not match the authorized route",
            ));
        }
        Ok(Work::Durable {
            envelope,
            cid: frame.header.correlation_id,
        })
    }
}
async fn run(
    config: BridgeConfig,
    remote: Arc<RemoteActorSystem>,
    mut outbox: DurableOutbox,
    stop: CancellationToken,
    drain: CancellationToken,
    status: watch::Sender<BridgeStatus>,
) -> Result<(), BridgeError> {
    let (jobs, mut work) = mpsc::channel::<Job>(config.queue_capacity);
    let (results, completed) = mpsc::channel::<Completed>(1);
    let handle = outbox.handle();
    let worker = async {
        let result = async {
            loop {
                let job = tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    _ = drain.cancelled() => {
                        let last = handle.status().last_enqueued;
                        if last != 0 {
                            handle.wait_acked(last).await.map_err(|e| BridgeError::Storage(e.to_string()))?;
                        }
                        break;
                    }
                    job = work.recv() => match job { Some(job) => job, None => break },
                };
                let operation = async {
                    let receipt = match &job.work {
                        Work::Tell(frame) => {
                            if remote.sender_of(&config.receiver.node_id).is_none() {
                                remote.connect(&config.receiver).await.map_err(|e| BridgeError::Broker(e.to_string()))?;
                            }
                            let sender = remote.sender_of(&config.receiver.node_id).ok_or_else(|| BridgeError::Broker("remote link unavailable".into()))?;
                            sender.send(frame.clone()).await.map_err(|e| BridgeError::Broker(e.to_string()))?;
                            status.send_modify(|s| s.qos0_forwarded = s.qos0_forwarded.saturating_add(1));
                            None
                        }
                        Work::Durable { envelope, cid } => {
                            let seq = handle.enqueue_idempotent(&envelope.sequence.to_string(), &envelope.type_key, &envelope.payload).await.map_err(|e| BridgeError::Storage(e.to_string()))?;
                            status.send_modify(|s| s.durable_accepted = s.durable_accepted.saturating_add(1));
                            handle.wait_acked(seq).await.map_err(|e| BridgeError::Storage(e.to_string()))?;
                            let reply = Frame::reply(*cid, "", DURABLE_RECEIPT_KEY, Bytes::copy_from_slice(&envelope.sequence.to_le_bytes()));
                            let mut bytes = BytesMut::new();
                            reply.encode(&mut bytes).map_err(|_| BridgeError::Invalid("receipt encoding failed".into()))?;
                            Some(bytes.freeze())
                        }
                    };
                    results.send(Completed { job, receipt }).await.map_err(|_| BridgeError::Closed)
                };
                tokio::select! { biased; _ = stop.cancelled() => break, result = operation => result? }
            }
            Ok::<(), BridgeError>(())
        }.await;
        if result.is_err() {
            stop.cancel();
        }
        // All admitted storage work has an owner even if the public close waiter
        // is dropped. Closing retains unfinished durable messages for reopening.
        let closed = outbox
            .close()
            .await
            .map_err(|e| BridgeError::Storage(e.to_string()));
        drop(results);
        result.and(closed)
    };
    let network = async {
        let result = network(&config, jobs, completed, &stop, &drain, &status).await;
        if result.is_err() {
            stop.cancel();
        }
        result
    };
    let (worker, network) = tokio::join!(worker, network);
    worker.and(network)
}

async fn network(
    config: &BridgeConfig,
    jobs: mpsc::Sender<Job>,
    mut completed: mpsc::Receiver<Completed>,
    stop: &CancellationToken,
    drain: &CancellationToken,
    status: &watch::Sender<BridgeStatus>,
) -> Result<(), BridgeError> {
    let bytes = Arc::new(Semaphore::new(config.queue_bytes as usize));
    let mut jobs = Some(jobs);
    let mut generation = 0u64;
    let mut delay = config.retry_min;
    loop {
        if stop.is_cancelled() {
            return Ok(());
        }
        generation = generation.checked_add(1).ok_or(BridgeError::Worker)?;
        let (client, mut eventloop) = AsyncClient::new(config.mqtt.clone(), 8);
        let mut pending: Option<Completed> = None;
        let mut outgoing_id = None;
        let mut broker_acked = false;
        let mut results_done = false;
        status.send_modify(|s| {
            s.ready = false;
            s.generation = generation;
        });
        let session = 'session: loop {
            // rumqttc's connect/write poll is not cancellation-safe. Preserve
            // this exact future when a worker completion or drain signal wins.
            let event = {
                let poll = eventloop.poll();
                tokio::pin!(poll);
                loop {
                    tokio::select! {
                    biased;
                    _ = stop.cancelled() => return Ok(()),
                    _ = drain.cancelled(), if jobs.is_some() => {
                        jobs.take();
                        status.send_modify(|s| s.draining = true);
                    }
                    result = completed.recv(), if pending.is_none() && !results_done => {
                        match result {
                            Some(result) if result.job.generation == generation => {
                                if let Some(payload) = &result.receipt {
                                    client.try_publish(config.receipt_topic(), QoS::AtLeastOnce, false, payload.to_vec()).map_err(|_| BridgeError::Broker("receipt request queue unavailable".into()))?;
                                    pending = Some(result);
                                    outgoing_id = None;
                                    broker_acked = false;
                                }
                            }
                            Some(_) => {}, // A stale session must never ACK a reused packet ID.
                            None => results_done = true,
                        }
                    }
                    event = &mut poll => break event,
                        }
                    if results_done && pending.is_none() {
                        return Ok(());
                    }
                }
            };
            match event {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    if jobs.is_some() {
                        client
                            .try_subscribe(config.topic(), QoS::AtLeastOnce)
                            .map_err(|_| {
                                BridgeError::Broker("subscription request queue unavailable".into())
                            })?;
                    }
                }
                Ok(Event::Incoming(Packet::SubAck(ack))) => {
                    if ack.return_codes != [rumqttc::SubscribeReasonCode::Success(QoS::AtLeastOnce)]
                    {
                        return Err(BridgeError::Broker(
                            "broker refused the required QoS1 subscription".into(),
                        ));
                    }
                    delay = config.retry_min;
                    status.send_modify(|s| {
                        s.ready = true;
                        s.last_error = None;
                    });
                }
                Ok(Event::Incoming(Packet::Publish(mut publish))) => {
                    if let Some(jobs) = &jobs {
                        let size = publish
                            .payload
                            .len()
                            .checked_mul(3)
                            .and_then(|n| n.checked_add(2048))
                            .and_then(|n| u32::try_from(n).ok())
                            .ok_or_else(|| {
                                BridgeError::Invalid("packet accounting overflow".into())
                            })?;
                        let Ok(permit) = bytes.clone().try_acquire_many_owned(size) else {
                            break 'session "admission byte capacity exhausted";
                        };
                        let work = decode(config, &publish)?;
                        publish.payload = Bytes::new();
                        if jobs
                            .try_send(Job {
                                generation,
                                ack: publish,
                                work,
                                _bytes: permit,
                            })
                            .is_err()
                        {
                            break 'session "admission message capacity exhausted";
                        }
                    }
                    // During drain, leave newly arrived QoS1 unacknowledged.
                }
                Ok(Event::Outgoing(Outgoing::Publish(id))) if pending.is_some() => {
                    outgoing_id = Some(id)
                }
                Ok(Event::Incoming(Packet::PubAck(ack)))
                    if outgoing_id == Some(ack.pkid) && !broker_acked =>
                {
                    let Some(result) = &pending else {
                        return Err(BridgeError::Worker);
                    };
                    client.try_ack(&result.job.ack).map_err(|_| {
                        BridgeError::Broker("completion ACK queue unavailable".into())
                    })?;
                    broker_acked = true;
                }
                Ok(Event::Outgoing(Outgoing::PubAck(id)))
                    if broker_acked && pending.as_ref().is_some_and(|p| p.job.ack.pkid == id) =>
                {
                    pending.take();
                    outgoing_id = None;
                    status.send_modify(|s| {
                        s.completed_receipts = s.completed_receipts.saturating_add(1)
                    });
                }
                Ok(_) => {}
                Err(rumqttc::ConnectionError::ConnectionRefused(_)) => {
                    return Err(BridgeError::Broker("broker rejected the connection".into()))
                }
                Err(_) => break 'session "broker connection lost",
            }
            if results_done && pending.is_none() {
                return Ok(());
            }
        };
        // Throw away all queued protocol requests on every connection loss. An
        // old PUBACK must not acknowledge a new delivery reusing its packet ID.
        drop(eventloop);
        drop(client);
        drop(pending);
        status.send_modify(|s| {
            s.ready = false;
            s.reconnects = s.reconnects.saturating_add(1);
            s.last_error = Some(session.into());
        });
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(()),
                _ = drain.cancelled(), if jobs.is_some() => { jobs.take(); status.send_modify(|s| s.draining = true); }
                result = completed.recv(), if !results_done => { if result.is_none() { results_done = true; } }
                _ = &mut sleep => break,
            }
            if results_done {
                return Ok(());
            }
        }
        delay = delay.saturating_mul(2).min(config.retry_max);
    }
}
