//! The wallet stream: one websocket, one `logsSubscribe` per watched wallet.
//! Ported from Yeks's Solana watcher.
//!
//! `mentions` takes exactly one address per subscription, so each wallet
//! gets its own. The bookkeeping (`SubBook`) is pure and tested; the socket
//! loop around it only moves frames. Every connect rebuilds the whole set
//! of subscriptions, and tells the pipeline to backfill what was missed
//! while the socket was down.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use crate::obs::health::Health;
use crate::util::{Clock, host_of, is_solana_address};

const SEEN_CAP: usize = 5_000;
const PING_EVERY: Duration = Duration::from_secs(30);
const STALE_AFTER: Duration = Duration::from_secs(90);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const BASE_BACKOFF_MS: u64 = 1_000;
const MAX_BACKOFF_MS: u64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamMsg {
    /// A watched wallet was in a successful transaction.
    Tx { wallet: String, signature: String },
    /// The socket (re)connected and every subscription was sent again.
    Connected { wallets: usize },
}

/// Where the stream reads the current list of wallets to watch.
#[async_trait]
pub trait AddressSource: Send + Sync {
    async fn addresses(&self) -> Vec<String>;
}

enum Pending {
    Subscribe(String),
    Unsubscribe,
}

/// Subscription bookkeeping for one socket.
pub struct SubBook {
    commitment: String,
    next_id: u64,
    /// address -> subscription id (`None` while the request is in flight).
    subs: HashMap<String, Option<u64>>,
    by_sub: HashMap<u64, String>,
    pending: HashMap<u64, Pending>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
}

impl SubBook {
    pub fn new(commitment: &str) -> Self {
        SubBook { commitment: commitment.to_string(), next_id: 1, subs: HashMap::new(), by_sub: HashMap::new(), pending: HashMap::new(), seen: HashSet::new(), seen_order: VecDeque::new() }
    }

    /// A fresh socket has no subscriptions. What was already seen is kept,
    /// so a reconnect does not replay a signature.
    pub fn reset(&mut self) {
        self.subs.clear();
        self.by_sub.clear();
        self.pending.clear();
    }

    fn frame(&mut self, method: &str, params: Value) -> (u64, String) {
        let id = self.next_id;
        self.next_id += 1;
        (id, json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string())
    }

    /// Frames that bring the socket's subscriptions in line with `want`.
    pub fn sync(&mut self, want: &[String]) -> Vec<String> {
        let mut frames = Vec::new();
        let wanted: HashSet<&str> = want.iter().map(String::as_str).filter(|a| is_solana_address(a)).collect();
        let gone: Vec<String> = self.subs.keys().filter(|a| !wanted.contains(a.as_str())).cloned().collect();
        for address in gone {
            if let Some(Some(sub)) = self.subs.remove(&address) {
                self.by_sub.remove(&sub);
                let (id, frame) = self.frame("logsUnsubscribe", json!([sub]));
                self.pending.insert(id, Pending::Unsubscribe);
                frames.push(frame);
            }
        }
        let mut fresh: Vec<&str> = wanted.into_iter().filter(|a| !self.subs.contains_key(*a)).collect();
        fresh.sort_unstable();
        for address in fresh {
            self.subs.insert(address.to_string(), None);
            let (id, frame) = self.frame("logsSubscribe", json!([{ "mentions": [address] }, { "commitment": self.commitment }]));
            self.pending.insert(id, Pending::Subscribe(address.to_string()));
            frames.push(frame);
        }
        frames
    }

    fn first_sight(&mut self, key: String) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        if self.seen_order.len() >= SEEN_CAP
            && let Some(oldest) = self.seen_order.pop_front()
        {
            self.seen.remove(&oldest);
        }
        self.seen.insert(key.clone());
        self.seen_order.push_back(key);
        true
    }

    /// Handles one text frame from the node. Returns a wallet's signature
    /// when the frame is a new, successful log notification, plus any frames
    /// to send back.
    pub fn on_text(&mut self, text: &str) -> (Option<(String, String)>, Vec<String>) {
        let Ok(msg) = serde_json::from_str::<Value>(text) else { return (None, Vec::new()) };

        if let Some(id) = msg.get("id").and_then(Value::as_u64)
            && let Some(pending) = self.pending.remove(&id)
        {
            let Pending::Subscribe(address) = pending else { return (None, Vec::new()) };
            let Some(sub) = msg.get("result").and_then(Value::as_u64) else {
                let reason = msg.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("no subscription id");
                log::warn!("logsSubscribe for {address} failed: {reason}");
                self.subs.remove(&address);
                return (None, Vec::new());
            };
            if !self.subs.contains_key(&address) {
                // Removed while the subscribe was in flight.
                let (unsub_id, frame) = self.frame("logsUnsubscribe", json!([sub]));
                self.pending.insert(unsub_id, Pending::Unsubscribe);
                return (None, vec![frame]);
            }
            self.subs.insert(address.clone(), Some(sub));
            self.by_sub.insert(sub, address);
            return (None, Vec::new());
        }

        if msg.get("method").and_then(Value::as_str) != Some("logsNotification") {
            return (None, Vec::new());
        }
        let params = msg.get("params");
        let sub = params.and_then(|p| p.get("subscription")).and_then(Value::as_u64);
        let value = params.and_then(|p| p.get("result")).and_then(|r| r.get("value"));
        let signature = value.and_then(|v| v.get("signature")).and_then(Value::as_str);
        let (Some(sub), Some(signature)) = (sub, signature) else { return (None, Vec::new()) };
        let Some(address) = self.by_sub.get(&sub).cloned() else { return (None, Vec::new()) };
        if value.and_then(|v| v.get("err")).is_some_and(|e| !e.is_null()) {
            return (None, Vec::new()); // a failed transaction moved nothing
        }
        if !self.first_sight(format!("{signature}:{address}")) {
            return (None, Vec::new());
        }
        (Some((address, signature.to_string())), Vec::new())
    }

    /// `(wallets wanted, subscriptions the node has confirmed)`.
    pub fn scope(&self) -> (usize, usize) {
        (self.subs.len(), self.by_sub.len())
    }
}

pub struct StreamConfig {
    pub ws_url: String,
    pub commitment: String,
}

fn backoff(attempt: u32) -> Duration {
    let base = BASE_BACKOFF_MS.saturating_mul(1u64 << attempt.saturating_sub(1).min(6)).min(MAX_BACKOFF_MS);
    // A little jitter so two instances do not reconnect in step.
    let jitter = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64 % (BASE_BACKOFF_MS / 2)).unwrap_or(0);
    Duration::from_millis(base + jitter)
}

/// Runs until `shutdown` turns true. `refresh` is notified whenever the set
/// of watched wallets may have changed.
pub async fn run_stream(
    config: StreamConfig,
    source: Arc<dyn AddressSource>,
    out: mpsc::Sender<StreamMsg>,
    health: Arc<Health>,
    clock: Clock,
    refresh: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) {
    let host = host_of(&config.ws_url);
    let mut book = SubBook::new(&config.commitment);
    let mut attempt: u32 = 0;

    loop {
        if *shutdown.borrow() {
            return;
        }
        let connecting = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(config.ws_url.as_str()));
        let socket = tokio::select! {
            result = connecting => result,
            _ = shutdown.changed() => return,
        };
        let mut socket = match socket {
            Ok(Ok((socket, _response))) => socket,
            Ok(Err(error)) => {
                log::warn!("stream connect to {host} failed: {}", error.to_string().replace(&config.ws_url, &host));
                attempt += 1;
                tokio::select! { _ = tokio::time::sleep(backoff(attempt)) => {}, _ = shutdown.changed() => return }
                continue;
            }
            Err(_) => {
                log::warn!("stream connect to {host} timed out");
                attempt += 1;
                tokio::select! { _ = tokio::time::sleep(backoff(attempt)) => {}, _ = shutdown.changed() => return }
                continue;
            }
        };

        book.reset();
        let frames = book.sync(&source.addresses().await);
        let mut alive = true;
        for frame in frames {
            if socket.send(Message::text(frame)).await.is_err() {
                alive = false;
                break;
            }
        }
        if alive {
            attempt = 0;
            let (wallets, _) = book.scope();
            health.connected.store(true, Ordering::Relaxed);
            health.connects.fetch_add(1, Ordering::Relaxed);
            health.last_connect_at.store(clock(), Ordering::Relaxed);
            health.subscriptions.store(wallets, Ordering::Relaxed);
            log::info!("stream connected to {host}: {wallets} wallets");
            if out.send(StreamMsg::Connected { wallets }).await.is_err() {
                return;
            }
        }

        let mut last_seen = Instant::now();
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // the first tick is immediate

        let reason = loop {
            if !alive {
                break "send failed".to_string();
            }
            tokio::select! {
                incoming = socket.next() => match incoming {
                    Some(Ok(Message::Text(text))) => {
                        last_seen = Instant::now();
                        let (event, frames) = book.on_text(text.as_str());
                        for frame in frames {
                            if socket.send(Message::text(frame)).await.is_err() {
                                alive = false;
                            }
                        }
                        health.subscriptions.store(book.scope().0, Ordering::Relaxed);
                        if let Some((wallet, signature)) = event {
                            health.events.fetch_add(1, Ordering::Relaxed);
                            health.last_event_at.store(clock(), Ordering::Relaxed);
                            if out.send(StreamMsg::Tx { wallet, signature }).await.is_err() {
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break "socket closed".to_string(),
                    Some(Ok(_)) => last_seen = Instant::now(), // pings, pongs, binary
                    Some(Err(error)) => break format!("socket error: {}", error.to_string().replace(&config.ws_url, &host)),
                },
                _ = ping.tick() => {
                    if last_seen.elapsed() > STALE_AFTER {
                        break format!("stale for {}s", last_seen.elapsed().as_secs());
                    }
                    if socket.send(Message::Ping(Default::default())).await.is_err() {
                        break "ping failed".to_string();
                    }
                }
                _ = refresh.notified() => {
                    let frames = book.sync(&source.addresses().await);
                    let changed = frames.len();
                    for frame in frames {
                        if socket.send(Message::text(frame)).await.is_err() {
                            alive = false;
                        }
                    }
                    let (wallets, _) = book.scope();
                    health.subscriptions.store(wallets, Ordering::Relaxed);
                    if changed > 0 {
                        log::info!("stream scope refreshed: {wallets} wallets ({changed} changes)");
                    }
                }
                _ = shutdown.changed() => {
                    let _ = socket.close(None).await;
                    health.connected.store(false, Ordering::Relaxed);
                    return;
                }
            }
        };

        health.connected.store(false, Ordering::Relaxed);
        log::warn!("stream to {host} dropped ({reason}); reconnecting");
        attempt += 1;
        tokio::select! { _ = tokio::time::sleep(backoff(attempt)) => {}, _ = shutdown.changed() => return }
    }
}
