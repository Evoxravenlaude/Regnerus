//! What `/health` reports: is the stream up, is anything arriving, and how
//! far behind the chain the bot sees a trade.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

const DELAY_SAMPLES: usize = 200;

#[derive(Default)]
pub struct Health {
    pub started_at: AtomicI64,
    pub connected: AtomicBool,
    pub connects: AtomicU64,
    pub last_connect_at: AtomicI64,
    pub last_event_at: AtomicI64,
    pub subscriptions: AtomicUsize,
    pub events: AtomicU64,
    pub tx_fetched: AtomicU64,
    pub tx_missing: AtomicU64,
    pub trades: AtomicU64,
    pub transfers: AtomicU64,
    pub cards: AtomicU64,
    pub errors: AtomicU64,
    /// Seconds from a trade's block time to this process seeing it.
    delays: Mutex<VecDeque<i64>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HealthSnapshot {
    pub started_at: i64,
    pub connected: bool,
    pub connects: u64,
    pub last_connect_at: i64,
    pub last_event_at: i64,
    pub subscriptions: usize,
    pub events: u64,
    pub tx_fetched: u64,
    pub tx_missing: u64,
    pub trades: u64,
    pub transfers: u64,
    pub cards: u64,
    pub errors: u64,
    pub delay_samples: usize,
    pub delay_median_s: Option<i64>,
    pub delay_p90_s: Option<i64>,
}

impl Health {
    pub fn new(now: i64) -> Self {
        let health = Health::default();
        health.started_at.store(now, Ordering::Relaxed);
        health
    }

    pub fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_delay(&self, seconds: i64) {
        let mut delays = self.delays.lock();
        if delays.len() == DELAY_SAMPLES {
            delays.pop_front();
        }
        delays.push_back(seconds.max(0));
    }

    pub fn snapshot(&self) -> HealthSnapshot {
        let mut sorted: Vec<i64> = self.delays.lock().iter().copied().collect();
        sorted.sort_unstable();
        let pick = |q: f64| -> Option<i64> { if sorted.is_empty() { None } else { Some(sorted[(((sorted.len() - 1) as f64) * q).round() as usize]) } };
        HealthSnapshot {
            started_at: self.started_at.load(Ordering::Relaxed),
            connected: self.connected.load(Ordering::Relaxed),
            connects: self.connects.load(Ordering::Relaxed),
            last_connect_at: self.last_connect_at.load(Ordering::Relaxed),
            last_event_at: self.last_event_at.load(Ordering::Relaxed),
            subscriptions: self.subscriptions.load(Ordering::Relaxed),
            events: self.events.load(Ordering::Relaxed),
            tx_fetched: self.tx_fetched.load(Ordering::Relaxed),
            tx_missing: self.tx_missing.load(Ordering::Relaxed),
            trades: self.trades.load(Ordering::Relaxed),
            transfers: self.transfers.load(Ordering::Relaxed),
            cards: self.cards.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            delay_samples: sorted.len(),
            delay_median_s: pick(0.5),
            delay_p90_s: pick(0.9),
        }
    }
}
