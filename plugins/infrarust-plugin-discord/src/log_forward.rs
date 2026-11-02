//! Opt-in forwarding of proxy log lines to one Discord channel.
//!
//! Off by default. The plugin reads the live proxy log stream only when the
//! operator grants `log-read` in its native permissions and sets
//! `[log_forward] enabled = true`. Each subscription takes one target from the
//! allowlist and one minimum severity. Records from this plugin and from its
//! Discord HTTP and gateway dependencies are never forwarded, so a failing
//! send cannot feed back into the forwarder. Sends are batched, rate limited,
//! and sent with every mention suppressed.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use infrarust_api::services::log_service::{
    LogFilter, LogLevel, LogRecord, LogService, LogSubscription,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use twilight_http::Client;
use twilight_model::channel::message::AllowedMentions;
use twilight_model::id::Id;
use twilight_model::id::marker::ChannelMarker;

use crate::config::LogForwardConfig;
use crate::limits::SlidingWindow;

/// Most allowlisted targets. Each one holds a proxy log subscription.
const MAX_TARGETS: usize = 8;
/// Records queued between the subscriptions and the sender.
const QUEUE_CAPACITY: usize = 256;
/// How long the sender gathers lines before it posts them as one message.
const BATCH_WINDOW: Duration = Duration::from_secs(3);
const MAX_BATCH_LINES: usize = 25;
/// Discord allows 2000 characters. This leaves room for the code fence and note.
const MAX_MESSAGE_CHARS: usize = 1800;
const RATE_WINDOW: Duration = Duration::from_secs(60);
const RATE_KEY: &str = "log-forward";

/// Crate roots whose records are never forwarded: this plugin and its Discord
/// HTTP and gateway stack. Matched on the crate root, so `twilight` also covers
/// `twilight_http` and `twilight_gateway`.
const DENIED_ROOTS: &[&str] = &[
    "infrarust_plugin_discord",
    "twilight",
    "hyper",
    "h2",
    "rustls",
    "reqwest",
    "tower",
];

/// Starts forwarding. Returns the task handles, which end when `cancel` fires.
///
/// # Errors
///
/// Returns a message when the config is invalid or the proxy refuses a subscription.
pub fn start(
    config: &LogForwardConfig,
    http: Arc<Client>,
    logs: Arc<dyn LogService>,
    cancel: CancellationToken,
) -> Result<Vec<JoinHandle<()>>, String> {
    let level = LogLevel::parse(&config.min_level).ok_or_else(|| {
        format!(
            "log_forward.min_level `{}` is not error, warn, info, debug or trace",
            config.min_level
        )
    })?;
    if config.channel_id == 0 {
        return Err("log_forward.channel_id must be set".into());
    }
    if config.max_messages_per_minute == 0 {
        return Err("log_forward.max_messages_per_minute must be at least 1".into());
    }
    if config.targets.is_empty() {
        return Err("log_forward.targets lists no module paths, so nothing is forwarded".into());
    }
    if config.targets.len() > MAX_TARGETS {
        return Err(format!(
            "log_forward.targets allows at most {MAX_TARGETS} entries"
        ));
    }

    let mut subscriptions = Vec::with_capacity(config.targets.len());
    for target in &config.targets {
        let target = target.trim();
        let filter = LogFilter::new(level).prefix(target);
        if !filter.is_valid() || is_denied(target) {
            return Err(format!("log_forward target `{target}` is not allowed"));
        }
        subscriptions.push(
            logs.subscribe(filter)
                .map_err(|err| format!("the proxy refused a log subscription: {err}"))?,
        );
    }

    let channel = Id::<ChannelMarker>::new(config.channel_id);
    let shed = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    let mut handles = Vec::with_capacity(subscriptions.len() + 1);
    for subscription in subscriptions {
        handles.push(tokio::spawn(pump(
            subscription,
            tx.clone(),
            Arc::clone(&shed),
            cancel.clone(),
        )));
    }
    drop(tx);
    handles.push(tokio::spawn(send_batches(
        rx,
        shed,
        http,
        channel,
        config.max_messages_per_minute,
        cancel,
    )));
    Ok(handles)
}

fn is_denied(target: &str) -> bool {
    let root = target.split("::").next().unwrap_or(target);
    DENIED_ROOTS.iter().any(|denied| root.starts_with(denied))
}

/// Moves records from one subscription into the shared queue.
///
/// A full queue drops the record and counts it in `shed`, so a slow Discord
/// never blocks the proxy's log path.
async fn pump(
    mut subscription: LogSubscription,
    tx: mpsc::Sender<LogRecord>,
    shed: Arc<AtomicU64>,
    cancel: CancellationToken,
) {
    let mut lagged = 0;
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            record = subscription.recv() => {
                let Some(record) = record else { break };
                // Lag drops are counted by the subscription itself.
                let total = subscription.dropped();
                shed.fetch_add(total.saturating_sub(lagged), Ordering::Relaxed);
                lagged = total;
                if is_denied(&record.target) {
                    continue;
                }
                if tx.try_send(record).is_err() {
                    shed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

async fn send_batches(
    mut rx: mpsc::Receiver<LogRecord>,
    shed: Arc<AtomicU64>,
    http: Arc<Client>,
    channel: Id<ChannelMarker>,
    max_per_minute: usize,
    cancel: CancellationToken,
) {
    let limiter = SlidingWindow::new(max_per_minute, RATE_WINDOW);
    let mut omitted: u64 = 0;
    loop {
        let first = tokio::select! {
            () = cancel.cancelled() => return,
            record = rx.recv() => match record {
                Some(record) => record,
                None => return,
            },
        };
        let mut batch = vec![first];
        let window = tokio::time::sleep(BATCH_WINDOW);
        tokio::pin!(window);
        while batch.len() < MAX_BATCH_LINES {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = &mut window => break,
                record = rx.recv() => match record {
                    Some(record) => batch.push(record),
                    None => break,
                },
            }
        }

        omitted += shed.swap(0, Ordering::Relaxed);
        if !limiter.allows(RATE_KEY) {
            omitted += batch.len() as u64;
            continue;
        }
        limiter.record(RATE_KEY);

        let body = render_batch(&batch, omitted);
        omitted = 0;
        if let Err(error) = http
            .create_message(channel)
            .content(&body)
            .allowed_mentions(Some(&AllowedMentions::default()))
            .await
        {
            tracing::warn!(error = %error, "Discord log forward message failed");
        }
    }
}

/// Renders a batch as one fenced block, dropping lines that would overflow the limit.
fn render_batch(batch: &[LogRecord], omitted_before: u64) -> String {
    let mut lines = String::new();
    let mut used = 0;
    let mut included: u64 = 0;
    for record in batch {
        let line = format!(
            "[{}] {}: {}",
            record.level.as_str().to_uppercase(),
            record.target,
            record.message
        );
        let line = sanitize(&line);
        let cost = line.chars().count() + 1;
        if used + cost > MAX_MESSAGE_CHARS - 120 {
            break;
        }
        lines.push_str(&line);
        lines.push('\n');
        used += cost;
        included += 1;
    }

    let skipped = omitted_before + (batch.len() as u64 - included);
    let mut body = String::new();
    if skipped > 0 {
        body.push_str(&format!(
            "{skipped} log line(s) were not forwarded (rate limit or size).\n"
        ));
    }
    body.push_str("```text\n");
    body.push_str(&lines);
    body.push_str("```");
    body
}

/// Makes a line safe to post: a backtick cannot close the code fence, and an
/// `@` cannot form a mention even if the allowed-mentions setting is ignored.
fn sanitize(value: &str) -> String {
    value.replace('`', "'").replace('@', "@\u{200b}")
}
