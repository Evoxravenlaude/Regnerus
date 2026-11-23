//! The decisions of the copy path, as pure functions: whether a user
//! copies a signal, whether a token passes the screen, whether a position
//! should close, and whether an entity is worth copying at all.

use crate::chain::solana::query::MintFacts;
use crate::model::*;

/// A copy that would move the price more than this is not sent.
pub const MAX_IMPACT_PCT: f64 = 10.0;
/// A token whose largest holder cluster owns more than this share is not copied (30%).
pub const MAX_CLUSTER_BPS: u64 = 3_000;
/// Proofs the calibration table must hold before a score-linked wallet may trigger a copy.
pub const PROOFS_TO_TRUST_SCORE: i64 = 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do and nothing to say.
    Skip(&'static str),
    /// Not copied; the user is told why.
    Hold(String),
    Copy {
        lamports: u64,
        live: bool,
    },
}

/// What the gate needs to know beyond the user's settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub follow: Option<FollowMode>,
    /// A size set for this entity with `/follow ID auto SIZE`.
    pub size_override: Option<u64>,
    /// The entity's buy, in lamports, when it paid in SOL.
    pub source_lamports: Option<u64>,
    /// Independent entities this user follows that bought the token in the user's window.
    pub consensus: usize,
    /// The wallet that traded is linked by proof, by an admin, or by a calibrated score.
    pub wallet_trusted: bool,
    pub open_positions: usize,
    pub spent_today: u64,
    /// The server sends real orders.
    pub live_enabled: bool,
    pub has_wallet: bool,
    pub killed: bool,
}

/// One user, one signal. Ported from Yeks's `decide`.
pub fn decide(account: &Account, settings: &TradeSettings, entity: &Entity, facts: &Facts) -> Decision {
    if account.paused {
        return Decision::Skip("paused");
    }
    if facts.follow != Some(FollowMode::Auto) {
        return Decision::Skip("not on auto");
    }
    if facts.killed {
        return Decision::Hold("trading is stopped by an admin (/kill)".into());
    }
    if entity.bait {
        return Decision::Hold("this entity is flagged as bait: its side wallets sold into its own buys".into());
    }
    if !entity.copyable {
        return Decision::Hold("this entity has not yet shown that a delayed copy of it makes money (/paper shows where it stands)".into());
    }
    if !facts.wallet_trusted {
        return Decision::Hold("the wallet that bought is linked by score only; until the calibration table is full, such wallets send cards and are not copied".into());
    }
    if let Some(source) = facts.source_lamports
        && source < settings.min_source_lamports
    {
        return Decision::Skip("below the minimum size");
    }
    if settings.require_consensus && facts.consensus < settings.min_entities.max(2) as usize {
        return Decision::Hold(format!("waiting for consensus: {} of {} entities so far", facts.consensus, settings.min_entities.max(2)));
    }
    let lamports = facts.size_override.unwrap_or(settings.size_lamports);
    if lamports == 0 {
        return Decision::Skip("size is zero");
    }
    if let Some(max) = settings.max_open
        && facts.open_positions >= max as usize
    {
        return Decision::Hold(format!("you already have {} open positions (your limit is {max})", facts.open_positions));
    }
    if let Some(cap) = settings.daily_cap_lamports
        && facts.spent_today.saturating_add(lamports) > cap
    {
        return Decision::Hold("today's copy budget is used up".into());
    }
    let live = settings.live && facts.live_enabled;
    if live && !facts.has_wallet {
        return Decision::Hold("you are set to live but have no trading wallet (/wallets)".into());
    }
    Decision::Copy { lamports, live }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitRules {
    pub tp_pct: Option<u32>,
    pub sl_pct: Option<u32>,
    pub max_hold_min: Option<u32>,
}

impl From<&TradeSettings> for ExitRules {
    fn from(settings: &TradeSettings) -> Self {
        ExitRules { tp_pct: settings.tp_pct, sl_pct: settings.sl_pct, max_hold_min: settings.max_hold_min }
    }
}

/// `value` is what the position is worth now plus what it has already
/// returned. Unknown value leaves only the time limit, so an unvalued
/// position is never stuck open.
pub fn decide_exit(rules: &ExitRules, entry_lamports: u64, value_lamports: Option<u64>, opened_at: i64, now: i64) -> Option<&'static str> {
    if let Some(value) = value_lamports
        && entry_lamports > 0
    {
        let pct = (value as f64 - entry_lamports as f64) / entry_lamports as f64 * 100.0;
        if rules.tp_pct.is_some_and(|tp| pct >= tp as f64) {
            return Some("take_profit");
        }
        if rules.sl_pct.is_some_and(|sl| pct <= -(sl as f64)) {
            return Some("stop_loss");
        }
    }
    if rules.max_hold_min.is_some_and(|max| now - opened_at >= max as i64 * 60) {
        return Some("max_hold");
    }
    None
}

/// The token screen. `Err` says why a copy becomes a card.
pub fn screen(mint: &MintFacts, price_impact_pct: f64, largest_cluster_bps: Option<u64>) -> Result<(), String> {
    if !mint.exists {
        return Err("the token's mint account could not be read".into());
    }
    if mint.freeze_authority.is_some() {
        return Err("someone can still freeze holders' accounts (freeze authority is set)".into());
    }
    if mint.mint_authority.is_some() {
        return Err("someone can still mint more supply (mint authority is set)".into());
    }
    const BLOCKING: &[(&str, &str)] = &[
        ("transferFeeConfig", "it taxes every transfer"),
        ("nonTransferable", "it cannot be transferred"),
        ("permanentDelegate", "a delegate can move or burn holders' tokens"),
        ("transferHook", "a program runs on every transfer and can block it"),
        ("defaultAccountState", "new accounts can start frozen"),
        ("pausableConfig", "transfers can be paused"),
    ];
    for extension in &mint.extensions {
        if let Some((_, why)) = BLOCKING.iter().find(|(name, _)| name == extension) {
            return Err(format!("a Token-2022 extension: {why}"));
        }
    }
    if price_impact_pct.abs() > MAX_IMPACT_PCT {
        return Err(format!("your size would move the price by {:.1}%", price_impact_pct.abs()));
    }
    if largest_cluster_bps.is_some_and(|bps| bps > MAX_CLUSTER_BPS) {
        return Err(format!("one cluster of wallets holds {:.0}% of the supply", largest_cluster_bps.unwrap_or(0) as f64 / 100.0));
    }
    Ok(())
}

/// The share of a position to sell when the entity sells: the same
/// fraction of its holdings. `held_before` is what the entity held of the
/// token before this sell.
pub fn sell_fraction(sold: u64, held_before: u64) -> f64 {
    if sold == 0 {
        return 0.0;
    }
    let fraction = sold as f64 / held_before.max(sold) as f64;
    if fraction >= 0.95 { 1.0 } else { fraction }
}

/// Where an entity stands against the copyability gate.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Copyability {
    pub closed: usize,
    pub pnl_lamports: i64,
    pub median_hold_s: i64,
    /// Share of sells that were of tokens the entity never bought.
    pub transferred_share: f64,
    pub bait: bool,
}

pub const MIN_PAPER_TRADES: usize = 10;
pub const MIN_MEDIAN_HOLD_S: i64 = 90;
pub const MAX_TRANSFERRED_SHARE: f64 = 0.2;

impl Copyability {
    /// The checks that fail, in words. Empty means copyable.
    pub fn failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.closed < MIN_PAPER_TRADES {
            out.push(format!("{} of {MIN_PAPER_TRADES} paper trades", self.closed));
        }
        if self.pnl_lamports <= 0 {
            out.push("paper result is not positive".to_string());
        }
        if self.closed > 0 && self.median_hold_s < MIN_MEDIAN_HOLD_S {
            out.push(format!("median hold {}s is under {MIN_MEDIAN_HOLD_S}s", self.median_hold_s));
        }
        if self.transferred_share >= MAX_TRANSFERRED_SHARE {
            out.push(format!("{:.0}% of its sells are of tokens it never bought", self.transferred_share * 100.0));
        }
        if self.bait {
            out.push("flagged as bait".to_string());
        }
        out
    }
}

/// Measures an entity from its shadow positions and its own trades.
pub fn copyability(shadow: &[Position], trades: &[Trade], bait: bool) -> Copyability {
    let closed: Vec<&Position> = shadow.iter().filter(|p| p.status == PositionStatus::Closed).collect();
    let mut holds: Vec<i64> = closed.iter().filter_map(|p| p.closed_at.map(|c| c - p.opened_at)).collect();
    holds.sort_unstable();
    let sells: Vec<&Trade> = trades.iter().filter(|t| t.side == Side::Sell).collect();
    let never_bought = sells.iter().filter(|s| !trades.iter().any(|t| t.side == Side::Buy && t.mint == s.mint)).count();
    Copyability {
        closed: closed.len(),
        pnl_lamports: closed.iter().map(|p| p.pnl_lamports()).sum(),
        median_hold_s: holds.get(holds.len() / 2).copied().unwrap_or(0),
        transferred_share: if sells.is_empty() { 0.0 } else { never_bought as f64 / sells.len() as f64 },
        bait,
    }
}
