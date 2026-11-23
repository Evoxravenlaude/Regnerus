//! Every message the bot sends, rendered by pure functions: data in, a
//! `Card` (HTML text plus buttons) out. Nothing here touches the network.

use crate::bot::telegram::{Button, Card};
use crate::model::*;
use crate::obs::budget::{BudgetSnapshot, Lane};
use crate::obs::health::HealthSnapshot;
use crate::store::Counts;
use crate::util::{ago, count, esc, fmt_compact, fmt_units, short};

fn account_url(address: &str) -> String {
    format!("https://solscan.io/account/{address}")
}

fn token_url(mint: &str) -> String {
    format!("https://solscan.io/token/{mint}")
}

fn tx_url(signature: &str) -> String {
    format!("https://solscan.io/tx/{signature}")
}

fn chart_url(mint: &str) -> String {
    format!("https://dexscreener.com/solana/{mint}")
}

fn link(url: &str, text: &str) -> String {
    format!("<a href=\"{url}\">{}</a>", esc(text))
}

/// `0.101 SOL` or `$250.5`.
pub fn quote_text(quote: Quote, raw: u64) -> String {
    match quote {
        Quote::Sol => format!("{} SOL", fmt_units(raw, 9, 4)),
        Quote::Usdc | Quote::Usdt => format!("${}", fmt_units(raw, 6, 2)),
    }
}

fn sol_text(lamports: u64) -> String {
    format!("{} SOL", fmt_units(lamports, 9, 6))
}

fn side_mark(side: Side) -> &'static str {
    match side {
        Side::Buy => "🟢",
        Side::Sell => "🔴",
    }
}

/// `0.101 SOL → 3.5M 2cHN…2c8W` for a buy; the other way round for a sell.
fn swap_line(side: Side, quote: Quote, quote_raw: u64, token_raw: u64, decimals: u8, mint: &str) -> String {
    let token = format!("{} {}", fmt_compact(token_raw, decimals), link(&token_url(mint), &short(mint)));
    let money = quote_text(quote, quote_raw);
    match side {
        Side::Buy => format!("{money} → {token}"),
        Side::Sell => format!("{token} → {money}"),
    }
}

/// How the transaction was built, in the few words that tell wallets apart.
/// Empty when there is nothing to say.
pub fn fingerprint_line(print: &Fingerprint) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(app) = &print.app {
        parts.push(format!("app {}", esc(app)));
    }
    if print.tip_lamports > 0 {
        let family = print.tip_family.as_deref().map(|f| format!(" ({})", esc(f))).unwrap_or_default();
        parts.push(format!("tip {}{family}", sol_text(print.tip_lamports)));
    }
    if print.priority_fee_lamports > 0 {
        parts.push(format!("priority {}", sol_text(print.priority_fee_lamports)));
    }
    if !print.self_paid && !print.fee_payer.is_empty() {
        parts.push(format!("fee paid by {}", link(&account_url(&print.fee_payer), &short(&print.fee_payer))));
    }
    parts.join(" · ")
}

/// The card followers get when an entity buys or sells. `trade` is the
/// newest trade in the signal; a signal of several trades says so.
pub fn signal_card(entity: &Entity, signal: &Signal, trade: &Trade) -> Card {
    let verb = if signal.side == Side::Buy { "BUY" } else { "SELL" };
    let mut lines = vec![
        format!("{} <b>{verb}</b> · <b>{}</b>", side_mark(signal.side), esc(&entity.tag())),
        swap_line(signal.side, signal.quote, signal.quote_amount_raw, signal.token_amount_raw, signal.decimals, &signal.mint),
    ];
    let mut where_line = format!("{} · wallet {}", signal.venue.title(), link(&account_url(&trade.wallet), &short(&trade.wallet)));
    if signal.trades > 1 {
        let noun = if signal.side == Side::Buy { "buys" } else { "sells" };
        let wallets = if signal.wallets.len() == 1 { "1 wallet".to_string() } else { format!("{} wallets", signal.wallets.len()) };
        where_line = format!("{} · {} {noun} by {wallets} in {}s", signal.venue.title(), signal.trades, (signal.last_at - signal.first_at).max(0));
    }
    lines.push(where_line);
    let print = fingerprint_line(&trade.fingerprint);
    if !print.is_empty() {
        lines.push(print);
    }
    if let Some(block_time) = trade.block_time {
        lines.push(format!("seen {}s after the block", (trade.seen_at - block_time).max(0)));
    }
    lines.push(format!("<code>{}</code>", esc(&signal.mint)));
    Card {
        text: lines.join("\n"),
        keyboard: vec![vec![
            Button::url("Transaction", tx_url(&trade.signature)),
            Button::url("Chart", chart_url(&signal.mint)),
            Button::callback(format!("E#{}", entity.id), format!("e:{}", entity.id)),
        ]],
    }
}

/// The same card with a Copy button, for buys once a swap venue is configured.
pub fn with_copy_button(mut card: Card, signal: &Signal) -> Card {
    if signal.side == Side::Buy
        && let Some(row) = card.keyboard.first_mut()
    {
        row.push(Button::callback("Copy", format!("b:{}", signal.id)));
    }
    card
}

fn trade_line(trade: &Trade, now: i64) -> String {
    let when = trade.block_time.unwrap_or(trade.seen_at);
    format!(
        "{} {} · {} · {} ago",
        side_mark(trade.side),
        swap_line(trade.side, trade.quote, trade.quote_amount_raw, trade.token_amount_raw, trade.decimals, &trade.mint),
        trade.venue.title(),
        ago(now - when)
    )
}

fn wallet_line(wallet: &Wallet) -> String {
    let how = match (wallet.state, wallet.linked_by) {
        (WalletState::Linked, Some(by)) => format!("linked ({by})"),
        (state, _) => state.to_string(),
    };
    format!("• {} · {how}", link(&account_url(&wallet.address), &short(&wallet.address)))
}

/// One entity: its wallets, whether this user follows it, its latest trades.
pub fn entity_card(entity: &Entity, wallets: &[Wallet], follow: FollowMode, trades: &[Trade], now: i64) -> Card {
    let standing = if entity.bait {
        " · flagged as bait"
    } else if entity.copyable {
        " · copyable"
    } else {
        ""
    };
    let mut lines = vec![format!("<b>{}</b> · {}{standing}", esc(&entity.tag()), entity.status)];
    lines.push(match follow {
        FollowMode::Off => "You are not following it.".to_string(),
        FollowMode::Cards => "You follow it: a card for every buy and sell.".to_string(),
        FollowMode::Auto => "You follow it on auto: a card for every trade, and its buys are copied with your /settings once it is copyable.".to_string(),
    });
    lines.push(String::new());
    lines.push(format!("<b>Wallets ({})</b>", wallets.len()));
    if wallets.is_empty() {
        lines.push("None.".to_string());
    }
    lines.extend(wallets.iter().map(wallet_line));
    lines.push(String::new());
    lines.push("<b>Recent trades</b>".to_string());
    if trades.is_empty() {
        lines.push("None seen yet.".to_string());
    }
    lines.extend(trades.iter().map(|t| trade_line(t, now)));

    let mut row = match follow {
        FollowMode::Off => vec![Button::callback("Follow", format!("f:{}:cards", entity.id))],
        FollowMode::Cards => vec![Button::callback("Mute", format!("f:{}:off", entity.id)), Button::callback("Auto-copy", format!("f:{}:auto", entity.id))],
        FollowMode::Auto => vec![Button::callback("Mute", format!("f:{}:off", entity.id)), Button::callback("Cards only", format!("f:{}:cards", entity.id))],
    };
    row.push(Button::callback("Refresh", format!("r:{}", entity.id)));
    Card { text: lines.join("\n"), keyboard: vec![row] }
}

/// `/entities`: every entity that is not retired, with this user's follow state.
pub fn entities_card(rows: &[(Entity, usize, FollowMode)]) -> Card {
    if rows.is_empty() {
        return Card::text("No entities yet. Paste a wallet address, or send <code>/track ADDRESS label</code>.");
    }
    let mut lines = vec!["<b>Entities</b>".to_string()];
    let mut keyboard: Vec<Vec<Button>> = Vec::new();
    for (entity, wallets, follow) in rows {
        let noun = if *wallets == 1 { "wallet" } else { "wallets" };
        let following = if *follow == FollowMode::Off { "muted" } else { "following" };
        let status = if entity.status == EntityStatus::Tracking { String::new() } else { format!(" · {}", entity.status) };
        lines.push(format!("<b>{}</b> · {wallets} {noun} · {following}{status}", esc(&entity.tag())));
        if keyboard.last().is_none_or(|row| row.len() >= 4) {
            keyboard.push(Vec::new());
        }
        if keyboard.len() <= 6
            && let Some(row) = keyboard.last_mut()
        {
            row.push(Button::callback(format!("E#{}", entity.id), format!("e:{}", entity.id)));
        }
    }
    Card { text: lines.join("\n"), keyboard }
}

/// What `/wallet` shows about one transfer's other side.
pub struct TransferRow<'a> {
    pub transfer: &'a Transfer,
    /// The label on the counterparty, if it is a known service.
    pub label: Option<Label>,
}

/// Everything `/wallet` shows about one address.
pub struct WalletView<'a> {
    pub address: &'a str,
    pub wallet: Option<&'a Wallet>,
    pub entity: Option<&'a Entity>,
    /// Set when the address itself is a labelled service.
    pub label: Option<&'a Label>,
    pub evidence: &'a [Evidence],
    pub trades: &'a [Trade],
    pub transfers: &'a [TransferRow<'a>],
}

/// `/wallet`: the case file. State, entity, every evidence row, recent
/// trades and transfers.
pub fn wallet_card(view: &WalletView<'_>, now: i64) -> Card {
    let WalletView { address, wallet, entity, label, evidence, trades, transfers } = *view;
    let mut lines = vec![format!("<b>Wallet</b> {}", link(&account_url(address), &short(address))), format!("<code>{}</code>", esc(address))];
    if let Some(label) = label {
        lines.push(format!("Labelled as a service: {} ({})", esc(&label.name), label.class));
    }
    match (wallet, entity) {
        (Some(wallet), Some(entity)) => {
            let how = wallet.linked_by.map(|by| format!(" by {by}")).unwrap_or_default();
            lines.push(format!("{} of <b>{}</b>{how} · score {}", wallet.state, esc(&entity.tag()), wallet.score));
            lines.push(format!("first seen {} ago", ago(now - wallet.first_seen)));
        }
        (Some(wallet), None) => lines.push(format!("{} · not attached to an entity", wallet.state)),
        (None, _) => lines.push("Not known to the bot. Send <code>/track</code> with this address to follow it.".to_string()),
    }
    if !evidence.is_empty() {
        lines.push(String::new());
        lines.push("<b>Evidence</b>".to_string());
        for row in evidence.iter().take(12) {
            let voided = if row.voided { " (voided)" } else { "" };
            lines.push(format!("{:+} {} · {} · {}{voided}", row.points, esc(&row.kind), esc(&row.family), esc(&row.source)));
        }
    }
    if !trades.is_empty() {
        lines.push(String::new());
        lines.push("<b>Recent trades</b>".to_string());
        lines.extend(trades.iter().map(|t| trade_line(t, now)));
    }
    if !transfers.is_empty() {
        lines.push(String::new());
        lines.push("<b>Recent transfers</b>".to_string());
        for row in transfers {
            let t = row.transfer;
            let amount = match &t.mint {
                None => format!("{} SOL", fmt_units(t.amount_raw, 9, 4)),
                Some(mint) => format!("{} {}", fmt_compact(t.amount_raw, t.decimals), link(&token_url(mint), &short(mint))),
            };
            let other = link(&account_url(&t.counterparty), &short(&t.counterparty));
            let named = row.label.as_ref().map(|l| format!(" ({}, {})", esc(&l.name), l.class)).unwrap_or_default();
            let arrow = if t.direction == Direction::In { "in from" } else { "out to" };
            let when = t.block_time.map(|at| format!(" · {} ago", ago(now - at))).unwrap_or_default();
            lines.push(format!("{amount} {arrow} {other}{named}{when}"));
        }
    }
    let mut keyboard = Vec::new();
    if let Some(entity) = entity {
        keyboard.push(vec![Button::callback(format!("E#{}", entity.id), format!("e:{}", entity.id))]);
    }
    Card { text: lines.join("\n"), keyboard }
}

pub fn health_card(health: &HealthSnapshot, counts: &Counts, rpc_host: &str, commitment: &str, now: i64) -> Card {
    let stream = if health.connected { "connected".to_string() } else { "DOWN".to_string() };
    let last_event = if health.last_event_at == 0 { "none yet".to_string() } else { format!("{} ago", ago(now - health.last_event_at)) };
    let delay = match (health.delay_median_s, health.delay_p90_s) {
        (Some(median), Some(p90)) => format!("median {median}s, 9 in 10 within {p90}s ({} trades)", health.delay_samples),
        _ => "no live trades measured yet".to_string(),
    };
    let lines = [
        "<b>Health</b>".to_string(),
        format!("Stream: {stream} · {} · {} · up {}", count(health.subscriptions, "wallet", "wallets"), count(health.connects, "connect", "connects"), ago(now - health.started_at)),
        format!("RPC: {} · {commitment}", esc(rpc_host)),
        format!("Last event: {last_event}"),
        format!(
            "Seen since start: {} · {} · {} · {}",
            count(health.events, "event", "events"),
            count(health.trades, "trade", "trades"),
            count(health.transfers, "transfer", "transfers"),
            count(health.cards, "card", "cards")
        ),
        format!("Missed lookups: {} · errors: {}", health.tx_missing, health.errors),
        format!("Block to seen: {delay}"),
        format!(
            "Stored: {} · {} · {} · {}",
            count(counts.entities, "entity", "entities"),
            count(counts.wallets, "wallet", "wallets"),
            count(counts.trades, "trade", "trades"),
            count(counts.transfers, "transfer", "transfers")
        ),
    ];
    Card::text(lines.join("\n"))
}

pub fn budget_card(budget: &BudgetSnapshot) -> Card {
    let mut lines = vec!["<b>RPC budget</b>".to_string(), format!("Today ({} UTC): {} of {} credits", budget.day, budget.total(), budget.daily_credits)];
    for (lane, spent) in Lane::ALL.iter().zip(budget.spent) {
        let cap = if *lane == Lane::Forensics { format!(" of {}", budget.forensics_cap) } else { String::new() };
        lines.push(format!("• {}: {spent}{cap}", lane.as_str()));
    }
    lines.push(format!("Rate: {:.0} of {:.0} requests a minute", budget.rate_per_min, budget.max_rate));
    if budget.banned_for_s > 0.0 {
        lines.push(format!("Standing down for {:.0}s after a rate limit.", budget.banned_for_s));
    }
    lines.push(format!("Since start: {} requests · {} rate limits", budget.requests, budget.rate_limits));
    Card::text(lines.join("\n"))
}

pub fn help_card(is_admin: bool) -> Card {
    let mut lines = vec![
        "<b>Regnerus</b>".to_string(),
        "Paste a Solana wallet to track it. Each tracked trader is an entity with a permanent id; the bot works out which other wallets are the same trader.".to_string(),
        String::new(),
        "<b>Tracking</b>".to_string(),
        "<code>/track ADDRESS label</code> start tracking a wallet".to_string(),
        "<code>/entities</code> · <code>/entity ID</code> · <code>/wallet ADDRESS</code>".to_string(),
        "<code>/candidates</code> wallets waiting for a decision".to_string(),
        "<code>/trail ADDRESS [days] [min SOL]</code> follow the money to an exchange".to_string(),
        "<code>/map MINT</code> a token's largest holders, grouped".to_string(),
        "<code>/rename ID label</code> · <code>/retire ID</code> · <code>/revive ID</code>".to_string(),
        String::new(),
        "<b>Copying</b>".to_string(),
        "<code>/follow ID off</code>, <code>cards</code> or <code>auto [SOL]</code>".to_string(),
        "<code>/settings</code> · <code>/set KEY VALUE</code> · <code>/live on</code> or <code>off</code>".to_string(),
        "<code>/positions</code> · <code>/close ID</code> · <code>/pnl</code> · <code>/paper</code>".to_string(),
        "<code>/wallets</code> · <code>/newwallet</code> · <code>/usewallet NAME</code>".to_string(),
        "<code>/pause</code> · <code>/resume</code> your cards and copies".to_string(),
        String::new(),
        "<code>/weights</code> · <code>/health</code> · <code>/budget</code>".to_string(),
    ];
    if is_admin {
        lines.push(String::new());
        lines.push("<b>Admin</b>".to_string());
        lines.push("<code>/link ADDRESS ID</code> · <code>/unlink ADDRESS</code> · <code>/confirm ADDRESS</code>".to_string());
        lines.push("<code>/merge FROM INTO</code> · <code>/split ADDRESS</code> · <code>/review ADDRESS</code>".to_string());
        lines.push("<code>/label ADDRESS class name</code> · <code>/unlabel ADDRESS</code> · <code>/labels</code>".to_string());
        lines.push(format!("classes: {}", LabelClass::all().iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ")));
        lines.push("<code>/copyable ID on</code>, <code>off</code> or <code>auto</code> · <code>/weights apply</code>".to_string());
        lines.push("<code>/kill</code> · <code>/unkill</code> stop and restart all new copies".to_string());
        lines.push("<code>/code uses</code> make an invite code · <code>/users</code>".to_string());
    }
    Card::text(lines.join("\n"))
}

pub fn labels_card(labels: &[Label]) -> Card {
    if labels.is_empty() {
        return Card::text("No labels.");
    }
    let mut lines = vec![format!("<b>Labels ({})</b>", labels.len())];
    for label in labels.iter().take(60) {
        lines.push(format!("{} · {} · {} · {}", label.class, esc(&label.name), link(&account_url(&label.address), &short(&label.address)), esc(&label.source)));
    }
    if labels.len() > 60 {
        lines.push(format!("and {} more", labels.len() - 60));
    }
    Card::text(lines.join("\n"))
}

pub fn users_card(accounts: &[Account]) -> Card {
    let mut lines = vec![format!("<b>Users ({})</b>", accounts.len())];
    for account in accounts {
        let mut flags = Vec::new();
        if account.is_admin {
            flags.push("admin");
        }
        if account.paused {
            flags.push("paused");
        }
        let flags = if flags.is_empty() { String::new() } else { format!(" · {}", flags.join(", ")) };
        lines.push(format!("<code>{}</code> {}{flags}", account.id, esc(&account.name)));
    }
    Card::text(lines.join("\n"))
}

// ---- identity: candidates, links, trails ----

use crate::cluster::bubble::MapReport;
use crate::identity::score::{CANDIDATE_AT, LINK_AT, Verdict, describe, family_cap};
use crate::review::calibration::Table;
use crate::trade::gate::Copyability;
use crate::trail::walk::WalkReport;

fn signed(points: i32) -> String {
    if points >= 0 { format!("+{points}") } else { format!("−{}", points.abs()) }
}

/// One row of evidence, with its points.
fn evidence_line(row: &Evidence) -> String {
    let mut line = format!("• {} <b>{}</b>", describe(&row.kind), signed(row.points));
    if let Some(signature) = row.signatures.first().filter(|s| !s.is_empty()) {
        line.push_str(&format!(" · {}", link(&tx_url(signature), "tx")));
    }
    line
}

/// The arithmetic behind a score: every row, then each family after its cap.
fn arithmetic(rows: &[Evidence], verdict: &Verdict) -> Vec<String> {
    let mut lines: Vec<String> = rows.iter().filter(|r| !r.voided).map(evidence_line).collect();
    let families: Vec<String> = verdict
        .families
        .iter()
        .map(|(name, points)| match family_cap(name) {
            Some(cap) => format!("{name} {} (cap {cap})", signed(*points)),
            None => format!("{name} {}", signed(*points)),
        })
        .collect();
    if !families.is_empty() {
        lines.push(families.join(" · "));
    }
    lines
}

/// "Possible new wallet": a wallet reached the candidate threshold.
pub fn candidate_card(entity: &Entity, wallet: &Wallet, rows: &[Evidence], verdict: &Verdict) -> Card {
    let mut lines = vec![
        format!("🟡 <b>Possible new wallet</b> for <b>{}</b>", esc(&entity.tag())),
        format!("{} · score <b>{}</b> (candidate at {CANDIDATE_AT}, linked at {LINK_AT})", link(&account_url(&wallet.address), &short(&wallet.address)), verdict.score),
    ];
    lines.extend(arithmetic(rows, verdict));
    lines.push(if verdict.funding {
        "It is watched from now on. Its trades send no cards until it is linked.".to_string()
    } else {
        "No funding signal yet: behaviour alone never links a wallet.".to_string()
    });
    lines.push(format!("<code>{}</code>", esc(&wallet.address)));
    let a = &wallet.address;
    Card {
        text: lines.join("\n"),
        keyboard: vec![
            vec![Button::callback("Confirm", format!("c:{a}")), Button::callback("Not them", format!("n:{a}"))],
            vec![Button::callback("Review", format!("v:{a}")), Button::callback("Watch only", format!("w:{a}"))],
        ],
    }
}

/// "New wallet linked": proof, a score or an admin attached a wallet.
pub fn linked_card(entity: &Entity, wallet: &Wallet, rows: &[Evidence], verdict: &Verdict) -> Card {
    let how = match wallet.linked_by {
        Some(LinkedBy::Proof) => "by proof: it paid into the entity's own exchange deposit address".to_string(),
        Some(LinkedBy::Score) => format!("by score ({})", verdict.score),
        Some(LinkedBy::Review) => format!("by score with the reviewer's vote ({}); cards only until something else agrees", verdict.score),
        Some(LinkedBy::Admin) => "by an admin".to_string(),
        _ => "as its root".to_string(),
    };
    let mut lines = vec![format!("🔗 <b>New wallet linked</b> to <b>{}</b>", esc(&entity.tag())), format!("{} · {how}", link(&account_url(&wallet.address), &short(&wallet.address)))];
    lines.extend(arithmetic(rows, verdict));
    lines.push("Its trades now count as the entity's.".to_string());
    lines.push(format!("<code>{}</code>", esc(&wallet.address)));
    let a = &wallet.address;
    Card {
        text: lines.join("\n"),
        keyboard: vec![vec![Button::callback("Why", format!("y:{a}")), Button::callback("Unlink", format!("n:{a}")), Button::callback(format!("E#{}", entity.id), format!("e:{}", entity.id))]],
    }
}

/// Wallets waiting for a decision.
pub fn candidates_card(rows: &[(Wallet, Option<Entity>)]) -> Card {
    if rows.is_empty() {
        return Card::text("No candidates. A wallet becomes one when its evidence adds up to 40.");
    }
    let mut lines = vec![format!("<b>Candidates ({})</b>", rows.len())];
    for (wallet, entity) in rows.iter().take(40) {
        let owner = entity.as_ref().map(|e| esc(&e.tag())).unwrap_or_else(|| "no entity".to_string());
        lines.push(format!("• {} · {} · {owner}\n  <code>/wallet {}</code>", link(&account_url(&wallet.address), &short(&wallet.address)), wallet.score, wallet.address));
    }
    Card::text(lines.join("\n"))
}

/// "Entity lost": quiet for days and no successor in sight.
pub fn lost_card(entity: &Entity, quiet_days: i64) -> Card {
    Card {
        text: format!("⚪ <b>{}</b> has been quiet for {quiet_days} days and no candidate wallet has turned up. It may have moved somewhere the trail does not reach.", esc(&entity.tag())),
        keyboard: vec![vec![Button::callback("Retire", format!("x:{}", entity.id)), Button::callback("Keep watching", format!("k:{}", entity.id))]],
    }
}

pub fn deposits_lines(deposits: &[DepositAddr]) -> Vec<String> {
    if deposits.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("<b>Deposit addresses ({})</b>", deposits.len())];
    for deposit in deposits.iter().take(8) {
        let what = match deposit.status {
            DepositStatus::Qualified => "tripwire",
            DepositStatus::Pending => "lead from a walk",
            DepositStatus::Rejected => "too many senders",
        };
        lines.push(format!("• {} · {} · {} · {what}", link(&account_url(&deposit.address), &short(&deposit.address)), esc(&deposit.exchange), count(deposit.senders, "sender", "senders")));
    }
    lines
}

/// The result of a grandfunder walk.
pub fn walk_card(report: &WalkReport, entity: Option<EntityId>, tagged: usize) -> Card {
    let mut lines = vec![
        format!("🧭 <b>Trail of {}</b>", link(&account_url(&report.target), &short(&report.target))),
        format!(
            "{} days · flows of {} and up · {} read · {} spent",
            report.days,
            sol_text(report.min_lamports),
            count(report.visited, "wallet", "wallets"),
            count(report.credits, "credit", "credits")
        ),
    ];
    if let Some(why) = &report.cut_short {
        lines.push(format!("Stopped at {why}: the trail may go further."));
    }
    if report.deposits.is_empty() && report.withdrawals.is_empty() {
        lines.push("No exchange was reached inside these limits.".to_string());
    }
    if !report.deposits.is_empty() {
        lines.push(String::new());
        lines.push("<b>Exchange deposit addresses reached</b>".to_string());
        for found in report.deposits.iter().take(10) {
            let personal = if found.personal { "personal" } else { "too many senders to be one person's" };
            lines.push(format!(
                "• {} · {} · {} away · {} · {personal}",
                link(&account_url(&found.address), &short(&found.address)),
                esc(&found.exchange),
                count(found.hops, "hop", "hops"),
                count(found.senders.len(), "sender", "senders")
            ));
        }
    }
    if !report.withdrawals.is_empty() {
        lines.push(String::new());
        lines.push("<b>Funded from an exchange</b>".to_string());
        for (wallet, exchange, lamports) in report.withdrawals.iter().take(10) {
            lines.push(format!("• {} ← {} · {}", link(&account_url(wallet), &short(wallet)), esc(exchange), sol_text(*lamports)));
        }
    }
    if !report.edges.is_empty() {
        lines.push(String::new());
        lines.push("<b>Largest flows followed</b>".to_string());
        let mut edges = report.edges.clone();
        edges.sort_by_key(|(_, _, lamports, _)| std::cmp::Reverse(*lamports));
        for (from, to, lamports, _) in edges.iter().take(8) {
            lines.push(format!("• {} → {} · {}", link(&account_url(from), &short(from)), link(&account_url(to), &short(to)), sol_text(*lamports)));
        }
    }
    lines.push(String::new());
    lines.push(match (entity, tagged) {
        (Some(id), 0) => format!("The wallet belongs to E#{id}. Nothing new was tagged."),
        (Some(id), n) => format!(
            "Tagged {} for E#{id}. An address the wallet paid into itself is a tripwire; ones further along the trail are leads, and their senders became evidence, not links.",
            count(n, "deposit address", "deposit addresses")
        ),
        (None, _) => "The wallet is not linked to an entity, so nothing was tagged. Track it first to turn deposit addresses into tripwires.".to_string(),
    });
    Card::text(lines.join("\n"))
}

fn share(amount: u64, supply: u64) -> String {
    if supply == 0 { "?".to_string() } else { format!("{:.1}%", amount as f64 * 100.0 / supply as f64) }
}

/// `/map`: a token's largest holders and the groups among them.
pub fn map_card(report: &MapReport) -> Card {
    let mut lines = vec![
        format!("🫧 <b>Holder map</b> · {}", link(&token_url(&report.mint), &short(&report.mint))),
        format!("{} · {} spent", count(report.holders.len(), "largest holder", "largest holders"), count(report.credits, "credit", "credits")),
    ];
    if report.clusters.is_empty() {
        lines.push("No two of them are tied by anything the bot can see.".to_string());
    }
    for (i, cluster) in report.clusters.iter().take(5).enumerate() {
        lines.push(String::new());
        lines.push(format!("<b>Cluster {}</b> · {} · {} of supply", i + 1, count(cluster.members.len(), "wallet", "wallets"), share(cluster.amount, report.supply)));
        lines.push(cluster.members.iter().take(6).map(|m| link(&account_url(m), &short(m))).collect::<Vec<_>>().join(", "));
        for tie in cluster.ties.iter().take(4) {
            lines.push(format!("• {}", esc(tie)));
        }
    }
    lines.push(String::new());
    lines.push("<b>Largest holders</b>".to_string());
    for holder in report.holders.iter().take(10) {
        let who = match (&holder.service, holder.entity) {
            (Some(service), _) => esc(service),
            (None, Some(entity)) => format!("E#{entity}"),
            (None, None) => "wallet".to_string(),
        };
        lines.push(format!("• {} · {} · {who}", link(&account_url(&holder.owner), &short(&holder.owner)), share(holder.amount, report.supply)));
    }
    lines.push(String::new());
    lines.push("A cluster is a question, not a verdict: the ties are a shared funder, a transfer, or the same entity.".to_string());
    Card { text: lines.join("\n"), keyboard: vec![vec![Button::url("Chart", chart_url(&report.mint))]] }
}

/// A token reached a user's consensus threshold.
pub fn consensus_card(mint: &str, entities: &[(Entity, usize)], independent: usize, window_min: u32) -> Card {
    let names: Vec<String> = entities.iter().map(|(entity, wallets)| if *wallets > 1 { format!("{} through {wallets} wallets", esc(&entity.tag())) } else { esc(&entity.tag()) }).collect();
    let text = format!(
        "🤝 <b>Consensus</b> · {}\n{}, {independent} independent, within {window_min} min\n{}\n<code>{}</code>",
        link(&token_url(mint), &short(mint)),
        count(entities.len(), "entity", "entities"),
        names.join(", "),
        esc(mint)
    );
    Card { text, keyboard: vec![vec![Button::url("Chart", chart_url(mint))]] }
}

// ---- trader ----

fn mode_word(mode: PositionMode) -> &'static str {
    match mode {
        PositionMode::Shadow => "shadow",
        PositionMode::Paper => "paper",
        PositionMode::Live => "LIVE",
    }
}

fn pnl_text(position: &Position) -> String {
    let pnl = position.pnl_lamports();
    let pct = if position.entry_lamports > 0 { pnl as f64 * 100.0 / position.entry_lamports as f64 } else { 0.0 };
    format!("{}{} SOL ({pct:+.1}%)", if pnl >= 0 { "+" } else { "−" }, fmt_units(pnl.unsigned_abs(), 9, 4))
}

/// What happened to a copy when it was opened.
pub fn copy_card(position: &Position) -> Card {
    let token = link(&token_url(&position.mint), &short(&position.mint));
    let text = match position.status {
        PositionStatus::Open => format!(
            "✅ <b>Copied ({})</b> · E#{}\n{} → {} {token}\n#{} · {}",
            mode_word(position.mode),
            position.entity_id,
            sol_text(position.entry_lamports),
            fmt_compact(position.tokens_raw, position.decimals),
            position.id,
            esc(&position.note)
        ),
        PositionStatus::Unconfirmed => {
            format!("⏳ <b>Copy sent ({})</b> · E#{}\n{} for {token}\n#{} · {}", mode_word(position.mode), position.entity_id, sol_text(position.entry_lamports), position.id, esc(&position.note))
        }
        _ => format!("⚠️ <b>Not copied ({})</b> · E#{}\n{token}\n{}", mode_word(position.mode), position.entity_id, esc(&position.note)),
    };
    let mut row = vec![Button::url("Chart", chart_url(&position.mint))];
    if position.status == PositionStatus::Open {
        row.push(Button::callback("Close", format!("z:{}", position.id)));
    }
    if let Some(signature) = &position.open_signature {
        row.push(Button::url("Transaction", tx_url(signature)));
    }
    Card { text, keyboard: vec![row] }
}

fn reason_text(reason: &str) -> &'static str {
    match reason {
        "take_profit" => "🎯 Take profit",
        "stop_loss" => "🛑 Stop loss",
        "max_hold" => "⏰ Max hold",
        "source_exit" => "🚪 The entity sold",
        "manual" => "✂️ Closed by you",
        _ => "✂️ Closed",
    }
}

/// A sell of part or all of a position.
pub fn exit_card(position: &Position, reason: &str) -> Card {
    let token = link(&token_url(&position.mint), &short(&position.mint));
    let text = if position.status == PositionStatus::Closed {
        format!(
            "{} · #{} ({})\n{token} · E#{}\nin {} · out {} · <b>{}</b>",
            reason_text(reason),
            position.id,
            mode_word(position.mode),
            position.entity_id,
            sol_text(position.entry_lamports),
            sol_text(position.exit_lamports),
            pnl_text(position)
        )
    } else {
        format!(
            "{} · part of #{} ({})\n{token} · {} left · {} back so far",
            reason_text(reason),
            position.id,
            mode_word(position.mode),
            fmt_compact(position.tokens_left, position.decimals),
            sol_text(position.exit_lamports)
        )
    };
    Card::text(text)
}

pub fn bait_card(entity: &Entity, wallet: &str, mint: &str) -> Card {
    Card::text(format!(
        "🚩 <b>{}</b> is flagged as <b>bait</b>\nA wallet tied to it, {}, sold {} minutes after the entity bought it. Auto-copy of this entity is stopped. An admin can clear the flag with <code>/copyable {} auto</code>.",
        esc(&entity.tag()),
        link(&account_url(wallet), &short(wallet)),
        link(&token_url(mint), &short(mint)),
        entity.id
    ))
}

pub fn positions_card(positions: &[Position], now: i64) -> Card {
    let open: Vec<&Position> = positions.iter().filter(|p| p.is_open()).collect();
    if open.is_empty() {
        return Card::text("No open positions.");
    }
    let mut lines = vec![format!("<b>Open positions ({})</b>", open.len())];
    for position in open.iter().take(30) {
        lines.push(format!(
            "#{} · {} · E#{} · {} · {} in · {} · {} ago\n  <code>/close {}</code>",
            position.id,
            mode_word(position.mode),
            position.entity_id,
            link(&token_url(&position.mint), &short(&position.mint)),
            sol_text(position.entry_lamports),
            position.status,
            ago(now - position.opened_at),
            position.id
        ));
    }
    Card::text(lines.join("\n"))
}

pub fn pnl_card(positions: &[Position]) -> Card {
    let mut lines = vec!["<b>Results</b>".to_string()];
    for mode in [PositionMode::Live, PositionMode::Paper] {
        let closed: Vec<&Position> = positions.iter().filter(|p| p.mode == mode && p.status == PositionStatus::Closed).collect();
        let pnl: i64 = closed.iter().map(|p| p.pnl_lamports()).sum();
        let wins = closed.iter().filter(|p| p.pnl_lamports() > 0).count();
        let spent: u64 = closed.iter().map(|p| p.entry_lamports).sum();
        lines.push(format!("{}: {} closed · {} won · {} in · {}{} SOL", mode_word(mode), closed.len(), wins, sol_text(spent), if pnl >= 0 { "+" } else { "−" }, fmt_units(pnl.unsigned_abs(), 9, 4)));
    }
    let failed = positions.iter().filter(|p| p.status == PositionStatus::Failed).count();
    let open = positions.iter().filter(|p| p.is_open()).count();
    lines.push(format!("{open} open · {failed} not filled"));
    for position in positions.iter().filter(|p| p.status == PositionStatus::Closed).take(8) {
        lines.push(format!("• #{} {} {} · {}", position.id, mode_word(position.mode), link(&token_url(&position.mint), &short(&position.mint)), pnl_text(position)));
    }
    Card::text(lines.join("\n"))
}

/// `/paper`: how a delayed copy of each entity would have done.
pub fn paper_card(rows: &[(Entity, Copyability)]) -> Card {
    if rows.is_empty() {
        return Card::text("No entities yet.");
    }
    let mut lines =
        vec!["<b>Paper copies by entity</b>".to_string(), "Every buy is copied on paper at the price the bot could have got. An entity becomes copyable when it passes every check.".to_string()];
    for (entity, c) in rows.iter().take(30) {
        let failures = c.failures();
        let verdict = if entity.copyable {
            "copyable".to_string()
        } else if failures.is_empty() {
            "passes; promoted at the next check".to_string()
        } else {
            failures.join("; ")
        };
        lines.push(format!(
            "• <b>{}</b> · {} closed · {}{} SOL · median hold {}\n  {}",
            esc(&entity.tag()),
            c.closed,
            if c.pnl_lamports >= 0 { "+" } else { "−" },
            fmt_units(c.pnl_lamports.unsigned_abs(), 9, 4),
            ago(c.median_hold_s),
            esc(&verdict)
        ));
    }
    Card::text(lines.join("\n"))
}

fn opt<T: std::fmt::Display>(value: Option<T>, unit: &str) -> String {
    value.map(|v| format!("{v}{unit}")).unwrap_or_else(|| "off".to_string())
}

pub fn settings_card(settings: &TradeSettings, live_enabled: bool, wallet: Option<&TradingWallet>) -> Card {
    let mode = match (settings.live, live_enabled) {
        (true, true) => "LIVE: real orders",
        (true, false) => "paper (you chose live, but the server's live switch is off)",
        (false, _) => "paper",
    };
    let lines = [
        "<b>Copy settings</b>".to_string(),
        format!("Mode: {mode} · <code>/live on</code> or <code>off</code>"),
        format!("Size per copy: {} · <code>/set size 0.05</code>", sol_text(settings.size_lamports)),
        format!("Take profit: {} · stop loss: {} · max hold: {}", opt(settings.tp_pct, "%"), opt(settings.sl_pct, "%"), opt(settings.max_hold_min, " min")),
        "  <code>/set tp 100</code> · <code>/set sl 30</code> · <code>/set hold 120</code> (or <code>off</code>)".to_string(),
        format!("Open positions limit: {} · daily budget: {}", opt(settings.max_open, ""), settings.daily_cap_lamports.map(sol_text).unwrap_or_else(|| "off".to_string())),
        "  <code>/set maxopen 3</code> · <code>/set daily 0.5</code>".to_string(),
        format!("Ignore entity buys under: {} · <code>/set minsource 0.5</code>", sol_text(settings.min_source_lamports)),
        format!("Slippage: {} · <code>/set slippage 300</code> or <code>auto</code>", settings.slippage_bps.map(|b| format!("{b} bps")).unwrap_or_else(|| "auto".to_string())),
        format!("Consensus: {} entities within {} min · required to copy: {}", settings.min_entities.max(2), settings.window_min.max(1), if settings.require_consensus { "yes" } else { "no" }),
        "  <code>/set entities 2</code> · <code>/set window 10</code> · <code>/set consensus on</code>".to_string(),
        match wallet {
            Some(wallet) => format!("Trading wallet: {} · <code>/wallets</code>", link(&account_url(&wallet.address), &short(&wallet.address))),
            None => "Trading wallet: none · <code>/wallets</code>".to_string(),
        },
        "Copying is per entity: <code>/follow ID auto</code>, or <code>/follow ID auto 0.1</code> for its own size.".to_string(),
    ];
    Card::text(lines.join("\n"))
}

/// `/weights`: how often each score band and each signal turned out right.
pub fn weights_card(table: &Table, is_admin: bool) -> Card {
    let mut lines = vec![
        "<b>Calibration</b>".to_string(),
        format!("{} settled by proof or an admin · {} still open · {} proofs (score-linked wallets are copied from 30)", table.settled, table.open, table.proofs),
    ];
    if table.settled == 0 {
        lines.push("Nothing to grade yet. A candidate is graded when proof or an admin settles it.".to_string());
    }
    if !table.bands.is_empty() {
        lines.push(String::new());
        lines.push("<b>By score when first proposed</b>".to_string());
        for line in &table.bands {
            lines.push(format!("• {}: {} right, {} wrong", line.name, line.right, line.wrong));
        }
    }
    if !table.kinds.is_empty() {
        lines.push(String::new());
        lines.push("<b>By signal</b>".to_string());
        for line in &table.kinds {
            let points = match (line.current, line.proposed) {
                (Some(now), Some(next)) if now != next => format!(" · now {} → measured {}", signed(now), signed(next)),
                (Some(now), Some(_)) => format!(" · {} (measured the same)", signed(now)),
                (Some(now), None) => format!(" · {} (too few to measure)", signed(now)),
                _ => String::new(),
            };
            lines.push(format!("• {}: {} right, {} wrong{points}", line.name, line.right, line.wrong));
        }
    }
    let changes = table.kinds.iter().any(|l| matches!((l.current, l.proposed), (Some(a), Some(b)) if a != b));
    if changes && is_admin {
        lines.push(String::new());
        lines.push("<code>/weights apply</code> puts the measured points in force. Each signal is measured on its own, not fitted jointly.".to_string());
    }
    Card::text(lines.join("\n"))
}
