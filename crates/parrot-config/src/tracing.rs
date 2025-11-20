//! Declarative application tracing. Loading configuration never starts a recorder.
use crate::ConfigError;
use std::path::PathBuf;

pub const DEFAULT_CAPACITY: usize = 4096;
pub const DEFAULT_INTERVAL_MS: u64 = 1000;
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 5000;
pub const DEFAULT_MAX_ATTEMPTS: u32 = 8;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TracingSection {
    pub enabled: Option<bool>,
    pub otlp_endpoint: Option<String>,
    pub allow_insecure_http: Option<bool>,
    pub capacity: Option<usize>,
    pub socket: Option<PathBuf>,
    pub interval_ms: Option<u64>,
    pub request_timeout_ms: Option<u64>,
    pub max_attempts: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct TracingResolved {
    pub enabled: bool,
    pub otlp_endpoint: Option<String>,
    pub allow_insecure_http: bool,
    pub capacity: usize,
    pub socket: Option<PathBuf>,
    pub interval_ms: u64,
    pub request_timeout_ms: u64,
    pub max_attempts: u32,
}

impl TracingSection {
    pub(crate) fn resolve(self) -> Result<TracingResolved, ConfigError> {
        let invalid = |key: &str, reason: &str| ConfigError::Invalid {
            key: format!("tracing.{key}"),
            reason: reason.into(),
        };
        let enabled = self.enabled.unwrap_or(self.otlp_endpoint.is_some());
        if enabled && self.otlp_endpoint.is_none() {
            return Err(invalid("otlp_endpoint", "required when tracing is enabled"));
        }
        if self.enabled != Some(false)
            && self.otlp_endpoint.is_none()
            && (self.allow_insecure_http.is_some()
                || self.capacity.is_some()
                || self.socket.is_some()
                || self.interval_ms.is_some()
                || self.request_timeout_ms.is_some()
                || self.max_attempts.is_some())
        {
            return Err(invalid(
                "otlp_endpoint",
                "tracing options require an endpoint or explicit enabled=false",
            ));
        }
        if self.otlp_endpoint.as_ref().is_some_and(|value| {
            value.is_empty() || value.len() > 8192 || value.chars().any(char::is_control)
        }) {
            return Err(invalid(
                "otlp_endpoint",
                "must be nonempty, bounded and free of control characters",
            ));
        }
        if self.socket.as_ref().is_some_and(|path| {
            let value = path.as_os_str().as_encoded_bytes();
            value.is_empty() || value.len() > 4096 || value.iter().any(u8::is_ascii_control)
        }) {
            return Err(invalid(
                "socket",
                "must be nonempty, bounded and free of control characters",
            ));
        }
        let capacity = self.capacity.unwrap_or(DEFAULT_CAPACITY);
        if !(1..=parrot_api::trace::MAX_RECORD_CAPACITY).contains(&capacity) {
            return Err(invalid("capacity", "must be in 1..=65536"));
        }
        let interval_ms = self.interval_ms.unwrap_or(DEFAULT_INTERVAL_MS);
        let request_timeout_ms = self
            .request_timeout_ms
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS);
        for (key, value) in [
            ("interval_ms", interval_ms),
            ("request_timeout_ms", request_timeout_ms),
        ] {
            if !(1..=3_600_000).contains(&value) {
                return Err(invalid(key, "must be in 1..=3600000"));
            }
        }
        let max_attempts = self.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS);
        if !(1..=1024).contains(&max_attempts) {
            return Err(invalid("max_attempts", "must be in 1..=1024"));
        }
        Ok(TracingResolved {
            enabled,
            otlp_endpoint: self.otlp_endpoint,
            allow_insecure_http: self.allow_insecure_http.unwrap_or(false),
            capacity,
            socket: self.socket,
            interval_ms,
            request_timeout_ms,
            max_attempts,
        })
    }
}
