//! The one object everything hangs off: settings, store, RPC, Telegram,
//! labels, budget and health. `commands.rs` adds what users can ask of it;
//! `pipeline.rs` adds what happens when a watched wallet moves.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::bot::telegram::TelegramCall;
use crate::chain::solana::labels::LabelBook;
use crate::chain::solana::rpc::ChainRpc;
use crate::chain::solana::stream::AddressSource;
use crate::config::Settings;
use crate::obs::budget::Budget;
use crate::obs::health::Health;
use crate::store::Store;
use crate::util::Clock;

const RECENT_CAP: usize = 10_000;

/// A bounded set of keys seen lately, oldest forgotten first.
#[derive(Default)]
pub(crate) struct Recent {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl Recent {
    /// True the first time a key is offered.
    pub(crate) fn insert(&mut self, key: String) -> bool {
        if self.set.contains(&key) {
            return false;
        }
        if self.order.len() >= RECENT_CAP
            && let Some(oldest) = self.order.pop_front()
        {
            self.set.remove(&oldest);
        }
        self.set.insert(key.clone());
        self.order.push_back(key);
        true
    }

    pub(crate) fn contains(&self, key: &str) -> bool {
        self.set.contains(key)
    }

    pub(crate) fn remove(&mut self, key: &str) {
        if self.set.remove(key) {
            self.order.retain(|k| k != key);
        }
    }
}

pub struct App {
    pub settings: Settings,
    pub store: Arc<dyn Store>,
    pub rpc: Arc<dyn ChainRpc>,
    pub tg: Arc<dyn TelegramCall>,
    pub labels: Arc<LabelBook>,
    pub budget: Arc<Budget>,
    pub health: Arc<Health>,
    pub clock: Clock,
    /// Notified whenever the set of watched wallets may have changed.
    pub refresh: Arc<Notify>,
    /// For `/health`: the RPC's host, never its full URL.
    pub rpc_host: String,
    /// Waits before each `getTransaction` attempt: a transaction can lag its
    /// log notification by a moment.
    pub tx_retry_ms: Vec<u64>,
    /// `signature:wallet` pairs already handled, so the backfill does not
    /// fetch them again.
    pub(crate) processed: Mutex<Recent>,
    /// One lock per (entity, mint, side): two trades of one signal are
    /// folded one after the other, never both as "the first".
    pub(crate) signal_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Where swaps are quoted and sent. `None`: no trading, watch only.
    venue: parking_lot::RwLock<Option<Arc<dyn crate::trade::venue::SwapVenue>>>,
    keys: parking_lot::RwLock<Option<Arc<dyn crate::trade::keys::KeyProvider>>>,
    reviewer: parking_lot::RwLock<Option<Arc<dyn crate::review::Reviewer>>>,
    /// One trade at a time per account: caps are checked and the position
    /// is written under this lock.
    pub(crate) account_locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    /// The exit a position hit on the previous monitor pass: a price exit
    /// needs two passes in a row.
    pub(crate) exit_hits: Mutex<HashMap<i64, &'static str>>,
    /// This process, for the executor lease.
    pub instance: String,
    /// Which entities move as one bloc, and when that was last worked out.
    pub(crate) blocs_cache: Mutex<Option<(i64, std::collections::BTreeMap<i64, i64>)>>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(settings: Settings, store: Arc<dyn Store>, rpc: Arc<dyn ChainRpc>, tg: Arc<dyn TelegramCall>, labels: Arc<LabelBook>, budget: Arc<Budget>, clock: Clock, rpc_host: String) -> Arc<App> {
        let health = Arc::new(Health::new(clock()));
        Arc::new(App {
            settings,
            store,
            rpc,
            tg,
            labels,
            budget,
            health,
            clock,
            refresh: Arc::new(Notify::new()),
            rpc_host,
            tx_retry_ms: vec![0, 700, 1_400],
            processed: Mutex::new(Recent::default()),
            signal_locks: Mutex::new(HashMap::new()),
            venue: parking_lot::RwLock::new(None),
            keys: parking_lot::RwLock::new(None),
            reviewer: parking_lot::RwLock::new(None),
            account_locks: Mutex::new(HashMap::new()),
            exit_hits: Mutex::new(HashMap::new()),
            instance: format!("{}-{}", std::process::id(), now_nanos()),
            blocs_cache: Mutex::new(None),
        })
    }

    pub fn venue(&self) -> Option<Arc<dyn crate::trade::venue::SwapVenue>> {
        self.venue.read().clone()
    }

    pub fn set_venue(&self, venue: Arc<dyn crate::trade::venue::SwapVenue>) {
        *self.venue.write() = Some(venue);
    }

    pub fn keys(&self) -> Option<Arc<dyn crate::trade::keys::KeyProvider>> {
        self.keys.read().clone()
    }

    pub fn set_keys(&self, keys: Arc<dyn crate::trade::keys::KeyProvider>) {
        *self.keys.write() = Some(keys);
    }

    pub fn reviewer(&self) -> Option<Arc<dyn crate::review::Reviewer>> {
        self.reviewer.read().clone()
    }

    pub fn set_reviewer(&self, reviewer: Arc<dyn crate::review::Reviewer>) {
        *self.reviewer.write() = Some(reviewer);
    }

    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    /// Admin-added labels from the database, laid over the seed.
    pub async fn load_labels(&self) -> usize {
        match self.store.labels().await {
            Ok(labels) => {
                let count = labels.len();
                for label in labels {
                    self.labels.set(label);
                }
                count
            }
            Err(error) => {
                log::warn!("could not load labels: {error}");
                0
            }
        }
    }

    /// Today's ledger, so a restart does not forget what was already spent.
    pub async fn restore_budget(&self) {
        let day = crate::util::day_of(self.now());
        if let Ok(rows) = self.store.credits(&day).await {
            for (lane, credits) in rows {
                if let Some(lane) = crate::obs::budget::Lane::ALL.iter().find(|l| l.as_str() == lane) {
                    self.budget.restore(&day, *lane, credits);
                }
            }
        }
    }

    /// Writes credits counted since the last flush to the ledger.
    pub async fn flush_ledger(&self) {
        for (day, lane, credits) in self.budget.take_unflushed() {
            if let Err(error) = self.store.add_credits(&day, lane.as_str(), credits).await {
                log::warn!("could not write the credit ledger: {error}");
            }
        }
    }
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

#[async_trait]
impl AddressSource for App {
    /// Watched wallets, and the deposit addresses that act as tripwires.
    async fn addresses(&self) -> Vec<String> {
        let mut out = match self.store.watched_wallets().await {
            Ok(wallets) => wallets.into_iter().map(|w| w.address).collect(),
            Err(error) => {
                log::error!("could not read the watched wallets: {error}");
                Vec::new()
            }
        };
        match self.store.watched_deposits().await {
            Ok(deposits) => out.extend(deposits.into_iter().map(|d| d.address)),
            Err(error) => log::error!("could not read the deposit addresses: {error}"),
        }
        out.sort();
        out.dedup();
        out
    }
}
