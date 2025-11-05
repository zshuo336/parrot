//! One bounded, explicitly owned OTLP/HTTP JSON export session.
use super::{export_otlp_json, MAX_OTLP_JSON_BYTES, MAX_SERVICE_NAME_BYTES};
use bytes::Bytes;
use parrot_api::trace::{RecordingSnapshot, TraceContext, TraceRecorder, MAX_RECORD_CAPACITY};
use reqwest::{header, Client, StatusCode, Url};
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, task::JoinHandle, time::Instant};

#[derive(Clone)]
pub struct TraceExportConfig {
    pub endpoint: String,
    pub service_name: String,
    pub capacity: usize,
    pub interval: Duration,
    pub request_timeout: Duration,
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// Total HTTP attempts per batch, including its first attempt.
    pub max_attempts: u32,
    pub max_batch_bytes: usize,
    pub max_response_bytes: usize,
    /// Explicitly permit plaintext HTTP. HTTPS certificate verification remains on.
    pub allow_insecure_http: bool,
    pub headers: header::HeaderMap,
    /// Additional trusted DER certificates (at most 32, 128 KiB each, 1 MiB total).
    /// Built-in public roots and hostname/signature verification remain enabled.
    pub additional_root_certificates: Vec<Vec<u8>>,
}
impl TraceExportConfig {
    pub fn new(endpoint: impl Into<String>, service_name: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            service_name: service_name.into(),
            capacity: 4096,
            interval: Duration::from_secs(1),
            request_timeout: Duration::from_secs(5),
            retry_min: Duration::from_millis(100),
            retry_max: Duration::from_secs(30),
            max_attempts: 8,
            max_batch_bytes: 8 * 1024 * 1024,
            max_response_bytes: 64 * 1024,
            allow_insecure_http: false,
            headers: header::HeaderMap::new(),
            additional_root_certificates: Vec::new(),
        }
    }
    pub(crate) fn validate(&self) -> Result<Url, TraceExportStartError> {
        let invalid = || TraceExportStartError::Configuration;
        if self.endpoint.len() > 8192 {
            return Err(invalid());
        }
        let url = Url::parse(&self.endpoint).map_err(|_| invalid())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || url.port() == Some(0)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || (url.scheme() == "http" && !self.allow_insecure_http)
            || self.service_name.is_empty()
            || self.service_name.len() > MAX_SERVICE_NAME_BYTES
            || !(1..=MAX_RECORD_CAPACITY).contains(&self.capacity)
            || !(1..=MAX_OTLP_JSON_BYTES).contains(&self.max_batch_bytes)
            || !(1..=1024 * 1024).contains(&self.max_response_bytes)
            || !(1..=1024).contains(&self.max_attempts)
            || [
                self.interval,
                self.request_timeout,
                self.retry_min,
                self.retry_max,
            ]
            .iter()
            .any(|d| d.is_zero() || *d > Duration::from_secs(3600))
            || self.retry_min > self.retry_max
            || self.additional_root_certificates.len() > 32
            || self
                .additional_root_certificates
                .iter()
                .any(|c| c.is_empty() || c.len() > 128 * 1024)
            || self
                .additional_root_certificates
                .iter()
                .map(Vec::len)
                .sum::<usize>()
                > 1024 * 1024
            || self.headers.len() > 32
            || self
                .headers
                .iter()
                .map(|(k, v)| k.as_str().len() + v.len())
                .sum::<usize>()
                > 8192
            || [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::HOST,
                header::TRANSFER_ENCODING,
                header::CONNECTION,
                header::ACCEPT_ENCODING,
                header::CONTENT_ENCODING,
                header::ACCEPT,
                header::TRAILER,
                header::TE,
                header::UPGRADE,
                header::EXPECT,
            ]
            .iter()
            .any(|name| self.headers.contains_key(name))
        {
            return Err(invalid());
        }
        Ok(url)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TraceExportStartError {
    #[error("invalid OTLP exporter configuration")]
    Configuration,
    #[error("OTLP exporter requires an active Tokio runtime")]
    NoRuntime,
    #[error("unable to create OTLP HTTP client")]
    Client,
    #[error(transparent)]
    Recorder(#[from] parrot_api::trace::RecordingError),
}

/// No URL, authorization header, response text or arbitrary transport error is logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceExportFailure {
    Transport,
    Timeout,
    Http(u16),
    ResponseTooLarge,
    InvalidResponse,
    PartialRejected {
        rejected_spans: u64,
    },
    /// The collector requested a delay outside the configured retry budget.
    RetryDelay,
    Serialization,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceExportEnd {
    Complete,
    ShutdownDeadline,
    Failed(TraceExportFailure),
    AttemptsExhausted(TraceExportFailure),
}
#[derive(Clone, Debug, Default)]
pub struct TraceExportStatus {
    pub attempts: u64,
    pub accepted_batches: u64,
    pub accepted_spans: u64,
    pub retries: u64,
    pub accepted_with_warning: u64,
    pub pending_spans: usize,
    pub counters_overflowed: bool,
    pub last_failure: Option<TraceExportFailure>,
    pub end: Option<TraceExportEnd>,
}
fn add(value: &mut u64, by: u64, overflow: &mut bool) {
    match value.checked_add(by) {
        Some(n) => *value = n,
        None => {
            *value = u64::MAX;
            *overflow = true;
        }
    }
}

#[derive(Debug)]
pub struct TraceExportBatch {
    pub snapshot: RecordingSnapshot,
    /// Exact bytes retained for retries. None only when serialization failed.
    pub json: Option<Bytes>,
}
#[derive(Debug)]
pub struct TraceExportReport {
    pub end: TraceExportEnd,
    pub status: TraceExportStatus,
    /// Unconfirmed batch. A timeout/transport failure may already have delivered it.
    /// Partial rejection does not identify which individual spans were rejected.
    pub pending: Option<TraceExportBatch>,
    /// Non-consuming final view. Already admitted spans may still finish later;
    /// check terminal/in_flight and the session's snapshot before claiming completeness.
    pub remaining: RecordingSnapshot,
}

/// Only this worker drains its private recorder. Roots can be attached to real
/// application operations. No ambient/global context is installed.
pub struct TraceExporter {
    recorder: TraceRecorder,
    stop: watch::Sender<Option<Instant>>,
    status: watch::Receiver<TraceExportStatus>,
    worker: Option<JoinHandle<Arc<TraceExportReport>>>,
    report: Option<Arc<TraceExportReport>>,
    join_failure: Option<TraceExportJoinError>,
}
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("OTLP exporter task failed (panicked={panicked}, cancelled={cancelled})")]
pub struct TraceExportJoinError {
    pub panicked: bool,
    pub cancelled: bool,
}
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("OTLP exporter recording admission is closed")]
pub struct TraceExportAttachError;

impl TraceExporter {
    pub fn start(config: TraceExportConfig) -> Result<Self, TraceExportStartError> {
        Self::build(config, false).map(|(exporter, _)| exporter)
    }

    /// Retain a read-only rolling completion window of `config.capacity` records.
    /// Reading it never confirms, drains or interferes with an export batch.
    pub fn start_with_history(
        config: TraceExportConfig,
    ) -> Result<(Self, super::TraceHistory), TraceExportStartError> {
        Self::build(config, true)
            .map(|(exporter, history)| (exporter, history.expect("history requested")))
    }

    fn build(
        config: TraceExportConfig,
        retain_history: bool,
    ) -> Result<(Self, Option<super::TraceHistory>), TraceExportStartError> {
        let endpoint = config.validate()?;
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| TraceExportStartError::NoRuntime)?;
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .timeout(config.request_timeout)
            .default_headers(config.headers.clone());
        for der in &config.additional_root_certificates {
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_der(der)
                    .map_err(|_| TraceExportStartError::Configuration)?,
            );
        }
        let client = builder.build().map_err(|_| TraceExportStartError::Client)?;
        let recorder = TraceRecorder::new(config.capacity)?;
        let history =
            retain_history.then(|| super::TraceHistory::new(recorder.clone(), config.capacity));
        let (stop, stopping) = watch::channel(None);
        let (status_tx, status) = watch::channel(TraceExportStatus::default());
        let worker = runtime.spawn(work(
            config,
            endpoint,
            client,
            recorder.clone(),
            stopping,
            status_tx,
            history.clone(),
        ));
        Ok((
            Self {
                recorder,
                stop,
                status,
                worker: Some(worker),
                report: None,
                join_failure: None,
            },
            history,
        ))
    }
    /// Route future traced incoming requests into this worker's private recorder.
    /// Keep the returned attachment alive; dropping it stops future capture only.
    /// No draining or closing handle is exposed. The exporter may subsequently
    /// stop or fail; its ordinary admission counters and shutdown report apply.
    /// Quiesce receivers before shutdown to include all admitted operations.
    pub fn attach_remote(
        &self,
        remote: &parrot_remote::RemoteActorSystem,
    ) -> Result<parrot_remote::TraceRecorderAttachment, TraceExportAttachError> {
        self.attach_remote_with_policy(remote, parrot_remote::IncomingTracePolicy::PropagateOnly)
    }

    /// Attach with an explicit incoming-root policy. The default `attach_remote`
    /// continues to preserve None. Guard replacement, admission and shutdown
    /// ownership are identical under both policies.
    pub fn attach_remote_with_policy(
        &self,
        remote: &parrot_remote::RemoteActorSystem,
        policy: parrot_remote::IncomingTracePolicy,
    ) -> Result<parrot_remote::TraceRecorderAttachment, TraceExportAttachError> {
        if self.recorder.is_closed()
            || self
                .worker
                .as_ref()
                .is_none_or(|worker| worker.is_finished())
        {
            return Err(TraceExportAttachError);
        }
        Ok(remote.attach_trace_recorder_with_policy(self.recorder.clone(), policy))
    }

    /// Wait until the worker stops without requesting shutdown or consuming its
    /// receipt. Cancellation is safe. An absent end means the status producer
    /// disappeared unexpectedly; call `shutdown` to obtain the join error.
    pub async fn wait_until_stopped(&self) -> TraceExportStatus {
        let mut status = self.status.clone();
        loop {
            let current = status.borrow_and_update().clone();
            if current.end.is_some() || status.changed().await.is_err() {
                return status.borrow().clone();
            }
        }
    }

    pub fn root(&self) -> TraceContext {
        self.recorder.root()
    }
    pub fn snapshot(&self) -> RecordingSnapshot {
        self.recorder.snapshot()
    }
    pub fn status(&self) -> TraceExportStatus {
        self.status.borrow().clone()
    }
    /// Close recording admission and wait for export until the grace deadline.
    /// Cancelling this waiter retains the owned task; a later call resumes the
    /// join and cannot extend an already requested deadline. The deadline bounds
    /// network waits. An already owned, bounded serialization job is joined even
    /// after expiry so it cannot detach or lose its drained snapshot. Keep the
    /// Tokio runtime alive until this method completes.
    pub async fn shutdown(
        &mut self,
        grace: Duration,
    ) -> Result<Arc<TraceExportReport>, TraceExportJoinError> {
        self.recorder.close();
        let deadline = Instant::now() + grace.min(Duration::from_secs(3600));
        self.stop
            .send_modify(|old| *old = Some(old.map_or(deadline, |d| d.min(deadline))));
        if let Some(report) = &self.report {
            return Ok(report.clone());
        }
        if let Some(error) = self.join_failure {
            return Err(error);
        }
        let result = self.worker.as_mut().expect("owned exporter worker").await;
        self.worker.take();
        let result = match result {
            Ok(report) => report,
            Err(error) => {
                let error = TraceExportJoinError {
                    panicked: error.is_panic(),
                    cancelled: error.is_cancelled(),
                };
                self.join_failure = Some(error);
                return Err(error);
            }
        };
        self.report = Some(result.clone());
        Ok(result)
    }
}
impl Drop for TraceExporter {
    fn drop(&mut self) {
        self.recorder.close();
        self.stop.send_replace(Some(Instant::now()));
        // The worker observes this deadline in idle, backoff and response waits.
        // Drop cannot synchronously join; explicit shutdown returns the receipt.
    }
}

enum Delivery {
    Accepted { warning: bool },
    Retry(TraceExportFailure, Option<Duration>),
    Failed(TraceExportFailure),
    Stopped,
}
async fn response(
    client: &Client,
    endpoint: &Url,
    body: Bytes,
    config: &TraceExportConfig,
    spans: usize,
) -> Delivery {
    let response = client
        .post(endpoint.clone())
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .body(body)
        .send()
        .await;
    let mut response = match response {
        Ok(r) => r,
        Err(e) => {
            return Delivery::Retry(
                if e.is_timeout() {
                    TraceExportFailure::Timeout
                } else {
                    TraceExportFailure::Transport
                },
                None,
            )
        }
    };
    let code = response.status();
    if matches!(code.as_u16(), 429 | 502 | 503 | 504) {
        let delay = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| {
                h.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
                    httpdate::parse_http_date(h).ok().map(|at| {
                        at.duration_since(std::time::SystemTime::now())
                            .unwrap_or_default()
                    })
                })
            });
        if delay.is_some_and(|d| d > config.retry_max) {
            return Delivery::Failed(TraceExportFailure::RetryDelay);
        }
        return Delivery::Retry(TraceExportFailure::Http(code.as_u16()), delay);
    }
    if code != StatusCode::OK {
        return Delivery::Failed(TraceExportFailure::Http(code.as_u16()));
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split(';').next())
        .map(str::trim);
    if !content_type.is_some_and(|t| t.eq_ignore_ascii_case("application/json")) {
        return Delivery::Failed(TraceExportFailure::InvalidResponse);
    }
    if response
        .content_length()
        .is_some_and(|n| n > config.max_response_bytes as u64)
    {
        return Delivery::Failed(TraceExportFailure::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if chunk.len() > config.max_response_bytes.saturating_sub(bytes.len()) {
                    return Delivery::Failed(TraceExportFailure::ResponseTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                return Delivery::Retry(
                    if e.is_timeout() {
                        TraceExportFailure::Timeout
                    } else {
                        TraceExportFailure::Transport
                    },
                    None,
                )
            }
        }
    }
    // Serde's derived struct deserializer also accepts positional arrays.
    // OTLP JSON messages must be objects; null fields retain protobuf defaults.
    let shape: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Delivery::Failed(TraceExportFailure::InvalidResponse),
    };
    if !shape.is_object()
        || shape
            .get("partialSuccess")
            .is_some_and(|p| !p.is_null() && !p.is_object())
    {
        return Delivery::Failed(TraceExportFailure::InvalidResponse);
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Reply {
        #[serde(default)]
        partial_success: Option<Partial>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Partial {
        #[serde(default)]
        rejected_spans: Option<Count>,
        #[serde(default)]
        error_message: Option<String>,
    }
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Count {
        Number(u64),
        String(String),
    }
    impl Default for Count {
        fn default() -> Self {
            Self::Number(0)
        }
    }
    let reply: Reply = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(_) => return Delivery::Failed(TraceExportFailure::InvalidResponse),
    };
    let mut warning = false;
    if let Some(partial) = reply.partial_success {
        let rejected = match partial.rejected_spans.unwrap_or_default() {
            Count::Number(n) => Some(n),
            Count::String(s) => s.parse::<u64>().ok(),
        };
        warning = partial
            .error_message
            .is_some_and(|message| !message.is_empty());
        match rejected {
            Some(0) => {}
            Some(n) if n <= spans as u64 => {
                return Delivery::Failed(TraceExportFailure::PartialRejected { rejected_spans: n })
            }
            _ => return Delivery::Failed(TraceExportFailure::InvalidResponse),
        }
    }
    Delivery::Accepted { warning }
}

fn expired(stop: &watch::Receiver<Option<Instant>>) -> bool {
    stop.borrow().is_some_and(|d| Instant::now() >= d)
}
async fn wait_until(at: Instant, stop: &mut watch::Receiver<Option<Instant>>, flush_on_stop: bool) {
    loop {
        let deadline = stop.borrow().map_or(at, |d| d.min(at));
        tokio::select! { biased;
            changed = stop.changed() => { if changed.is_err() || flush_on_stop { return; } },
            _ = tokio::time::sleep_until(deadline) => return,
        }
    }
}
async fn deliver(
    client: &Client,
    endpoint: &Url,
    pending: &TraceExportBatch,
    config: &TraceExportConfig,
    stop: &mut watch::Receiver<Option<Instant>>,
) -> Delivery {
    let request_deadline = Instant::now() + config.request_timeout;
    let request = response(
        client,
        endpoint,
        pending.json.as_ref().unwrap().clone(),
        config,
        pending.snapshot.records.len(),
    );
    tokio::pin!(request);
    loop {
        let deadline = stop
            .borrow()
            .map_or(request_deadline, |d| d.min(request_deadline));
        tokio::select! { biased;
            changed = stop.changed() => { if changed.is_err() { return Delivery::Stopped; } },
            _ = tokio::time::sleep_until(deadline) => return if expired(stop) { Delivery::Stopped } else { Delivery::Retry(TraceExportFailure::Timeout, None) },
            result = &mut request => return result,
        }
    }
}
async fn work(
    config: TraceExportConfig,
    endpoint: Url,
    client: Client,
    recorder: TraceRecorder,
    mut stop: watch::Receiver<Option<Instant>>,
    status: watch::Sender<TraceExportStatus>,
    history: Option<super::TraceHistory>,
) -> Arc<TraceExportReport> {
    let mut pending: Option<TraceExportBatch> = None;
    let mut attempts = 0;
    let mut delay = config.retry_min;
    let end = loop {
        if expired(&stop) {
            let snapshot = recorder.snapshot();
            break if pending.is_none() && snapshot.terminal && snapshot.records.is_empty() {
                TraceExportEnd::Complete
            } else {
                TraceExportEnd::ShutdownDeadline
            };
        }
        if pending.is_none() {
            let snapshot = match &history {
                Some(history) => history.drain(),
                None => recorder.drain(),
            };
            if snapshot.records.is_empty() {
                if snapshot.terminal {
                    break TraceExportEnd::Complete;
                }
                // Closing admission may leave accepted spans in flight. The
                // ordinary interval must not prevent their final flush.
                let interval = if stop.borrow().is_some() {
                    config.interval.min(Duration::from_millis(10))
                } else {
                    config.interval
                };
                wait_until(Instant::now() + interval, &mut stop, true).await;
                continue;
            }
            status.send_modify(|s| s.pending_spans = snapshot.records.len());
            // Keep an independent owner until the blocking task is joined, including
            // panic/cancellation. No serialization runs on the actor scheduler.
            let snapshot = Arc::new(snapshot);
            let retained = snapshot.clone();
            let service = config.service_name.clone();
            let limit = config.max_batch_bytes;
            let bytes = tokio::task::spawn_blocking(move || {
                export_otlp_json(&retained, &service, limit).map(Bytes::from)
            })
            .await
            .ok()
            .and_then(Result::ok);
            let snapshot = Arc::try_unwrap(snapshot).expect("serialization task joined");
            pending = Some(TraceExportBatch {
                snapshot,
                json: bytes,
            });
            if pending.as_ref().unwrap().json.is_none() {
                break TraceExportEnd::Failed(TraceExportFailure::Serialization);
            }
            attempts = 0;
            delay = config.retry_min;
        }
        if expired(&stop) {
            break TraceExportEnd::ShutdownDeadline;
        }
        attempts += 1;
        status.send_modify(|s| add(&mut s.attempts, 1, &mut s.counters_overflowed));
        match deliver(
            &client,
            &endpoint,
            pending.as_ref().unwrap(),
            &config,
            &mut stop,
        )
        .await
        {
            Delivery::Accepted { warning } => {
                let spans = pending.take().unwrap().snapshot.records.len() as u64;
                status.send_modify(|s| {
                    add(&mut s.accepted_batches, 1, &mut s.counters_overflowed);
                    add(&mut s.accepted_spans, spans, &mut s.counters_overflowed);
                    if warning {
                        add(&mut s.accepted_with_warning, 1, &mut s.counters_overflowed);
                    }
                    s.pending_spans = 0;
                    s.last_failure = None;
                });
            }
            Delivery::Stopped => break TraceExportEnd::ShutdownDeadline,
            Delivery::Failed(error) => break TraceExportEnd::Failed(error),
            Delivery::Retry(error, suggested) => {
                status.send_modify(|s| s.last_failure = Some(error.clone()));
                if attempts >= config.max_attempts {
                    break TraceExportEnd::AttemptsExhausted(error);
                }
                status.send_modify(|s| add(&mut s.retries, 1, &mut s.counters_overflowed));
                wait_until(
                    Instant::now() + suggested.unwrap_or(delay).max(config.retry_min),
                    &mut stop,
                    false,
                )
                .await;
                delay = delay.saturating_mul(2).min(config.retry_max);
            }
        }
    };
    recorder.close();
    status.send_modify(|s| {
        s.end = Some(end.clone());
        if let TraceExportEnd::Failed(error) | TraceExportEnd::AttemptsExhausted(error) = &end {
            s.last_failure = Some(error.clone());
        }
    });
    Arc::new(TraceExportReport {
        end,
        status: status.borrow().clone(),
        pending,
        remaining: recorder.snapshot(),
    })
}
