//! Logging configuration shared by console and file output.
//!
//! Initialize once at process startup; subsequent successful Parrot initialization
//! calls are no-ops. A failed file open can be retried. For recoverable startup
//! errors, use [`init_with_file`] instead of the convenience functions that panic.
//!
//! ```rust
//! use parrot::logging;
//! logging::init_default(false);
//! logging::info!("application started");
//! ```
//!
//! Actor macros are exported at the crate root:
//!
//! ```rust
//! use parrot::{actor_span, log_lifecycle};
//! let span = actor_span!("worker", "worker-1");
//! let _entered = span.enter();
//! log_lifecycle!("worker", "worker-1", "started");
//! ```
//!
//! File output follows the same format and filter configuration as console output:
//!
//! ```no_run
//! use parrot::logging::{init_with_file, LogConfig};
//! # fn main() -> std::io::Result<()> {
//! init_with_file(LogConfig { json_format: true, ..Default::default() }, "service.log")?;
//! # Ok(())
//! # }
//! ```

use std::io;
use std::io::IsTerminal;
use std::sync::Mutex;
use tracing::{Level, Subscriber};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Configuration for the Parrot logging system
///
/// This struct allows customizing the behavior of the logging system.
///
/// # Examples
///
/// ```rust
/// use parrot::logging::LogConfig;
/// use tracing::Level;
///
/// // Default configuration
/// let default_config = LogConfig::default();
///
/// // Custom configuration
/// let custom_config = LogConfig {
///     level: Level::DEBUG,
///     json_format: true,
///     show_file_line: false,
///     show_thread_info: true,
///     show_time: true,
///     target_filters: Some("parrot=debug,parrot::system=trace".to_string()),
/// };
/// ```
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
static INIT: Mutex<bool> = Mutex::new(false);

/// Initialize the logging system with the given configuration
///
/// This function sets up the global tracing subscriber with the specified configuration.
/// It's safe to call multiple times; only the first call will take effect.
///
/// # Parameters
/// * `config` - The logging configuration to apply
///
/// # Examples
///
/// ```rust
/// use parrot::logging::{init, LogConfig};
///
/// // Initialize with custom config
/// let config = LogConfig {
///     level: tracing::Level::DEBUG,
///     ..Default::default()
/// };
/// init(config);
/// ```
pub fn init(config: LogConfig) {
    if let Err(error) = initialize(config, None) {
        eprintln!("Error setting global tracing subscriber: {error}");
    }
}

fn format_layer<S, W>(
    config: &LogConfig,
    writer: W,
    ansi: bool,
) -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a> + 'static,
    W: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let layer = fmt::layer()
        .with_writer(writer)
        .with_ansi(ansi && !config.json_format)
        .with_file(config.show_file_line)
        .with_line_number(config.show_file_line)
        .with_thread_names(config.show_thread_info)
        .with_thread_ids(config.show_thread_info);
    if config.json_format {
        let layer = layer.json().flatten_event(true);
        if config.show_time {
            Box::new(layer)
        } else {
            Box::new(layer.without_time())
        }
    } else if config.show_time {
        Box::new(layer)
    } else {
        Box::new(layer.without_time())
    }
}

fn initialize(config: LogConfig, log_file: Option<&str>) -> io::Result<()> {
    let mut initialized = INIT
        .lock()
        .map_err(|_| io::Error::other("logging initialization lock poisoned"))?;
    if *initialized {
        return Ok(());
    }
    // Open once before reporting success. A failed open leaves initialization retryable.
    let writer = log_file.map(file_writer).transpose()?;
    let mut filter = EnvFilter::builder()
        .with_default_directive(config.level.into())
        .from_env_lossy();
    if let Some(filters) = &config.target_filters {
        for filter_text in filters.split(',') {
            if let Ok(directive) = filter_text.parse() {
                filter = filter.add_directive(directive);
            }
        }
    }
    let console = format_layer(&config, io::stdout, io::stdout().is_terminal());
    let file = writer.map(|writer| format_layer(&config, Mutex::new(writer), false));
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|error| io::Error::new(io::ErrorKind::AlreadyExists, error))?;
    *initialized = true;
    Ok(())
}

/// Utility function to create a file writer for logs
///
/// Creates a boxed writer that can be used with tracing formatters.
/// The writer opens a file in append mode, creating it if it doesn't exist.
///
/// # Arguments
/// * `path` - Path to the log file
///
/// # Returns
/// A boxed writer that can be used with a `fmt::Layer`
///
/// # Errors
/// Returns an error if the file cannot be opened or created
pub fn file_writer(path: &str) -> io::Result<Box<dyn io::Write + Send + Sync + 'static>> {
    use std::fs::OpenOptions;

    let file = OpenOptions::new().create(true).append(true).open(path)?;

    Ok(Box::new(file))
}

/// Initialize logging with both console and file output
///
/// This function sets up logging to both console and a specified file path.
/// Both outputs use the requested format, timestamp, location and thread settings.
/// The file never contains ANSI colors. It is opened once in append mode before
/// success is returned; an open failure leaves initialization available for retry.
/// After a successful Parrot initialization, subsequent calls are no-ops.
/// A global subscriber installed by another library returns `AlreadyExists`.
///
/// # Arguments
/// * `config` - Base logging configuration
/// * `log_file` - Path to the log file
///
/// # Returns
/// Result indicating success or an IO error
///
/// # Examples
///
/// ```no_run
/// use parrot::logging::{self, LogConfig};
///
/// let config = LogConfig::default();
/// logging::init_with_file(config, "application.log").expect("Failed to set up file logging");
/// ```
pub fn init_with_file(config: LogConfig, log_file: &str) -> Result<(), io::Error> {
    initialize(config, Some(log_file))
}

/// Initialize default logging for the Parrot system
///
/// This sets up a reasonable default configuration that works well for most cases.
/// It uses INFO level logging with human-readable console output. If `use_file`
/// is true, it also appends to [`DEFAULT_LOG_FILE`] in the current directory.
/// For an explicit path or recoverable errors, use [`init_with_file`].
///
/// # Panics
/// Panics if requested file initialization fails. Subsequent calls after successful
/// Parrot initialization are no-ops, including changes to `use_file`.
///
/// # Examples
///
/// ```rust
/// use parrot::logging;
///
/// // Simple initialization with defaults
/// logging::init_default(false);
///
/// // Now you can use logging macros
/// logging::info!("Application started");
/// ```
pub fn init_default(use_file: bool) {
    if use_file {
        init_default_with_file(DEFAULT_LOG_FILE);
    } else {
        init(LogConfig::default());
    }
}

/// File used by [`init_default(true)`](init_default), relative to the current directory.
pub const DEFAULT_LOG_FILE: &str = "parrot.log";

/// Initialize logging with default settings and a file output
///
/// This function sets up logging with default settings and a file output.
/// The file is opened in append mode, creating it if it doesn't exist.
///
/// # Arguments
/// * `path` - Path to the log file
///
/// # Examples
///
/// ```no_run
/// use parrot::logging;
///
/// logging::init_default_with_file("application.log");
/// ```
pub fn init_default_with_file(path: &str) {
    let config = LogConfig::default();
    init_with_file(config, path).unwrap();
}

/// Initialize logging optimized for development environments
///
/// Shows detailed logs with colors and location information.
/// This configuration includes:
/// - DEBUG level for all Parrot modules
/// - TRACE level for thread-related operations
/// - Colorized console output with file/line information
/// - Thread names and IDs displayed
/// - Timestamps enabled
///
/// # Examples
///
/// ```rust
/// use parrot::logging;
///
/// // Initialize with development-friendly settings
/// logging::init_development();
/// ```
pub fn init_development() {
    init(development_config());
}

/// Initialize logging with development settings and a file output
///
/// This function sets up logging with development settings and a file output.
/// The file is opened in append mode, creating it if it doesn't exist.
///
/// # Arguments
/// * `path` - Path to the log file
///
/// # Examples
///
/// ```no_run
/// use parrot::logging;
///
/// // Initialize with development-friendly settings and a file output
/// logging::init_development_with_file("application.log");
/// ```
pub fn init_development_with_file(path: &str) {
    let config = development_config();
    init_with_file(config, path).unwrap();
}

/// Initialize logging optimized for production environments
///
/// Uses JSON format, omits file/line information for security, focuses on essential information.
/// This configuration includes:
/// - INFO level for all modules (can be overridden with environment variables)
/// - JSON formatted output for easy parsing by log aggregators
/// - No file/line information (for security and performance)
/// - Thread information included for diagnostic purposes
/// - Timestamps enabled
///
/// # Examples
///
/// ```rust
/// use parrot::logging;
///
/// // Initialize with production-optimized settings
/// logging::init_production();
/// ```
pub fn init_production() {
    init(production_config());
}

/// Initialize logging with production settings and a file output
///
/// This function sets up logging with production settings and a file output.
/// The file is opened in append mode, creating it if it doesn't exist.
///
/// # Arguments
/// * `path` - Path to the log file
///
/// # Examples
///
/// ```no_run
/// use parrot::logging;
///
/// // Initialize with production-optimized settings and a file output
/// logging::init_production_with_file("application.log");
/// ```
pub fn init_production_with_file(path: &str) {
    let config = production_config();
    init_with_file(config, path).unwrap();
}

/// Initialize logging for testing
///
/// Only shows warnings and errors by default to keep test output clean.
/// This configuration includes:
/// - WARN level for all modules
/// - Plain text format for readability
/// - File/line information for debugging test failures
/// - No thread information to reduce noise
/// - No timestamps to keep output compact
///
/// # Examples
///
/// ```rust
/// use parrot::logging;
/// logging::init_test();
/// logging::warn!("test diagnostic");
/// ```
pub fn init_test() {
    init(test_config());
}

/// Initialize logging for testing with a file output
///
/// This function sets up logging for testing with a file output.
/// The file is opened in append mode, creating it if it doesn't exist.
///
/// # Arguments
/// * `path` - Path to the log file
///
/// # Examples
///
/// ```no_run
/// use parrot::logging;
/// logging::init_test_with_file("test.log");
/// logging::warn!("test diagnostic");
/// ```
pub fn init_test_with_file(path: &str) {
    let config = test_config();
    init_with_file(config, path).unwrap();
}

/// Create a new span for actor operations
///
/// This is a convenience wrapper around tracing's span macros
/// for creating spans specifically for actor operations.
///
/// # Examples
///
/// ```rust
/// use parrot::actor_span;
///
/// // Create a basic actor span
/// let span = actor_span!("user_actor", "user-123");
/// let _guard = span.enter();
///
/// // With additional fields
/// let span = actor_span!("order_actor", "order-456", status = "processing");
/// ```
#[macro_export]
macro_rules! actor_span {
    ($actor_type:expr, $actor_id:expr) => {
        $crate::logging::info_span!("actor", type = $actor_type, id = $actor_id)
    };
    ($actor_type:expr, $actor_id:expr, $($fields:tt)*) => {
        $crate::logging::info_span!("actor", type = $actor_type, id = $actor_id, $($fields)*)
    };
}

/// Create a new span for message processing
///
/// Use this macro to trace message handling within actors.
///
/// # Examples
///
/// ```rust
/// use parrot::message_span;
///
/// // Create a basic message span
/// let span = message_span!("CreateUser");
/// let _guard = span.enter();
///
/// // With additional fields
/// let span = message_span!("UpdateOrder", order_id = "order-456");
/// ```
#[macro_export]
macro_rules! message_span {
    ($message_type:expr) => {
        $crate::logging::debug_span!("message", type = $message_type)
    };
    ($message_type:expr, $($fields:tt)*) => {
        $crate::logging::debug_span!("message", type = $message_type, $($fields)*)
    };
}

/// Create a new span for system operations
///
/// Use this macro to trace system-level operations.
///
/// # Examples
///
/// ```rust
/// use parrot::system_span;
///
/// // Create a basic system span
/// let span = system_span!("system_startup");
/// let _guard = span.enter();
///
/// // With additional fields
/// let span = system_span!("actor_spawning", actor_count = 5);
/// ```
#[macro_export]
macro_rules! system_span {
    ($operation:expr) => {
        $crate::logging::info_span!("system", operation = $operation)
    };
    ($operation:expr, $($fields:tt)*) => {
        $crate::logging::info_span!("system", operation = $operation, $($fields)*)
    };
}

/// Log actor lifecycle events - use for important actor state changes
///
/// # Examples
///
/// ```rust
/// use parrot::log_lifecycle;
///
/// // Log a basic lifecycle event
/// log_lifecycle!("user_actor", "user-123", "started");
///
/// // With additional fields
/// log_lifecycle!("order_actor", "order-456", "stopped", reason = "completed");
/// ```
#[macro_export]
macro_rules! log_lifecycle {
    ($actor_type:expr, $actor_id:expr, $event:expr) => {
        $crate::logging::info!(actor_type = $actor_type, actor_id = $actor_id, event = $event);
    };
    ($actor_type:expr, $actor_id:expr, $event:expr, $($fields:tt)*) => {
        $crate::logging::info!(actor_type = $actor_type, actor_id = $actor_id, event = $event, $($fields)*);
    };
}

/// Log message processing events - use for detailed message handling
///
/// # Examples
///
/// ```rust
/// use parrot::log_message;
///
/// // Log a basic message event
/// log_message!("CreateUser", "received");
///
/// // With additional fields
/// log_message!("UpdateOrder", "processed", duration_ms = 45.5);
/// ```
#[macro_export]
macro_rules! log_message {
    ($message_type:expr, $status:expr) => {
        $crate::logging::debug!(message_type = $message_type, status = $status);
    };
    ($message_type:expr, $status:expr, $($fields:tt)*) => {
        $crate::logging::debug!(message_type = $message_type, status = $status, $($fields)*);
    };
}

/// Log system events - use for important system state changes
///
/// # Examples
///
/// ```rust
/// use parrot::log_system;
///
/// // Log a basic system event
/// log_system!("system_startup", "completed");
///
/// // With additional fields
/// log_system!("actor_spawning", "completed", count = 5, duration_ms = 230);
/// ```
#[macro_export]
macro_rules! log_system {
    ($operation:expr, $status:expr) => {
        $crate::logging::info!(operation = $operation, status = $status);
    };
    ($operation:expr, $status:expr, $($fields:tt)*) => {
        $crate::logging::info!(operation = $operation, status = $status, $($fields)*);
    };
}

/// Log error events - use for all error conditions
///
/// # Examples
///
/// ```rust
/// use parrot::log_error;
///
/// // Log a basic error
/// let error = std::io::Error::new(std::io::ErrorKind::NotFound, "File not found");
/// log_error!(error);
///
/// // With additional context fields
/// log_error!(error, component = "storage", operation = "read_config");
/// ```
#[macro_export]
macro_rules! log_error {
    ($error:expr) => {
        $crate::logging::error!(error = %$error);
    };
    ($error:expr, $($fields:tt)*) => {
        $crate::logging::error!(error = %$error, $($fields)*);
    };
}

/// Log scheduling events
///
/// # Examples
///
/// ```rust
/// use parrot::log_scheduler;
///
/// // Log a basic scheduler event
/// log_scheduler!("dedicated_pool", "actor_scheduled");
///
/// // With additional fields
/// log_scheduler!("shared_pool", "work_queued", queue_size = 10, priority = "high");
/// ```
#[macro_export]
macro_rules! log_scheduler {
    ($scheduler:expr, $event:expr) => {
        $crate::logging::debug!(scheduler = $scheduler, event = $event);
    };
    ($scheduler:expr, $event:expr, $($fields:tt)*) => {
        $crate::logging::debug!(scheduler = $scheduler, event = $event, $($fields)*);
    };
}

/// Get the current tracing dispatcher
///
/// This is useful when spawning threads that need access to the current tracing configuration.
///
/// # Returns
/// The current global dispatcher
///
/// # Examples
///
/// ```rust
/// use parrot::logging;
/// use std::thread;
///
/// // Initialize logging
/// logging::init_default(false);
///
/// // Capture the current dispatcher
/// let dispatcher = logging::current_subscriber();
///
/// // Use it in a new thread
/// thread::spawn(move || {
///     let _guard = tracing::dispatcher::set_default(&dispatcher);
///     // Logs in this thread will use the same configuration
///     tracing::info!("Worker thread started");
/// }).join().unwrap();
/// ```
#[inline]
pub fn current_subscriber() -> tracing::Dispatch {
    tracing::dispatcher::get_default(|d| d.clone())
}

// Re-export the most commonly used tracing macros for convenience
pub use tracing::{debug, debug_span, error, info, info_span, trace, warn};

fn development_config() -> LogConfig {
    LogConfig {
        level: Level::DEBUG,
        json_format: false,
        show_file_line: true,
        show_thread_info: true,
        show_time: true,
        target_filters: Some("parrot=debug,parrot::thread=trace".to_string()),
    }
}

fn production_config() -> LogConfig {
    LogConfig {
        level: Level::INFO,
        json_format: true,
        show_file_line: false,
        show_thread_info: true,
        show_time: true,
        target_filters: None,
    }
}

fn test_config() -> LogConfig {
    LogConfig {
        level: Level::WARN,
        json_format: false,
        show_file_line: true,
        show_thread_info: false,
        show_time: false,
        target_filters: None,
    }
}
