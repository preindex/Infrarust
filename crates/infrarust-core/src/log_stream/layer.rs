//! The tracing layer that captures events for [`LogStream`].

use std::fmt::{self, Write as _};
use std::sync::Arc;

use infrarust_api::services::log_service::{LogLevel, MAX_LOG_MESSAGE_BYTES};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use super::LogStream;
use super::scrub::scrub;

/// Captures events into a [`LogStream`]. It records only level, target,
/// timestamp, and one formatted message. Span history is never read.
pub struct LogStreamLayer {
    stream: Arc<LogStream>,
}

impl LogStreamLayer {
    pub(super) const fn new(stream: Arc<LogStream>) -> Self {
        Self { stream }
    }
}

impl<S> Layer<S> for LogStreamLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = level_of(*metadata.level());
        self.stream
            .dispatch(level, metadata.target(), || render(event));
    }
}

fn level_of(level: tracing::Level) -> LogLevel {
    if level == tracing::Level::ERROR {
        LogLevel::Error
    } else if level == tracing::Level::WARN {
        LogLevel::Warn
    } else if level == tracing::Level::INFO {
        LogLevel::Info
    } else if level == tracing::Level::DEBUG {
        LogLevel::Debug
    } else {
        LogLevel::Trace
    }
}

/// Flattens an event into one line: the message, then `name=value` pairs.
fn render(event: &Event<'_>) -> String {
    let mut collector = Collector::default();
    event.record(&mut collector);
    let line = format!("{}{}", collector.message, collector.fields);
    // Output is cut to MAX_LOG_MESSAGE_BYTES anyway, so scrub only a bounded prefix.
    let bounded = prefix_bytes(&line, MAX_SCRUB_INPUT_BYTES);
    let single_line: String = bounded
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    truncate_bytes(&scrub(&single_line), MAX_LOG_MESSAGE_BYTES)
}

/// Scrubbing scans for `=` and `:` with a forward walk, so its input is capped.
const MAX_SCRUB_INPUT_BYTES: usize = 4096;

fn prefix_bytes(value: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[derive(Default)]
struct Collector {
    message: String,
    fields: String,
}

impl Visit for Collector {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

const ELLIPSIS: char = '…';

/// Cuts `value` to at most `max_bytes`, on a character boundary, marking the cut with `…`.
fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.saturating_sub(ELLIPSIS.len_utf8());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = value[..end].to_owned();
    cut.push(ELLIPSIS);
    cut
}
