//! Live proxy log subscriptions for plugins holding `LogRead`.
//!
//! [`LogStream`] is the proxy-side half of [`LogService`]. Its
//! [`LogStreamLayer`] sits on the process tracing subscriber, so it sees every
//! event that passes the operator's global log filter. Delivery is
//! non-blocking: a full subscriber queue drops the record and counts the drop.

mod layer;
mod scrub;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use infrarust_api::services::log_service::private::Sealed;
use infrarust_api::services::log_service::{
    LogFilter, LogLevel, LogRecord, LogService, LogSubscribeError, LogSubscription,
};
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::util::sync::lock;

pub use layer::LogStreamLayer;

/// The most live subscriptions the proxy keeps at once.
pub const MAX_SUBSCRIBERS: usize = 16;

/// Queued records per subscription before the proxy starts dropping.
pub const SUBSCRIBER_QUEUE_CAPACITY: usize = 256;

struct Subscriber {
    filter: LogFilter,
    sender: mpsc::Sender<LogRecord>,
    dropped: Arc<AtomicU64>,
}

/// Fan-out point between the tracing layer and plugin subscriptions.
#[derive(Default)]
pub struct LogStream {
    subscribers: Mutex<Vec<Subscriber>>,
    /// Mirrors `subscribers.len()`, so the hot path skips formatting when nobody listens.
    live: AtomicUsize,
}

impl LogStream {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The tracing layer that feeds this stream. Install it on the process subscriber.
    #[must_use]
    pub fn layer(self: &Arc<Self>) -> LogStreamLayer {
        LogStreamLayer::new(Arc::clone(self))
    }

    /// Hands one event to every matching subscriber.
    ///
    /// `render` runs only when at least one subscriber accepts the event, so
    /// formatting cost is paid only when someone is listening.
    fn dispatch(&self, level: LogLevel, target: &str, render: impl FnOnce() -> String) {
        if self.live.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut subscribers = lock(&self.subscribers);
        subscribers.retain(|subscriber| !subscriber.sender.is_closed());
        self.live.store(subscribers.len(), Ordering::Release);

        let mut matching = subscribers
            .iter()
            .filter(|subscriber| subscriber.filter.matches(level, target))
            .peekable();
        if matching.peek().is_none() {
            return;
        }

        let message = render();
        let timestamp = SystemTime::now();
        for subscriber in matching {
            let record = LogRecord::new(level, target, timestamp, message.clone());
            if let Err(TrySendError::Full(_)) = subscriber.sender.try_send(record) {
                subscriber.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Sealed for LogStream {}

impl LogService for LogStream {
    fn subscribe(&self, filter: LogFilter) -> Result<LogSubscription, LogSubscribeError> {
        if !filter.is_valid() {
            return Err(LogSubscribeError::InvalidTarget);
        }
        let mut subscribers = lock(&self.subscribers);
        subscribers.retain(|subscriber| !subscriber.sender.is_closed());
        if subscribers.len() >= MAX_SUBSCRIBERS {
            return Err(LogSubscribeError::TooManySubscribers {
                max: MAX_SUBSCRIBERS,
            });
        }

        let (sender, receiver) = mpsc::channel(SUBSCRIBER_QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        subscribers.push(Subscriber {
            filter,
            sender,
            dropped: Arc::clone(&dropped),
        });
        self.live.store(subscribers.len(), Ordering::Release);
        Ok(LogSubscription::new(receiver, dropped))
    }
}
