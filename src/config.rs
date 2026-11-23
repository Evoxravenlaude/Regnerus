//! Every setting the bot reads, in one place.
//!
//! Values come from the environment (Railway's Variables tab in
//! production) or from a local `.env` file. Anything not set falls back to
//! the default written here. Three values are secrets and have no default:
//! the Telegram token, the database URL and the RPC URL (it carries a key).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use crate::chain::solana::rpc::Commitment;

/// Minimal .env reader: KEY=VALUE lines, # comments, optional quotes.
pub fn load_dotenv(path: impl AsRef<Path>) -> HashMap<String, String> {
    let mut found = HashMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return found;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        let key = key.strip_prefix("export ").unwrap_or(key).trim();
        let mut value = value.trim();
        let bytes = value.as_bytes();
        if bytes.len() >= 2 && bytes[0] == bytes[bytes.len() - 1] && (bytes[0] == b'"' || bytes[0] == b'\'') {
            value = &value[1..value.len() - 1];
        }
        if !key.is_empty() {
            found.insert(key.to_string(), value.to_string());
        }
    }
    found
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub telegram_bot_token: String,
    /// Telegram user ids that are admins without an invite code.
    pub admin_ids: BTreeSet<i64>,
    pub database_url: String,
    pub db_schema: String,
    pub solana_rpc_url: String,
    pub solana_ws_url: String,
    pub commitment: Commitment,
    pub rpc_requests_per_min: f64,
    pub rpc_burst: f64,
    pub rpc_daily_credits: i64,
    pub forensics_share: f64,
    /// How often every watched wallet is checked for missed transactions.
    pub backfill_interval_s: u64,
    /// Signatures read per wallet on each check.
    pub backfill_limit: usize,
    /// Transactions read silently when a wallet is first tracked.
    pub history_on_track: usize,
    /// Trades by one entity in one token inside this window are one signal.
    pub signal_window_s: i64,
    /// A trade found later than this is stored without a card.
    pub alert_max_age_s: i64,
    pub log_level: String,
    /// Log Telegram messages instead of sending them.
    pub dry_run: bool,

    // ---- trail ----
    /// The volume filter: SOL sent out below this is not followed.
    pub exit_min_lamports: u64,
    /// The deposit test: more distinct senders than this is not one person's address.
    pub deposit_max_senders: usize,
    /// How long after a deposit a matching exchange withdrawal still counts.
    pub hop_window_s: i64,
    pub walk_days: i64,
    pub walk_min_lamports: u64,
    pub walk_hops: u32,
    pub walk_wallets: usize,
    /// The most credits one walk may spend.
    pub walk_credits: u64,
    /// Transactions read around an entity's buy when looking for co-buyers. Zero turns the scan off.
    pub cobuy_txs: usize,
    /// "Thinly bought": fewer distinct buyers than this in the window.
    pub cobuy_thin: usize,
    /// Pause between forensics jobs when the queue is empty.
    pub forensics_idle_s: u64,

    // ---- trader ----
    /// Size of the bot's own paper copy of every entity buy. Zero turns shadow copies off.
    pub shadow_lamports: u64,
    pub monitor_interval_s: u64,
    /// A buy seen later than this is a card, not a copy: its price is gone.
    pub copy_max_age_s: i64,
    /// Side wallets selling this soon after the entity bought flags it as bait.
    pub bait_window_s: i64,
    pub jupiter_url: String,
    pub jupiter_api_key: String,
    /// Routers left out of orders, so every transaction has one signer and known programs.
    pub jupiter_exclude_routers: String,
    /// Real orders are sent only with both of these set.
    pub live_mode: bool,
    pub live_ack: String,
    /// SOL left in a trading wallet on buys, for fees and rent.
    pub sol_reserve_lamports: u64,
    /// `env` or `doppler`.
    pub secrets_provider: String,
    pub doppler_token: String,
    pub doppler_write_token: String,
    pub doppler_project: String,
    pub doppler_config: String,

    // ---- reviewer ----
    pub anthropic_api_key: String,
    pub anthropic_url: String,
    pub review_model: String,
    pub review_daily_cap: i64,
}

/// What `EXECUTION_LIVE_ACK` must hold before a real order is sent.
pub const LIVE_ACK_PHRASE: &str = "I_UNDERSTAND_THIS_MOVES_REAL_FUNDS";

impl Settings {
    /// Real orders are sent only when the switch is on and the phrase matches.
    pub fn is_live(&self) -> bool {
        self.live_mode && self.live_ack == LIVE_ACK_PHRASE
    }
}

impl Settings {
    /// The process environment, with `./.env` filling the gaps.
    pub fn from_env() -> Settings {
        let dotenv = load_dotenv(".env");
        Settings::from_lookup(&|name| std::env::var(name).ok().or_else(|| dotenv.get(name).cloned()))
    }

    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Settings {
        let text = |name: &str| get(name).map(|v| v.trim().to_string()).unwrap_or_default();
        let number = |name: &str, default: f64| text(name).parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0).unwrap_or(default);
        let truthy = |name: &str| matches!(text(name).to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");

        let solana_rpc_url = text("SOLANA_RPC_URL");
        let mut solana_ws_url = text("SOLANA_WS_URL");
        if solana_ws_url.is_empty() {
            solana_ws_url = ws_url_from(&solana_rpc_url);
        }
        let db_schema = text("DB_SCHEMA");
        Settings {
            telegram_bot_token: text("TELEGRAM_BOT_TOKEN"),
            admin_ids: text("ADMIN_IDS").split([',', ' ', ';']).filter_map(|part| part.trim().parse::<i64>().ok()).collect(),
            database_url: text("DATABASE_URL"),
            db_schema: if db_schema.is_empty() { "regnerus".to_string() } else { db_schema },
            solana_rpc_url,
            solana_ws_url,
            commitment: Commitment::parse(&text("SOLANA_COMMITMENT")).unwrap_or(Commitment::Finalized),
            rpc_requests_per_min: number("RPC_REQUESTS_PER_MIN", 600.0).max(1.0),
            rpc_burst: number("RPC_BURST", 40.0).max(1.0),
            rpc_daily_credits: number("RPC_DAILY_CREDITS", 300_000.0) as i64,
            forensics_share: number("RPC_FORENSICS_SHARE", 0.3).clamp(0.0, 1.0),
            backfill_interval_s: (number("BACKFILL_INTERVAL_S", 300.0) as u64).max(30),
            backfill_limit: (number("BACKFILL_LIMIT", 25.0) as usize).clamp(1, 1000),
            history_on_track: (number("HISTORY_ON_TRACK", 20.0) as usize).min(1000),
            signal_window_s: number("SIGNAL_WINDOW_S", 30.0) as i64,
            alert_max_age_s: number("ALERT_MAX_AGE_S", 3_600.0) as i64,
            log_level: {
                let level = text("LOG_LEVEL");
                if level.is_empty() { "INFO".to_string() } else { level }
            },
            dry_run: truthy("DRY_RUN"),
            exit_min_lamports: (number("EXIT_MIN_SOL", 0.5) * 1e9) as u64,
            deposit_max_senders: (number("DEPOSIT_MAX_SENDERS", 8.0) as usize).max(1),
            hop_window_s: (number("HOP_WINDOW_HOURS", 6.0) * 3_600.0) as i64,
            walk_days: (number("WALK_DAYS", 30.0) as i64).max(1),
            walk_min_lamports: (number("WALK_MIN_SOL", 5.0) * 1e9) as u64,
            walk_hops: (number("WALK_HOPS", 4.0) as u32).clamp(1, 8),
            walk_wallets: (number("WALK_WALLETS", 200.0) as usize).clamp(1, 2_000),
            walk_credits: number("WALK_CREDITS", 1_500.0) as u64,
            cobuy_txs: (number("COBUY_TXS", 20.0) as usize).min(100),
            cobuy_thin: (number("COBUY_THIN", 25.0) as usize).max(2),
            forensics_idle_s: (number("FORENSICS_IDLE_S", 5.0) as u64).max(1),
            shadow_lamports: (number("SHADOW_SOL", 0.1) * 1e9) as u64,
            monitor_interval_s: (number("MONITOR_INTERVAL_S", 20.0) as u64).max(5),
            copy_max_age_s: number("COPY_MAX_AGE_S", 120.0) as i64,
            bait_window_s: number("BAIT_WINDOW_S", 600.0) as i64,
            jupiter_url: {
                let url = text("JUPITER_API_URL");
                if url.is_empty() { "https://api.jup.ag/swap/v2".to_string() } else { url.trim_end_matches('/').to_string() }
            },
            jupiter_api_key: text("JUPITER_API_KEY"),
            jupiter_exclude_routers: get("JUPITER_EXCLUDE_ROUTERS").map(|v| v.trim().to_string()).unwrap_or_else(|| "jupiterz,dflow,okx".to_string()),
            live_mode: truthy("EXECUTION_LIVE_MODE"),
            live_ack: text("EXECUTION_LIVE_ACK"),
            sol_reserve_lamports: (number("SOL_RESERVE", 0.02) * 1e9) as u64,
            secrets_provider: {
                let provider = text("SECRETS_PROVIDER").to_ascii_lowercase();
                if provider.is_empty() { "env".to_string() } else { provider }
            },
            doppler_token: text("DOPPLER_TOKEN"),
            doppler_write_token: text("DOPPLER_WRITE_TOKEN"),
            doppler_project: text("DOPPLER_PROJECT"),
            doppler_config: text("DOPPLER_CONFIG"),
            anthropic_api_key: text("ANTHROPIC_API_KEY"),
            anthropic_url: {
                let url = text("ANTHROPIC_API_URL");
                if url.is_empty() { "https://api.anthropic.com".to_string() } else { url.trim_end_matches('/').to_string() }
            },
            review_model: {
                let model = text("REVIEW_MODEL");
                if model.is_empty() { "claude-sonnet-5-5".to_string() } else { model }
            },
            review_daily_cap: number("REVIEW_DAILY_CAP", 20.0) as i64,
        }
    }

    /// What is missing for a real run, in words a person can act on.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.telegram_bot_token.is_empty() && !self.dry_run {
            out.push("TELEGRAM_BOT_TOKEN is not set.".to_string());
        }
        if self.solana_rpc_url.is_empty() {
            out.push("SOLANA_RPC_URL is not set (an https RPC endpoint with WebSocket support).".to_string());
        } else if !self.solana_rpc_url.starts_with("http") {
            out.push("SOLANA_RPC_URL must start with http:// or https://.".to_string());
        }
        if self.live_mode && self.live_ack != LIVE_ACK_PHRASE {
            out.push(format!("EXECUTION_LIVE_MODE is on but EXECUTION_LIVE_ACK is not the phrase {LIVE_ACK_PHRASE}. Set it to send real orders, or turn EXECUTION_LIVE_MODE off."));
        }
        if !matches!(self.secrets_provider.as_str(), "env" | "doppler") {
            out.push("SECRETS_PROVIDER must be env or doppler.".to_string());
        }
        if self.secrets_provider == "doppler" && (self.doppler_token.is_empty() || self.doppler_project.is_empty() || self.doppler_config.is_empty()) {
            out.push("SECRETS_PROVIDER=doppler needs DOPPLER_TOKEN, DOPPLER_PROJECT and DOPPLER_CONFIG.".to_string());
        }
        if self.admin_ids.is_empty() {
            out.push("ADMIN_IDS is empty: nobody could use the bot. Set it to your Telegram user id.".to_string());
        }
        out
    }
}

/// `https://host/path` becomes `wss://host/path`: most providers serve both
/// on the same host and key.
pub fn ws_url_from(http_url: &str) -> String {
    if let Some(rest) = http_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        String::new()
    }
}
