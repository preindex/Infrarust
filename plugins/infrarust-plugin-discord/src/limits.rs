//! Per-key sliding-window counter, used to throttle failed `/link` attempts.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Entries beyond this count trigger a sweep of keys with no recent events.
const SWEEP_THRESHOLD: usize = 4096;

pub struct SlidingWindow {
    max: usize,
    window: Duration,
    events: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl SlidingWindow {
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            events: Mutex::new(HashMap::new()),
        }
    }

    /// `true` while `key` has fewer than `max` events in the window.
    pub fn allows(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(queue) = events.get_mut(key) else {
            return true;
        };
        prune(queue, now, self.window);
        queue.len() < self.max
    }

    pub fn record(&self, key: &str) {
        let now = Instant::now();
        let mut events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        if events.len() > SWEEP_THRESHOLD {
            let window = self.window;
            events.retain(|_, queue| {
                prune(queue, now, window);
                !queue.is_empty()
            });
        }
        let queue = events.entry(key.to_owned()).or_default();
        prune(queue, now, self.window);
        queue.push_back(now);
    }
}

fn prune(queue: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while queue
        .front()
        .is_some_and(|front| now.saturating_duration_since(*front) >= window)
    {
        queue.pop_front();
    }
}
