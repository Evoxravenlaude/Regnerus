//! The path of one copy: signal, gate, screen, execute, record, exit.
//!
//! Paper and shadow copies take the venue's quote as their fill. A live
//! copy asks for a transaction, vets it, signs it and hands it back to be
//! landed. The position row is written before anything is sent.

use std::sync::Arc;

use serde_json::{Value, json};

use super::gate::{Copyability, Decision, ExitRules, Facts, PROOFS_TO_TRUST_SCORE, copyability, decide, decide_exit, screen, sell_fraction};
use super::sign::{Keypair, decode_base64, encode_base64, sign_transaction, vet_swap};
use super::venue::{OrderRequest, SwapVenue, VenueError};
use crate::app::App;
use crate::bot::cards;
use crate::bot::telegram::{Card, send};
use crate::chain::solana::query::{accounts, balance, mint_facts, token_balance};
use crate::chain::solana::tx::WSOL_MINT;
use crate::model::*;
use crate::obs::budget::Lane;
use crate::store::StoreResult;
use crate::util::{day_of, fmt_units};

/// A cached bubble map is trusted by the screen for this long.
const MAP_FRESH_S: i64 = 600;
/// An order that never reported back is given this long before it is judged.
const UNCONFIRMED_GRACE_S: i64 = 180;
const LEASE_TTL_S: i64 = 90;

enum CloseError {
    /// The token cannot be sold at all right now.
    NoRoute,
    Other(String),
}

impl From<VenueError> for CloseError {
    fn from(error: VenueError) -> Self {
        if error.no_route() { CloseError::NoRoute } else { CloseError::Other(error.message) }
    }
}

impl App {
    fn account_lock(&self, account: AccountId) -> Arc<tokio::sync::Mutex<()>> {
        self.account_locks.lock().entry(account).or_default().clone()
    }

    /// The admin's stop switch: no new copies while it is on. Exits still run.
    pub async fn killed(&self) -> bool {
        matches!(self.store.get_state("kill").await, Ok(Some(value)) if value == "1")
    }

    pub async fn trading_wallet(&self, account: AccountId) -> Option<TradingWallet> {
        self.store.doc("twallet", &account.to_string()).await.ok().flatten().and_then(|v| serde_json::from_value(v).ok())
    }

    /// Tells a user why a buy was not copied. The same reason for the same
    /// entity is given once an hour, not on every buy.
    async fn hold(&self, account: AccountId, entity: EntityId, why: &str) {
        let key = format!("hold:{account}:{entity}:{why}:{}", self.now() / 3_600);
        if self.processed.lock().insert(key) {
            self.tell(account, format!("Not copied: {why}.")).await;
        }
    }

    async fn tell(&self, account: AccountId, text: String) {
        if account != SHADOW_ACCOUNT {
            send(self.tg.as_ref(), account, &Card::text(text)).await;
        }
    }

    /// What the entity held of a token before one of its sells, from the trades on record.
    async fn held_before(&self, entity: EntityId, mint: &str, signature: &str) -> StoreResult<u64> {
        let mut held: i128 = 0;
        for trade in self.store.trades_of_mint(mint, 0, 5_000).await?.iter().filter(|t| t.entity_id == Some(entity) && t.signature != signature) {
            match trade.side {
                Side::Buy => held += trade.token_amount_raw as i128,
                Side::Sell => held -= trade.token_amount_raw as i128,
            }
        }
        Ok(held.max(0) as u64)
    }

    async fn facts_for(&self, account: &Account, settings: &TradeSettings, entity: &Entity, wallet: &Wallet, signal: &Signal) -> StoreResult<Facts> {
        let today = self.now() - self.now().rem_euclid(86_400);
        let mine = self.store.positions_of_account(account.id, 0).await?;
        let size_override = self.store.doc("follow_size", &format!("{}:{}", account.id, entity.id)).await?.and_then(|v| v.as_u64());
        let trusted = match wallet.linked_by {
            Some(LinkedBy::Root | LinkedBy::Proof | LinkedBy::Admin) => true,
            Some(LinkedBy::Score) => self.proof_count().await >= PROOFS_TO_TRUST_SCORE,
            _ => false,
        };
        Ok(Facts {
            follow: Some(FollowMode::Auto),
            size_override,
            source_lamports: (signal.quote == Quote::Sol).then_some(signal.quote_amount_raw),
            consensus: if settings.require_consensus { self.consensus_for(account.id, &signal.mint, settings).await?.independent } else { 0 },
            wallet_trusted: trusted,
            open_positions: mine.iter().filter(|p| p.is_open()).count(),
            spent_today: mine.iter().filter(|p| p.opened_at >= today && p.status != PositionStatus::Failed).map(|p| p.entry_lamports).sum(),
            live_enabled: self.settings.is_live(),
            has_wallet: self.trading_wallet(account.id).await.is_some(),
            killed: self.killed().await,
        })
    }

    /// Called for every new trade of a linked wallet, after it is folded
    /// into its signal. `first` is true for the trade that opened the signal.
    pub(crate) async fn trader_on_trade(self: &Arc<Self>, entity: &Entity, wallet: &Wallet, signal: &Signal, trade: &Trade, first: bool) -> StoreResult<()> {
        match trade.side {
            Side::Buy if first => {
                if self.settings.cobuy_txs > 0 {
                    let at = trade.block_time.unwrap_or(trade.seen_at);
                    self.enqueue("cobuy", &trade.signature, Some(entity.id), json!({ "mint": trade.mint, "slot": trade.slot, "at": at }), 5).await;
                }
                if self.venue().is_some() {
                    let age = self.now() - trade.block_time.unwrap_or(trade.seen_at);
                    let stale = age > self.settings.copy_max_age_s;
                    // The bot's own paper copy: how a delayed copy of this entity would have done.
                    if self.settings.shadow_lamports > 0 && !stale {
                        self.open_copy(SHADOW_ACCOUNT, PositionMode::Shadow, self.settings.shadow_lamports, signal, None, None).await;
                    }
                    for (account, mode) in self.followers(entity.id).await? {
                        if mode != FollowMode::Auto {
                            continue;
                        }
                        let settings = self.trade_settings(account.id).await;
                        let facts = self.facts_for(&account, &settings, entity, wallet, signal).await?;
                        match decide(&account, &settings, entity, &facts) {
                            Decision::Skip(_) => {}
                            Decision::Hold(why) => self.hold(account.id, entity.id, &why).await,
                            // A buy found late (after an outage, say) is copied at a price that is gone.
                            Decision::Copy { .. } if stale => self.hold(account.id, entity.id, "the buy was found too late to copy at its price").await,
                            Decision::Copy { lamports, live } => {
                                let mode = if live { PositionMode::Live } else { PositionMode::Paper };
                                self.open_copy(account.id, mode, lamports, signal, settings.slippage_bps, Some(&settings)).await;
                            }
                        }
                    }
                }
                // Copies first; the consensus alert can wait a moment.
                self.check_consensus(signal).await?;
            }
            Side::Buy => {}
            Side::Sell => {
                // The copy sells the same fraction of what it holds.
                let held = self.held_before(entity.id, &trade.mint, &trade.signature).await?;
                let fraction = sell_fraction(trade.token_amount_raw, held);
                for position in self.store.open_positions().await? {
                    if position.entity_id == entity.id && position.mint == trade.mint && position.status == PositionStatus::Open {
                        self.close_copy(position.id, fraction, "source_exit").await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Opens one copy of a signal for one account and tells the account how it went.
    ///
    /// `caps` are the user's limits, checked again here under the account's
    /// lock: ten buys arriving in the same second cannot all slip under a
    /// limit of three. A copy the user asked for by hand passes `None`.
    pub async fn open_copy(self: &Arc<Self>, account: AccountId, mode: PositionMode, lamports: u64, signal: &Signal, slippage: Option<u16>, caps: Option<&TradeSettings>) -> Option<Position> {
        let lock = self.account_lock(account);
        let _held = lock.lock().await;
        let Some(venue) = self.venue() else {
            self.tell(account, "Not copied: no swap venue is configured.".into()).await;
            return None;
        };
        if let Some(settings) = caps {
            let mine = self.store.positions_of_account(account, 0).await.unwrap_or_default();
            let today = self.now() - self.now().rem_euclid(86_400);
            let open = mine.iter().filter(|p| p.is_open()).count();
            let spent: u64 = mine.iter().filter(|p| p.opened_at >= today && p.status != PositionStatus::Failed).map(|p| p.entry_lamports).sum();
            if settings.max_open.is_some_and(|max| open >= max as usize) {
                self.hold(account, signal.entity_id, &format!("you already have {open} open positions (your limit is {})", settings.max_open.unwrap_or(0))).await;
                return None;
            }
            if settings.daily_cap_lamports.is_some_and(|cap| spent.saturating_add(lamports) > cap) {
                self.hold(account, signal.entity_id, "today's copy budget is used up").await;
                return None;
            }
        }
        let mut position = Position {
            id: 0,
            account,
            entity_id: signal.entity_id,
            signal_id: signal.id,
            mint: signal.mint.clone(),
            decimals: signal.decimals,
            mode,
            status: PositionStatus::Opening,
            entry_lamports: lamports,
            tokens_raw: 0,
            tokens_left: 0,
            exit_lamports: 0,
            opened_at: self.now(),
            closed_at: None,
            exit_reason: None,
            open_signature: None,
            note: String::new(),
        };
        // Written before anything is sent. A second copy of the same signal stops here.
        match self.store.insert_position(&position).await {
            Ok(Some(id)) => position.id = id,
            Ok(None) => return None,
            Err(error) => {
                log::error!("could not record a position for {account}: {error}");
                return None;
            }
        }
        if let Err(why) = self.fill_open(venue.as_ref(), &mut position, slippage).await {
            position.status = PositionStatus::Failed;
            position.note = why;
        }
        if let Err(error) = self.store.update_position(&position).await {
            log::error!("could not save position {}: {error}", position.id);
        }
        if account != SHADOW_ACCOUNT {
            send(self.tg.as_ref(), account, &cards::copy_card(&position)).await;
        }
        log::info!("{} copy {} for {account}: {} ({})", mode, position.id, position.status, position.note);
        Some(position)
    }

    async fn fill_open(&self, venue: &dyn SwapVenue, position: &mut Position, slippage: Option<u16>) -> Result<(), String> {
        let lamports = position.entry_lamports;
        let live = position.mode == PositionMode::Live;
        let wallet = if live { Some(self.live_wallet(position.account, lamports).await?) } else { None };
        let facts = accounts(self.rpc.as_ref(), std::slice::from_ref(&position.mint), Lane::Copy)
            .await
            .map(|infos| mint_facts(infos.first().unwrap_or(&Value::Null)))
            .map_err(|e| format!("could not read the token ({e})"))?;
        let request =
            OrderRequest { input_mint: WSOL_MINT.to_string(), output_mint: position.mint.clone(), amount: lamports, taker: wallet.as_ref().map(|w| w.address.clone()), slippage_bps: slippage };
        let order = venue.order(&request).await.map_err(|e| e.message)?;
        let cluster = self
            .store
            .doc("map", &position.mint)
            .await
            .ok()
            .flatten()
            .filter(|d| d.get("at").and_then(Value::as_i64).is_some_and(|at| self.now() - at <= MAP_FRESH_S))
            .and_then(|d| d.get("largest_bps").and_then(Value::as_u64));
        screen(&facts, order.price_impact_pct, cluster)?;
        position.note = format!("price impact {:.2}%", order.price_impact_pct.abs());
        let Some(wallet) = wallet else {
            // Paper: the quote is the fill.
            position.tokens_raw = order.out_amount;
            position.tokens_left = order.out_amount;
            position.status = PositionStatus::Open;
            return Ok(());
        };
        let (signed, id) = self.sign_order(&wallet, order.transaction.as_deref(), order.gasless, WSOL_MINT, lamports).await?;
        position.open_signature = Some(id);
        self.store.update_position(position).await.map_err(|e| e.to_string())?;
        match venue.execute(&signed, &order.request_id).await {
            Ok(fill) if fill.success => {
                position.tokens_raw = if fill.output > 0 { fill.output } else { order.out_amount };
                position.tokens_left = position.tokens_raw;
                position.entry_lamports = if fill.input > 0 { fill.input } else { lamports };
                position.open_signature = fill.signature.or(position.open_signature.take());
                position.status = PositionStatus::Open;
                Ok(())
            }
            Ok(fill) => Err(format!("the swap did not go through: {} (code {})", fill.error.unwrap_or_else(|| "no reason given".into()), fill.code)),
            // Sent, and nothing came back: it may have landed. The monitor checks the wallet.
            Err(error) if error.code == "network" => {
                position.status = PositionStatus::Unconfirmed;
                position.note = "sent; the venue did not answer, checking the wallet".to_string();
                Ok(())
            }
            Err(error) => Err(error.message),
        }
    }

    /// Everything that must hold before real money moves, and the wallet to move it from.
    async fn live_wallet(&self, account: AccountId, lamports: u64) -> Result<TradingWallet, String> {
        if !self.settings.is_live() {
            return Err("live trading is switched off on the server".into());
        }
        if self.killed().await {
            return Err("trading is stopped by an admin".into());
        }
        if !self.store.acquire_lease("executor", &self.instance, LEASE_TTL_S, self.now()).await.map_err(|e| e.to_string())? {
            return Err("another running copy of the bot holds the executor lease".into());
        }
        let wallet = self.trading_wallet(account).await.ok_or("you have no trading wallet (/wallets)")?;
        if lamports > 0 {
            let held = balance(self.rpc.as_ref(), &wallet.address, Lane::Copy).await.map_err(|e| format!("could not read the wallet balance ({e})"))?;
            let needed = lamports.saturating_add(self.settings.sol_reserve_lamports);
            if held < needed {
                return Err(format!("the trading wallet holds {} SOL; this copy needs {} with the reserve", fmt_units(held, 9, 4), fmt_units(needed, 9, 4)));
            }
        }
        Ok(wallet)
    }

    /// Vets the venue's transaction and signs it. Returns the signed bytes as base64 and the transaction id.
    async fn sign_order(&self, wallet: &TradingWallet, transaction: Option<&str>, gasless: bool, input_mint: &str, input_amount: u64) -> Result<(String, String), String> {
        let transaction = transaction.ok_or("the venue returned no transaction")?;
        if gasless {
            return Err("the venue offered a sponsored transaction, which needs a second signer; not signed".into());
        }
        let bytes = decode_base64(transaction).map_err(|e| e.to_string())?;
        vet_swap(&bytes, &wallet.address, input_mint, input_amount, &[]).map_err(|why| format!("refused to sign: {why}"))?;
        let keys = self.keys().ok_or("no key provider is configured")?;
        let secret = keys.reveal(&wallet.secret_ref).await?;
        let key = Keypair::from_secret(&secret).map_err(|e| e.to_string())?;
        if key.address() != wallet.address {
            return Err("the stored key does not belong to the trading wallet's address".into());
        }
        let (signed, id) = sign_transaction(&bytes, &key).map_err(|e| e.to_string())?;
        Ok((encode_base64(&signed), id))
    }

    /// Sells a fraction of a position. A close never sells more than the position holds.
    pub async fn close_copy(self: &Arc<Self>, id: i64, fraction: f64, reason: &str) {
        let Ok(Some(peek)) = self.store.position(id).await else { return };
        let lock = self.account_lock(peek.account);
        let _held = lock.lock().await;
        let Ok(Some(mut position)) = self.store.position(id).await else { return };
        if position.status != PositionStatus::Open || position.tokens_left == 0 || fraction <= 0.0 {
            return;
        }
        let Some(venue) = self.venue() else { return };
        let amount = if fraction >= 0.999 { position.tokens_left } else { ((position.tokens_left as f64 * fraction) as u64).clamp(1, position.tokens_left) };
        match self.fill_close(venue.as_ref(), &position, amount).await {
            Ok((sold, lamports)) => {
                position.tokens_left -= sold.min(position.tokens_left);
                position.exit_lamports += lamports;
                position.note.clear();
            }
            // A token that cannot be sold when its time is up is a total loss on paper.
            Err(CloseError::NoRoute) if position.mode != PositionMode::Live && reason == "max_hold" => position.tokens_left = 0,
            Err(CloseError::NoRoute) => {
                self.close_blocked(&mut position, "no route to sell this token right now").await;
                return;
            }
            Err(CloseError::Other(why)) => {
                self.close_blocked(&mut position, &why).await;
                return;
            }
        }
        if position.tokens_left == 0 {
            position.status = PositionStatus::Closed;
            position.closed_at = Some(self.now());
            position.exit_reason = Some(reason.to_string());
            self.exit_hits.lock().remove(&position.id);
        }
        if let Err(error) = self.store.update_position(&position).await {
            log::error!("could not save position {}: {error}", position.id);
        }
        if position.account != SHADOW_ACCOUNT {
            send(self.tg.as_ref(), position.account, &cards::exit_card(&position, reason)).await;
        }
    }

    /// A close that could not be sent: the position stays open and the user hears about it once.
    async fn close_blocked(&self, position: &mut Position, why: &str) {
        let note = format!("close blocked: {why}");
        if position.note != note {
            position.note = note.clone();
            let _ = self.store.update_position(position).await;
            self.tell(position.account, format!("Position #{} could not be closed yet: {why}. It will be tried again.", position.id)).await;
        }
    }

    /// Returns `(tokens sold, lamports received)`.
    async fn fill_close(&self, venue: &dyn SwapVenue, position: &Position, amount: u64) -> Result<(u64, u64), CloseError> {
        let live = position.mode == PositionMode::Live;
        // Exits ignore the reserve and the stop switch: a close is never blocked by them.
        let wallet = if live { Some(self.trading_wallet(position.account).await.ok_or_else(|| CloseError::Other("the trading wallet is gone".into()))?) } else { None };
        let request = OrderRequest { input_mint: position.mint.clone(), output_mint: WSOL_MINT.to_string(), amount, taker: wallet.as_ref().map(|w| w.address.clone()), slippage_bps: None };
        let order = venue.order(&request).await?;
        let Some(wallet) = wallet else { return Ok((amount, order.out_amount)) };
        if !self.settings.is_live() {
            return Err(CloseError::Other("live trading is switched off on the server".into()));
        }
        let (signed, _) = self.sign_order(&wallet, order.transaction.as_deref(), order.gasless, &position.mint, amount).await.map_err(CloseError::Other)?;
        let fill = venue.execute(&signed, &order.request_id).await?;
        if !fill.success {
            return Err(CloseError::Other(format!("the sell did not go through: {} (code {})", fill.error.unwrap_or_else(|| "no reason given".into()), fill.code)));
        }
        Ok((if fill.input > 0 { fill.input } else { amount }, if fill.output > 0 { fill.output } else { order.out_amount }))
    }

    /// One pass over every open position: settle the unconfirmed, mark the
    /// rest to market and apply the exit rules. Returns how many it looked at.
    pub async fn monitor_positions(self: &Arc<Self>) -> usize {
        let positions = match self.store.open_positions().await {
            Ok(positions) => positions,
            Err(error) => {
                log::warn!("monitor could not read positions: {error}");
                return 0;
            }
        };
        let Some(venue) = self.venue() else { return 0 };
        let now = self.now();
        let count = positions.len();
        // One quote per token per pass: positions in the same token share its price.
        let mut prices: std::collections::HashMap<String, Option<f64>> = std::collections::HashMap::new();
        for mut position in positions {
            match position.status {
                PositionStatus::Opening | PositionStatus::Unconfirmed => {
                    if position.status == PositionStatus::Opening && now - position.opened_at < 60 {
                        continue; // still being opened
                    }
                    self.settle_unconfirmed(&mut position).await;
                }
                PositionStatus::Open => {
                    let rules = if position.account == SHADOW_ACCOUNT { ExitRules::from(&TradeSettings::default()) } else { ExitRules::from(&self.trade_settings(position.account).await) };
                    let price = match prices.get(&position.mint) {
                        Some(price) => *price,
                        None => {
                            let request = OrderRequest { input_mint: position.mint.clone(), output_mint: WSOL_MINT.to_string(), amount: position.tokens_left, taker: None, slippage_bps: None };
                            let price = venue.order(&request).await.ok().filter(|_| position.tokens_left > 0).map(|order| order.out_amount as f64 / position.tokens_left as f64);
                            prices.insert(position.mint.clone(), price);
                            price
                        }
                    };
                    let value = price.map(|price| (position.tokens_left as f64 * price) as u64 + position.exit_lamports);
                    match decide_exit(&rules, position.entry_lamports, value, position.opened_at, now) {
                        Some("max_hold") => self.close_copy(position.id, 1.0, "max_hold").await,
                        Some(hit) => {
                            // One reading of a thin market is not enough to sell on.
                            let again = self.exit_hits.lock().insert(position.id, hit) == Some(hit);
                            if again {
                                self.close_copy(position.id, 1.0, hit).await;
                            }
                        }
                        None => {
                            self.exit_hits.lock().remove(&position.id);
                        }
                    }
                }
                _ => {}
            }
        }
        count
    }

    /// An order whose outcome never came back: the wallet's balance is the answer.
    async fn settle_unconfirmed(self: &Arc<Self>, position: &mut Position) {
        let age = self.now() - position.opened_at;
        if position.mode != PositionMode::Live {
            position.status = PositionStatus::Failed;
            position.note = "interrupted before it was filled".into();
        } else {
            let Some(wallet) = self.trading_wallet(position.account).await else { return };
            match token_balance(self.rpc.as_ref(), &wallet.address, &position.mint, Lane::Watch).await {
                Ok(held) if held > 0 => {
                    position.status = PositionStatus::Open;
                    position.tokens_raw = held;
                    position.tokens_left = held;
                    position.note = "confirmed from the wallet's balance".into();
                }
                Ok(_) if age > UNCONFIRMED_GRACE_S => {
                    position.status = PositionStatus::Failed;
                    position.note = "the order never landed".into();
                }
                _ => return,
            }
        }
        if self.store.update_position(position).await.is_ok() && position.account != SHADOW_ACCOUNT {
            send(self.tg.as_ref(), position.account, &cards::copy_card(position)).await;
        }
    }

    pub async fn copyability_of(&self, entity: &Entity) -> StoreResult<Copyability> {
        let shadow = self.store.positions_of_entity(entity.id, PositionMode::Shadow).await?;
        let trades = self.store.trades_of_entity(entity.id, 500).await?;
        Ok(copyability(&shadow, &trades, entity.bait))
    }

    /// Re-runs the copyability gate for every tracked entity. An entity is
    /// promoted as soon as it passes and demoted after two failed runs.
    pub async fn copyability_pass(self: &Arc<Self>) -> StoreResult<()> {
        for entity in self.store.entities().await?.into_iter().filter(|e| e.status == EntityStatus::Tracking) {
            let key = entity.id.to_string();
            let gate = self.store.doc("copygate", &key).await?.unwrap_or_else(|| json!({}));
            if gate.get("manual").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            let failures = self.copyability_of(&entity).await?.failures();
            let fails = if failures.is_empty() { 0 } else { gate.get("fails").and_then(Value::as_i64).unwrap_or(0) + 1 };
            self.store.put_doc("copygate", &key, &json!({ "fails": fails, "at": self.now(), "day": day_of(self.now()) }), self.now()).await?;
            if failures.is_empty() && !entity.copyable {
                self.store.set_entity_flags(entity.id, Some(true), None).await?;
                self.notify_followers(
                    entity.id,
                    &Card::text(format!("<b>{}</b> is now copyable: its paper copies pass every check. Followers on auto will copy its next buy.", crate::util::esc(&entity.tag()))),
                )
                .await;
            } else if !failures.is_empty() && entity.copyable && fails >= 2 {
                self.store.set_entity_flags(entity.id, Some(false), None).await?;
                self.notify_followers(entity.id, &Card::text(format!("<b>{}</b> is no longer copyable: {}.", crate::util::esc(&entity.tag()), failures.join("; ")))).await;
            }
        }
        Ok(())
    }

    /// The bait detector. A wallet tied to an entity by evidence, but not
    /// linked, sold a token minutes after the entity bought it.
    pub(crate) async fn check_bait(self: &Arc<Self>, wallet: &Wallet, trade: &Trade) -> StoreResult<()> {
        let Some(entity_id) = wallet.entity_id else { return Ok(()) };
        if trade.side != Side::Sell || wallet.state == WalletState::Linked {
            return Ok(());
        }
        let at = trade.block_time.unwrap_or(trade.seen_at);
        let bought = self.store.signals_since(at - self.settings.bait_window_s).await?.into_iter().any(|s| s.entity_id == entity_id && s.mint == trade.mint && s.side == Side::Buy && s.first_at <= at);
        let Some(entity) = self.store.entity(entity_id).await?.filter(|e| !e.bait) else { return Ok(()) };
        if bought {
            self.store.set_entity_flags(entity_id, Some(false), Some(true)).await?;
            log::warn!("{} flagged as bait: {} sold {} after its buy", entity.tag(), wallet.address, trade.mint);
            self.notify_followers(entity_id, &cards::bait_card(&entity, &wallet.address, &trade.mint)).await;
        }
        Ok(())
    }
}
