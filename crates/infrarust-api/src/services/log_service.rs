//! Live proxy log subscriptions.
//!
//! A plugin holding the [`LogRead`](crate::permissions::Capability::LogRead)
//! capability can receive the proxy's log events as they happen. The stream is
//! deliberately narrow:
//!
//! - Only events that pass the operator's global log filter (`RUST_LOG` or
//!   `--log-level`) are seen. Lowering the filter is the only way to get more.
//! - Each record carries a level, a target, a timestamp, and one formatted
//!   message line. Structured fields are flattened into that line, and span
//!   history is never included.
//! - Subscriptions are live only. There is no replay of earlier records.
//! - Delivery never blocks the proxy. Each subscription has a bounded queue,
//!   and records that do not fit are dropped and counted in
//!   [`LogSubscription::dropped`].
//! - Messages are bounded to [`MAX_LOG_MESSAGE_BYTES`], single-line, and
//!   scrubbed of IPv4 addresses and common secret assignments on a best-effort
//!   basis. IPv6 addresses and secrets without a recognizable key are not
//!   scrubbed, so a log stream still needs operator review before it leaves
//!   the host.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use tokio::sync::mpsc;

/// The largest message a [`LogRecord`] carries, in bytes, including any
/// truncation marker. Longer messages are cut at a character boundary.
pub const MAX_LOG_MESSAGE_BYTES: usize = 1024;

/// The sealed trait for subscribing to proxy log events.
///
/// Obtain it through [`PluginContext::log_service`](crate::plugin::PluginContext::log_service),
/// which returns `None` unless the plugin holds
/// [`Capability::LogRead`](crate::permissions::Capability::LogRead).
pub trait LogService: Send + Sync + private::Sealed {
    /// Opens a live subscription that receives records matching `filter`.
    ///
    /// # Errors
    ///
    /// Returns [`LogSubscribeError::TooManySubscribers`] when the proxy already
    /// has its maximum number of live subscriptions, and
    /// [`LogSubscribeError::InvalidTarget`] when a target filter is empty.
    fn subscribe(&self, filter: LogFilter) -> Result<LogSubscription, LogSubscribeError>;
}

pub mod private {
    /// Sealed — only the proxy implements [`LogService`](super::LogService).
    pub trait Sealed {}
}

/// Event severity, ordered from most to least severe.
///
/// Because `Error` sorts first, `level <= min` means "at least as severe as `min`".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Parses a level name, case-insensitively. Returns `None` for anything else.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    /// The lowercase name of the level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which records a subscription accepts by target.
///
/// Targets are Rust module paths such as `infrarust_core::session`. A
/// [`LogTarget::Prefix`] matches on whole path segments: `infrarust_core`
/// matches `infrarust_core::session` but not `infrarust_core_extra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogTarget {
    Any,
    Exact(String),
    Prefix(String),
}

impl LogTarget {
    /// Whether `target` is accepted by this filter.
    #[must_use]
    pub fn matches(&self, target: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(exact) => target == exact,
            Self::Prefix(prefix) => {
                target == prefix
                    || target
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|rest| rest.starts_with("::"))
            }
        }
    }
}

/// The records a subscription accepts: a minimum severity and a target filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFilter {
    min_level: LogLevel,
    target: LogTarget,
}

impl LogFilter {
    /// Accepts every target at `min_level` or more severe.
    #[must_use]
    pub const fn new(min_level: LogLevel) -> Self {
        Self {
            min_level,
            target: LogTarget::Any,
        }
    }

    /// Accepts only records whose target equals `target` exactly.
    #[must_use]
    pub fn exact(mut self, target: impl Into<String>) -> Self {
        self.target = LogTarget::Exact(target.into());
        self
    }

    /// Accepts records whose target is `prefix` or starts with `prefix::`.
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.target = LogTarget::Prefix(prefix.into());
        self
    }

    /// The most lenient severity this filter accepts.
    #[must_use]
    pub const fn min_level(&self) -> LogLevel {
        self.min_level
    }

    /// The target filter.
    #[must_use]
    pub const fn target(&self) -> &LogTarget {
        &self.target
    }

    /// Whether a record with this level and target passes the filter.
    #[must_use]
    pub fn matches(&self, level: LogLevel, target: &str) -> bool {
        level <= self.min_level && self.target.matches(target)
    }

    /// Whether the target filter is well formed. Empty and whitespace targets are refused.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        match &self.target {
            LogTarget::Any => true,
            LogTarget::Exact(value) | LogTarget::Prefix(value) => {
                !value.trim().is_empty()
                    && value.len() <= 256
                    && !value.contains(char::is_whitespace)
            }
        }
    }
}

/// One log event delivered to a subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// The event's severity.
    pub level: LogLevel,
    /// The event's Rust module path, such as `infrarust_core::session`.
    pub target: String,
    /// When the proxy observed the event.
    pub timestamp: SystemTime,
    /// The formatted, single-line, scrubbed message, at most [`MAX_LOG_MESSAGE_BYTES`] bytes.
    pub message: String,
}

impl LogRecord {
    /// Builds a record. Plugins only receive records; the proxy builds them.
    #[must_use]
    pub fn new(
        level: LogLevel,
        target: impl Into<String>,
        timestamp: SystemTime,
        message: impl Into<String>,
    ) -> Self {
        Self {
            level,
            target: target.into(),
            timestamp,
            message: message.into(),
        }
    }
}

/// A live subscription to proxy log records.
///
/// Dropping the subscription closes it. The proxy stops delivering to it on
/// its next event.
#[derive(Debug)]
pub struct LogSubscription {
    receiver: mpsc::Receiver<LogRecord>,
    dropped: Arc<AtomicU64>,
}

impl LogSubscription {
    /// Pairs a receiver with the counter the proxy increments on drops.
    ///
    /// The proxy builds subscriptions; plugins receive them from [`LogService::subscribe`].
    #[must_use]
    pub const fn new(receiver: mpsc::Receiver<LogRecord>, dropped: Arc<AtomicU64>) -> Self {
        Self { receiver, dropped }
    }

    /// Waits for the next record. Returns `None` once the proxy has closed the stream.
    pub async fn recv(&mut self) -> Option<LogRecord> {
        self.receiver.recv().await
    }

    /// Returns the next record if one is already queued.
    pub fn try_recv(&mut self) -> Result<LogRecord, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    /// The number of records dropped for this subscription because its queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Why a log subscription was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LogSubscribeError {
    /// The proxy already has its maximum number of live subscriptions.
    #[error("too many log subscriptions (limit {max})")]
    TooManySubscribers {
        /// The limit the proxy enforces.
        max: usize,
    },

    /// The target filter is empty, contains whitespace, or is too long.
    #[error("invalid log target filter")]
    InvalidTarget,
}
