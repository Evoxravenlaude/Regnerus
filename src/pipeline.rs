//! From a signature to a stored trade and a card.
//!
//! A watched wallet appears in a transaction; the transaction is fetched
//! and classified; a trade is stored once (that insert is the claim-once
//! rule) and folded into its entity's signal; followers get one card per
//! signal, edited in place while more trades join it.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::app::App;
use crate::bot::cards::signal_card;
use crate::bot::telegram::{edit, send};
use crate::chain::solana::stream::StreamMsg;
use crate::chain::solana::tx::{Classified, ParsedTx, classify, fingerprint};
use crate::model::*;
use crate::obs::budget::Lane;
use crate::obs::health::Health;
use crate::store::StoreResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Stored as a trade. `new` is false when it was already there.
    Trade {
        new: bool,
    },
    /// Stored this many new transfers.
    Transfers(usize),
    /// A transaction that is neither, or a failed one.
    Other,
    /// The wallet is not one the bot knows.
    UnknownWallet,
    /// The node did not have the transaction after every retry.
    Missing,
    Failed(String),
}

impl App {
    /// Handles one signature for one watched wallet. `live` events use the
    /// copy lane; history and backfill use the watch lane. `silent` stores
    /// without sending cards.
    pub async fn process_signature(self: &Arc<Self>, wallet: &str, signature: &str, live: bool, silent: bool) -> Outcome {
        let key = format!("{signature}:{wallet}");
        if !self.processed.lock().insert(key.clone()) {
            return Outcome::Other;
        }
        let outcome = self.process_inner(wallet, signature, live, silent).await;
        if matches!(outcome, Outcome::Missing | Outcome::Failed(_)) {
            // Not handled: let the backfill try again.
            self.processed.lock().remove(&key);
            Health::bump(if outcome == Outcome::Missing { &self.health.tx_missing } else { &self.health.errors });
        }
        outcome
    }

    /// Fetches and parses a transaction, waiting a moment between tries: a
    /// transaction can lag its log notification.
    async fn fetch_tx(&self, address: &str, signature: &str, lane: Lane) -> Result<ParsedTx, Outcome> {
        let mut raw = None;
        for wait in &self.tx_retry_ms {
            if *wait > 0 {
                tokio::time::sleep(Duration::from_millis(*wait)).await;
            }
            match self.rpc.get_transaction(signature, lane).await {
                Ok(Some(value)) => {
                    raw = Some(value);
                    break;
                }
                Ok(None) => continue,
                Err(error) => return Err(Outcome::Failed(error.to_string())),
            }
        }
        let Some(raw) = raw else {
            log::warn!("transaction {signature} for {address} was not available after {} tries", self.tx_retry_ms.len());
            return Err(Outcome::Missing);
        };
        Health::bump(&self.health.tx_fetched);
        ParsedTx::from_value(&raw).ok_or_else(|| Outcome::Failed("the transaction did not parse".into()))
    }

    async fn process_inner(self: &Arc<Self>, address: &str, signature: &str, live: bool, silent: bool) -> Outcome {
        let lane = if live { Lane::Copy } else { Lane::Watch };
        let wallet = match self.store.wallet(address).await {
            Ok(Some(wallet)) => wallet,
            // Not a wallet: it may be a deposit address the stream watches as a tripwire.
            Ok(None) => {
                return match self.store.deposit(address).await {
                    Ok(Some(deposit)) if deposit.status == DepositStatus::Qualified => match self.fetch_tx(address, signature, lane).await {
                        Ok(tx) => match self.on_tripwire(&deposit, &tx, signature).await {
                            Ok(proofs) => Outcome::Transfers(proofs),
                            Err(error) => Outcome::Failed(error.to_string()),
                        },
                        Err(outcome) => outcome,
                    },
                    Ok(_) => Outcome::UnknownWallet,
                    Err(error) => Outcome::Failed(error.to_string()),
                };
            }
            Err(error) => return Outcome::Failed(error.to_string()),
        };
        let tx = match self.fetch_tx(address, signature, lane).await {
            Ok(tx) => tx,
            Err(outcome) => return outcome,
        };

        match classify(&tx, address) {
            Classified::Trade(core) => {
                let print = fingerprint(&tx, address, &|a| self.labels.get(a));
                let trade = Trade {
                    signature: core.signature,
                    slot: core.slot,
                    block_time: core.block_time,
                    seen_at: self.now(),
                    wallet: address.to_string(),
                    entity_id: wallet.entity_id,
                    side: core.side,
                    mint: core.mint,
                    token_amount_raw: core.token_amount_raw,
                    decimals: core.decimals,
                    token_program: core.token_program,
                    quote: core.quote,
                    quote_amount_raw: core.quote_amount_raw,
                    programs: core.programs,
                    venue: core.venue,
                    fingerprint: print,
                };
                match self.on_trade(&wallet, trade, live, silent).await {
                    Ok(new) => Outcome::Trade { new },
                    Err(error) => Outcome::Failed(error.to_string()),
                }
            }
            Classified::Transfers(found) => {
                let mut stored = 0;
                for core in found {
                    // A tip is part of how a transaction was sent, not money moved to someone.
                    if self.labels.is_class(&core.counterparty, LabelClass::Tip) {
                        continue;
                    }
                    let transfer = Transfer {
                        signature: signature.to_string(),
                        slot: tx.slot,
                        block_time: tx.block_time,
                        wallet: address.to_string(),
                        direction: core.direction,
                        counterparty: core.counterparty,
                        mint: core.mint,
                        amount_raw: core.amount_raw,
                        decimals: core.decimals,
                    };
                    match self.store.insert_transfer(&transfer).await {
                        Ok(true) => {
                            stored += 1;
                            Health::bump(&self.health.transfers);
                            // Where the money goes is the trail engine's business.
                            if !silent && let Err(error) = self.on_transfer(&wallet, &transfer).await {
                                log::warn!("exit watch on {signature} failed: {error}");
                            }
                        }
                        Ok(false) => {}
                        Err(error) => return Outcome::Failed(error.to_string()),
                    }
                }
                Outcome::Transfers(stored)
            }
            Classified::Other => Outcome::Other,
        }
    }

    /// Stores the trade and, when it is new, fresh and by a linked wallet of
    /// a tracked entity, folds it into a signal and tells the followers.
    async fn on_trade(self: &Arc<Self>, wallet: &Wallet, trade: Trade, live: bool, silent: bool) -> StoreResult<bool> {
        if !self.store.insert_trade(&trade).await? {
            return Ok(false);
        }
        Health::bump(&self.health.trades);
        if live && let Some(block_time) = trade.block_time {
            self.health.record_delay(trade.seen_at - block_time);
        }
        if silent {
            return Ok(true);
        }
        if wallet.state != WalletState::Linked {
            // A candidate's trades send no cards; they are read for what
            // they say about whose wallet it is.
            if let Err(error) = self.on_candidate_trade(wallet, &trade).await {
                log::warn!("behaviour check on {} failed: {error}", trade.signature);
            }
            if let Err(error) = self.check_bait(wallet, &trade).await {
                log::warn!("bait check on {} failed: {error}", trade.signature);
            }
            return Ok(true);
        }
        let Some(entity_id) = wallet.entity_id else { return Ok(true) };
        let Some(entity) = self.store.entity(entity_id).await? else { return Ok(true) };
        if entity.status != EntityStatus::Tracking {
            return Ok(true);
        }
        let at = trade.block_time.unwrap_or(trade.seen_at);
        if self.now() - at > self.settings.alert_max_age_s {
            log::info!("{} {} by {} is {}s old; stored without a card", trade.side, trade.mint, entity.tag(), self.now() - at);
            return Ok(true);
        }

        let lock = self.signal_lock(entity_id, &trade.mint, trade.side);
        let _held = lock.lock().await;
        let open = self.store.open_signal(entity_id, &trade.mint, trade.side, at - self.settings.signal_window_s).await?;
        match open.filter(|signal| signal.quote == trade.quote) {
            Some(mut signal) => {
                signal.last_at = signal.last_at.max(at);
                signal.quote_amount_raw = signal.quote_amount_raw.saturating_add(trade.quote_amount_raw);
                signal.token_amount_raw = signal.token_amount_raw.saturating_add(trade.token_amount_raw);
                signal.trades += 1;
                if !signal.wallets.contains(&trade.wallet) {
                    signal.wallets.push(trade.wallet.clone());
                }
                self.store.update_signal(&signal).await?;
                let card = self.card_for(signal_card(&entity, &signal, &trade), &signal);
                for (account, message_id) in self.store.signal_messages(signal.id).await? {
                    edit(self.tg.as_ref(), account, message_id, &card).await;
                }
                drop(_held);
                if let Err(error) = self.trader_on_trade(&entity, wallet, &signal, &trade, false).await {
                    log::warn!("trader on {} failed: {error}", trade.signature);
                }
            }
            None => {
                let signal = self
                    .store
                    .create_signal(&Signal {
                        id: 0,
                        entity_id,
                        mint: trade.mint.clone(),
                        side: trade.side,
                        first_at: at,
                        last_at: at,
                        quote: trade.quote,
                        quote_amount_raw: trade.quote_amount_raw,
                        token_amount_raw: trade.token_amount_raw,
                        decimals: trade.decimals,
                        wallets: vec![trade.wallet.clone()],
                        trades: 1,
                        venue: trade.venue,
                    })
                    .await?;
                let card = self.card_for(signal_card(&entity, &signal, &trade), &signal);
                for follow in self.store.follows_of_entity(entity_id).await? {
                    if follow.mode == FollowMode::Off {
                        continue;
                    }
                    let paused = self.store.account(follow.account).await?.is_none_or(|a| a.paused);
                    if paused {
                        continue;
                    }
                    if let Some(message_id) = send(self.tg.as_ref(), follow.account, &card).await {
                        Health::bump(&self.health.cards);
                        self.store.set_signal_message(signal.id, follow.account, message_id).await?;
                    }
                }
                drop(_held);
                // The first trade of a signal is what a copy acts on.
                if let Err(error) = self.trader_on_trade(&entity, wallet, &signal, &trade, true).await {
                    log::warn!("trader on {} failed: {error}", trade.signature);
                }
            }
        }
        Ok(true)
    }

    /// A buy card gets a Copy button once there is somewhere to send a copy.
    fn card_for(&self, card: crate::bot::telegram::Card, signal: &Signal) -> crate::bot::telegram::Card {
        if self.venue().is_some() { crate::bot::cards::with_copy_button(card, signal) } else { card }
    }

    fn signal_lock(&self, entity: EntityId, mint: &str, side: Side) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.signal_locks.lock();
        if locks.len() > 2_000 {
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        locks.entry(format!("{entity}:{mint}:{side}")).or_default().clone()
    }

    /// Reads what a wallet did since its cursor and handles each signature,
    /// oldest first. A wallet with no cursor yet gets its recent history
    /// read silently: tracking a wallet never fires cards for its past.
    pub async fn backfill_wallet(self: &Arc<Self>, address: &str) -> usize {
        let wallet = match self.store.wallet(address).await {
            Ok(Some(wallet)) => wallet,
            _ => return 0,
        };
        // No cursor at all means the wallet has never been read. An empty
        // cursor means it was read and had no transactions yet: a fresh
        // wallet, whose first transaction must not be mistaken for history.
        let first_time = wallet.last_signature.is_none();
        let until = wallet.last_signature.as_deref().filter(|cursor| !cursor.is_empty());
        // With no history wanted, one signature is still read: it becomes
        // the cursor that later passes start from.
        let limit = if first_time { self.settings.history_on_track.max(1) } else { self.settings.backfill_limit };
        let rows = match self.rpc.get_signatures(address, until, limit, Lane::Watch).await {
            Ok(rows) => rows,
            Err(error) => {
                log::warn!("backfill of {address} failed: {error}");
                return 0;
            }
        };
        let Some(newest) = rows.first().map(|row| row.signature.clone()) else {
            if first_time && let Err(error) = self.store.set_cursor(address, "").await {
                log::warn!("could not save the cursor of {address}: {error}");
            }
            return 0;
        };
        if first_time && self.settings.history_on_track == 0 {
            if let Err(error) = self.store.set_cursor(address, &newest).await {
                log::warn!("could not save the cursor of {address}: {error}");
            }
            return 0;
        }
        if !first_time && rows.len() >= limit {
            log::warn!("{address} had {limit} or more transactions since the last check; older ones in the gap were not read");
        }
        let mut handled = 0;
        let mut complete = true;
        for row in rows.iter().rev() {
            if row.failed || self.processed.lock().contains(&format!("{}:{address}", row.signature)) {
                continue;
            }
            match self.process_signature(address, &row.signature, false, first_time).await {
                Outcome::Missing | Outcome::Failed(_) => complete = false,
                _ => handled += 1,
            }
        }
        // Only move the cursor past signatures that were all handled; a
        // failed lookup is retried on the next pass.
        if complete && let Err(error) = self.store.set_cursor(address, &newest).await {
            log::warn!("could not save the cursor of {address}: {error}");
        }
        handled
    }

    /// One pass over every watched wallet.
    pub async fn backfill_all(self: &Arc<Self>) -> usize {
        let wallets = match self.store.watched_wallets().await {
            Ok(wallets) => wallets,
            Err(error) => {
                log::warn!("backfill could not list wallets: {error}");
                return 0;
            }
        };
        let mut handled = 0;
        for wallet in wallets {
            handled += self.backfill_wallet(&wallet.address).await;
        }
        for deposit in self.store.watched_deposits().await.unwrap_or_default() {
            handled += self.backfill_deposit(&deposit.address).await;
        }
        handled
    }

    /// Reads what reached a deposit address while the stream was not
    /// listening. Its history from before it was tagged is not replayed:
    /// the deposit test already read that.
    async fn backfill_deposit(self: &Arc<Self>, address: &str) -> usize {
        let cursor = self.store.doc("depcursor", address).await.ok().flatten().and_then(|d| d.as_str().map(str::to_string));
        let limit = if cursor.is_some() { self.settings.backfill_limit } else { 1 };
        let Ok(rows) = self.rpc.get_signatures(address, cursor.as_deref(), limit, Lane::Watch).await else { return 0 };
        let Some(newest) = rows.first().map(|row| row.signature.clone()) else { return 0 };
        let mut handled = 0;
        if cursor.is_some() {
            for row in rows.iter().rev().filter(|row| !row.failed) {
                if !matches!(self.process_signature(address, &row.signature, false, false).await, Outcome::Missing | Outcome::Failed(_)) {
                    handled += 1;
                }
            }
        }
        let _ = self.store.put_doc("depcursor", address, &serde_json::json!(newest), self.now()).await;
        handled
    }

    /// One message from the stream.
    pub async fn on_stream(self: &Arc<Self>, msg: StreamMsg) {
        match msg {
            StreamMsg::Tx { wallet, signature } => {
                if let Outcome::Failed(error) = self.process_signature(&wallet, &signature, true, false).await {
                    log::error!("handling {signature} for {wallet} failed: {error}");
                }
            }
            StreamMsg::Connected { wallets } => {
                // Whatever happened while the socket was down is read now.
                let handled = self.backfill_all().await;
                log::info!("backfill after connect: {wallets} wallets, {handled} transactions handled");
            }
        }
    }

    /// Whether the stream looks stuck, for the health message.
    pub fn stream_down_for(&self) -> Option<i64> {
        if self.health.connected.load(Ordering::Relaxed) {
            return None;
        }
        let since = self.health.last_connect_at.load(Ordering::Relaxed).max(self.health.started_at.load(Ordering::Relaxed));
        Some(self.now() - since)
    }
}
