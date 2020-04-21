// Logging System for Parrot
// This module provides a unified logging interface for the Parrot actor system.
// It's built on top of the `tracing` ecosystem, which offers structured logging and
// distributed tracing capabilities.

use std::io;
use std::sync::Once;
use tracing::{Level, Metadata, Subscriber};
use tracing_subscriber::{
    filter::LevelFilter, fmt, prelude::*, registry::Registry, EnvFilter, Layer,
};

/// Configuration for the Parrot logging system
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Minimum log level to display
    pub level: Level,
    /// Whether to use JSON format for logs
    pub json_format: bool,
    /// Whether to include file and line information
    pub show_file_line: bool,
    /// Whether to include thread name/id
    pub show_thread_info: bool,
    /// Whether to include timestamps
    pub show_time: bool,
    /// Target filter expressions (format: "target=level,target2=level2,...")
    pub target_filters: Option<String>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: Level::INFO,
            json_format: false,
            show_file_line: true,
            show_thread_info: true,
            show_time: true,
            target_filters: None,
        }
    }
}

// Initialization guard to ensure we only initialize once
static INIT: Once = Once::new();

/// Initialize the logging system with the given configuration
pub fn init(config: LogConfig) {
    INIT.call_once(|| {
        let mut env_filter = EnvFilter::from_default_env()
            .add_directive(config.level.into());

        // Add any target-specific filters if provided
        if let Some(filters) = config.target_filters {
            for filter in filters.split(',') {
                if let Ok(directive) = filter.parse() {
                    env_filter = env_filter.add_directive(directive);
                }
            }
        }

        let fmt_layer = fmt::layer()
            .with_ansi(atty::is(atty::Stream::Stdout))
            .with_file(config.show_file_line)
            .with_line_number(config.show_file_line)
            .with_thread_names(config.show_thread_info)
            .with_thread_ids(config.show_thread_info)
            .with_timer(if config.show_time {
                fmt::time::uptime()
            } else {
                fmt::time::disabled()
            });

        let subscriber = if config.json_format {
            Registry::default()
                .with(env_filter)
                .with(fmt::layer().json().flatten_event(true))
                .boxed()
        } else {
            Registry::default()
                .with(env_filter)
                .with(fmt_layer)
                .boxed()
        };

        set_global_subscriber(subscriber);
    });
}

// Helper function to set the global subscriber
fn set_global_subscriber<S>(subscriber: S) 
where
    S: Subscriber + Send + Sync + 'static,
{
    if let Err(err) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("Error setting global tracing subscriber: {}", err);
    }
}

/// Initialize default logging for the Parrot system
/// 
/// This sets up a reasonable default configuration that works well for most cases.
pub fn init_default() {
    init(LogConfig::default());
}

/// Initialize logging optimized for development environments
/// 
/// Shows detailed logs with colors and location information
pub fn init_development() {
    let config = LogConfig {
        level: Level::DEBUG,
        json_format: false,
        show_file_line: true,
        show_thread_info: true,
        show_time: true,
        target_filters: Some("parrot=debug,parrot::thread=trace".to_string()),
    };
    init(config);
}

/// Initialize logging optimized for production environments
/// 
/// Uses JSON format, omits file/line information for security, focuses on essential information
pub fn init_production() {
    let config = LogConfig {
        level: Level::INFO, 
        json_format: true,
        show_file_line: false,
        show_thread_info: true,
        show_time: true,
        target_filters: None,
    };
    init(config);
}

/// Initialize logging for testing
/// 
/// Only shows warnings and errors by default to keep test output clean
pub fn init_test() {
    let config = LogConfig {
        level: Level::WARN,
        json_format: false,
        show_file_line: true,
        show_thread_info: false,
        show_time: false,
        target_filters: None,
    };
    init(config);
}

/// Utility function to create a file writer for logs
/// 
/// # Arguments
/// * `path` - Path to the log file
/// 
/// # Returns
/// A writer that can be used with a `fmt::Layer`
pub fn file_writer(path: &str) -> io::Result<impl io::Write + Send + Sync + 'static> {
    use std::fs::OpenOptions;
    
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Initialize logging with both console and file output
/// 
/// # Arguments
/// * `config` - Base logging configuration
/// * `log_file` - Path to the log file
pub fn init_with_file(config: LogConfig, log_file: &str) -> Result<(), io::Error> {
    INIT.call_once(|| {
        let env_filter = EnvFilter::from_default_env()
            .add_directive(config.level.into());

        // Console layer
        let console_layer = fmt::layer()
            .with_ansi(atty::is(atty::Stream::Stdout))
            .with_file(config.show_file_line)
            .with_line_number(config.show_file_line)
            .with_thread_names(config.show_thread_info)
            .with_thread_ids(config.show_thread_info);

        // File layer
        let file_appender = match file_writer(log_file) {
            Ok(writer) => writer,
            Err(e) => {
                eprintln!("Failed to open log file: {}", e);
                return;
            }
        };

        let file_layer = fmt::layer()
            .with_ansi(false)  // No ANSI colors in files
            .with_writer(file_appender)
            .with_file(true)   // Always include file info in the log file
            .with_line_number(true)
            .with_thread_names(true)
            .with_thread_ids(true);

        let subscriber = Registry::default()
            .with(env_filter)
            .with(console_layer)
            .with(file_layer)
            .boxed();

        set_global_subscriber(subscriber);
    });
    
    Ok(())
}

/// Create a new span for actor operations
/// 
/// This is a convenience wrapper around tracing's span macros
/// for creating spans specifically for actor operations
#[macro_export]
macro_rules! actor_span {
    ($actor_type:expr, $actor_id:expr) => {
        tracing::info_span!("actor", type = $actor_type, id = $actor_id)
    };
    ($actor_type:expr, $actor_id:expr, $($fields:tt)*) => {
        tracing::info_span!("actor", type = $actor_type, id = $actor_id, $($fields)*)
    };
}

/// Create a new span for message processing
#[macro_export]
macro_rules! message_span {
    ($message_type:expr) => {
        tracing::debug_span!("message", type = $message_type)
    };
    ($message_type:expr, $($fields:tt)*) => {
        tracing::debug_span!("message", type = $message_type, $($fields)*)
    };
}

/// Create a new span for system operations
#[macro_export]
macro_rules! system_span {
    ($operation:expr) => {
        tracing::info_span!("system", operation = $operation)
    };
    ($operation:expr, $($fields:tt)*) => {
        tracing::info_span!("system", operation = $operation, $($fields)*)
    };
}

/// Log actor lifecycle events - use for important actor state changes
#[macro_export]
macro_rules! log_lifecycle {
    ($actor_type:expr, $actor_id:expr, $event:expr) => {
        tracing::info!(actor_type = $actor_type, actor_id = $actor_id, event = $event);
    };
    ($actor_type:expr, $actor_id:expr, $event:expr, $($fields:tt)*) => {
        tracing::info!(actor_type = $actor_type, actor_id = $actor_id, event = $event, $($fields)*);
    };
}

/// Log message processing events - use for detailed message handling
#[macro_export]
macro_rules! log_message {
    ($message_type:expr, $status:expr) => {
        tracing::debug!(message_type = $message_type, status = $status);
    };
    ($message_type:expr, $status:expr, $($fields:tt)*) => {
        tracing::debug!(message_type = $message_type, status = $status, $($fields)*);
    };
}

/// Log system events - use for important system state changes
#[macro_export]
macro_rules! log_system {
    ($operation:expr, $status:expr) => {
        tracing::info!(operation = $operation, status = $status);
    };
    ($operation:expr, $status:expr, $($fields:tt)*) => {
        tracing::info!(operation = $operation, status = $status, $($fields)*);
    };
}

/// Log error events - use for all error conditions
#[macro_export]
macro_rules! log_error {
    ($error:expr) => {
        tracing::error!(error = %$error);
    };
    ($error:expr, $($fields:tt)*) => {
        tracing::error!(error = %$error, $($fields)*);
    };
}

/// Log scheduling events
#[macro_export]
macro_rules! log_scheduler {
    ($scheduler:expr, $event:expr) => {
        tracing::debug!(scheduler = $scheduler, event = $event);
    };
    ($scheduler:expr, $event:expr, $($fields:tt)*) => {
        tracing::debug!(scheduler = $scheduler, event = $event, $($fields)*);
    };
}

/// Get the current tracing subscriber to allow installing it in other threads
/// 
/// This is useful when spawning threads that need access to the current subscriber
#[inline]
pub fn current_subscriber() -> impl Subscriber + Send + Sync + 'static {
    tracing::dispatcher::get_default(|d| d.clone())
}

// Re-export the most commonly used tracing macros for convenience
pub use tracing::{debug, error, info, trace, warn}; 