//! The records the bot keeps. An entity is the permanent id; wallets attach
//! to it by evidence; trades and transfers are what the watcher saw.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type EntityId = i64;
/// A Telegram user id.
pub type AccountId = i64;

/// Declares a text-backed enum: stored as a short lowercase word in the
/// database and parsed back from it.
macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub fn as_str(&self) -> &'static str { match self { $($name::$variant => $text),+ } }
            pub fn parse(text: &str) -> Option<Self> { match text { $($text => Some($name::$variant),)+ _ => None } }
            pub fn all() -> &'static [$name] { &[$($name::$variant),+] }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

text_enum!(
    /// Whether the bot is still following an entity.
    EntityStatus { Tracking => "tracking", Lost => "lost", Retired => "retired" }
);

text_enum!(
    /// How far a wallet has come toward being treated as an entity's own.
    WalletState { Seen => "seen", Candidate => "candidate", Linked => "linked", Rejected => "rejected" }
);

text_enum!(
    /// What made a wallet linked.
    LinkedBy { Root => "root", Proof => "proof", Score => "score", Admin => "admin", Review => "review" }
);

text_enum!(Side { Buy => "buy", Sell => "sell" });

text_enum!(Quote { Sol => "SOL", Usdc => "USDC", Usdt => "USDT" });

impl Quote {
    pub fn decimals(&self) -> u8 {
        match self {
            Quote::Sol => 9,
            Quote::Usdc | Quote::Usdt => 6,
        }
    }
}

text_enum!(Venue {
    PumpFun => "pumpfun",
    PumpSwap => "pumpswap",
    Raydium => "raydium",
    Jupiter => "jupiter",
    Meteora => "meteora",
    Orca => "orca",
    Other => "other",
});

impl Venue {
    pub fn title(&self) -> &'static str {
        match self {
            Venue::PumpFun => "pump.fun",
            Venue::PumpSwap => "PumpSwap",
            Venue::Raydium => "Raydium",
            Venue::Jupiter => "Jupiter",
            Venue::Meteora => "Meteora",
            Venue::Orca => "Orca",
            Venue::Other => "other",
        }
    }
}

text_enum!(TokenProgram { SplToken => "spl-token", Token2022 => "token-2022" });

text_enum!(Direction { In => "in", Out => "out" });

text_enum!(
    /// `Auto` is reserved for the trader; the watcher treats it as `Cards`.
    FollowMode { Off => "off", Cards => "cards", Auto => "auto" }
);

text_enum!(
    /// What kind of non-personal address this is.
    LabelClass {
        CexHot => "cex_hot",
        CexDeposit => "cex_deposit",
        Onramp => "onramp",
        Bridge => "bridge",
        AppSigner => "app_signer",
        AppFee => "app_fee",
        Tip => "tip",
        Program => "program",
        Hub => "hub",
    }
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entity {
    pub id: EntityId,
    pub label: String,
    pub status: EntityStatus,
    pub copyable: bool,
    pub bait: bool,
    pub created_by: AccountId,
    pub created_at: i64,
}

impl Entity {
    /// `E#12 label`, the way every card names an entity.
    pub fn tag(&self) -> String {
        format!("E#{} {}", self.id, self.label)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wallet {
    pub address: String,
    pub entity_id: Option<EntityId>,
    pub state: WalletState,
    pub score: i32,
    pub linked_by: Option<LinkedBy>,
    pub first_seen: i64,
    /// Newest signature the backfill has processed for this wallet. `None`
    /// until the wallet is first read; empty when it was read and had no
    /// transactions yet.
    pub last_signature: Option<String>,
}

/// One reason a wallet belongs to an entity. Never deleted, only voided.
#[derive(Clone, Debug, PartialEq)]
pub struct Evidence {
    pub id: i64,
    pub wallet: String,
    pub entity_id: EntityId,
    pub kind: String,
    pub family: String,
    pub points: i32,
    pub signatures: Vec<String>,
    pub details: Value,
    pub source: String,
    pub voided: bool,
    pub observed_at: i64,
}

/// The evidence a caller supplies; the store assigns the id.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEvidence {
    pub wallet: String,
    pub entity_id: EntityId,
    pub kind: String,
    pub family: String,
    pub points: i32,
    pub signatures: Vec<String>,
    pub details: Value,
    pub source: String,
    pub observed_at: i64,
}

/// How a transaction was built: the Solana form of "execution DNA".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub fee_payer: String,
    /// The wallet paid its own fee. False for app-signed trades.
    pub self_paid: bool,
    pub wallet_signed: bool,
    pub cu_limit: Option<u32>,
    /// Micro-lamports per compute unit, compared as an exact value.
    pub cu_price: Option<u64>,
    /// Network fee above the base fee per signature.
    pub priority_fee_lamports: u64,
    pub tip_lamports: u64,
    pub tip_account: Option<String>,
    /// `jito`, or the name of an app's own landing accounts.
    pub tip_family: Option<String>,
    /// Trading app, from a labelled fee wallet or a labelled signer.
    pub app: Option<String>,
    pub app_fee_lamports: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trade {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    /// When this process saw it; `seen_at - block_time` is the copy delay.
    pub seen_at: i64,
    pub wallet: String,
    pub entity_id: Option<EntityId>,
    pub side: Side,
    pub mint: String,
    pub token_amount_raw: u64,
    pub decimals: u8,
    pub token_program: Option<TokenProgram>,
    pub quote: Quote,
    pub quote_amount_raw: u64,
    pub programs: Vec<String>,
    pub venue: Venue,
    pub fingerprint: Fingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub wallet: String,
    pub direction: Direction,
    /// The other owner. For a token account whose owner the transaction
    /// does not reveal, the token account itself.
    pub counterparty: String,
    /// `None` is native SOL.
    pub mint: Option<String>,
    pub amount_raw: u64,
    pub decimals: u8,
}

/// One entity's buy (or sell) of one token: every trade by its linked
/// wallets within the signal window, added together.
#[derive(Clone, Debug, PartialEq)]
pub struct Signal {
    pub id: i64,
    pub entity_id: EntityId,
    pub mint: String,
    pub side: Side,
    pub first_at: i64,
    pub last_at: i64,
    pub quote: Quote,
    pub quote_amount_raw: u64,
    pub token_amount_raw: u64,
    pub decimals: u8,
    pub wallets: Vec<String>,
    pub trades: u32,
    pub venue: Venue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: AccountId,
    pub name: String,
    pub is_admin: bool,
    pub paused: bool,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Follow {
    pub account: AccountId,
    pub entity_id: EntityId,
    pub mode: FollowMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub address: String,
    pub class: LabelClass,
    pub name: String,
    /// `seed`, `admin`, or the service a label was learned from.
    pub source: String,
}

// ---- trail, trader and reviewer records ----

text_enum!(JobStatus { Queued => "queued", Running => "running", Done => "done", Failed => "failed" });

/// One unit of forensics work, queued so it can wait for budget and
/// survive a restart.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    /// The address (or mint) the job is about. One live job per kind and subject.
    pub subject: String,
    pub entity_id: Option<EntityId>,
    pub payload: Value,
    pub status: JobStatus,
    pub attempts: i32,
    pub run_after: i64,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NewJob {
    pub kind: String,
    pub subject: String,
    pub entity_id: Option<EntityId>,
    pub payload: Value,
    pub run_after: i64,
}

text_enum!(
    /// `Qualified` passed the deposit test and is a tripwire. `Rejected`
    /// has too many senders to be one person's address.
    DepositStatus { Pending => "pending", Qualified => "qualified", Rejected => "rejected" }
);

/// An exchange deposit address an entity has paid into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositAddr {
    pub address: String,
    pub entity_id: EntityId,
    pub exchange: String,
    pub status: DepositStatus,
    pub senders: i32,
    pub first_seen: i64,
}

text_enum!(
    /// `Shadow` is the bot's own paper copy of an entity, kept to measure
    /// whether a delayed copy of it makes money.
    PositionMode { Shadow => "shadow", Paper => "paper", Live => "live" }
);

text_enum!(PositionStatus { Opening => "opening", Open => "open", Closed => "closed", Failed => "failed", Unconfirmed => "unconfirmed" });

/// The account id that owns shadow positions.
pub const SHADOW_ACCOUNT: AccountId = 0;

/// One copy of one signal. Amounts of SOL are lamports; token amounts are raw units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub id: i64,
    pub account: AccountId,
    pub entity_id: EntityId,
    pub signal_id: i64,
    pub mint: String,
    pub decimals: u8,
    pub mode: PositionMode,
    pub status: PositionStatus,
    pub entry_lamports: u64,
    pub tokens_raw: u64,
    pub tokens_left: u64,
    pub exit_lamports: u64,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
    pub exit_reason: Option<String>,
    pub open_signature: Option<String>,
    /// Why it failed, or what the screen said.
    pub note: String,
}

impl Position {
    pub fn is_open(&self) -> bool {
        matches!(self.status, PositionStatus::Open | PositionStatus::Opening | PositionStatus::Unconfirmed)
    }

    /// Realised result in lamports. Meaningful once closed.
    pub fn pnl_lamports(&self) -> i64 {
        self.exit_lamports as i64 - self.entry_lamports as i64
    }
}

/// A user's copy settings. Stored as one JSON document per account.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TradeSettings {
    /// SOL spent on each copy, in lamports. Zero means "show me, never copy".
    pub size_lamports: u64,
    /// `None` lets the venue choose.
    pub slippage_bps: Option<u16>,
    pub tp_pct: Option<u32>,
    pub sl_pct: Option<u32>,
    pub max_hold_min: Option<u32>,
    pub max_open: Option<u32>,
    pub daily_cap_lamports: Option<u64>,
    /// Ignore entity buys smaller than this.
    pub min_source_lamports: u64,
    pub require_consensus: bool,
    pub min_entities: u32,
    pub window_min: u32,
    /// Real money when the server allows it; paper otherwise.
    pub live: bool,
}

impl Default for TradeSettings {
    fn default() -> Self {
        TradeSettings {
            size_lamports: 50_000_000,
            slippage_bps: None,
            tp_pct: Some(100),
            sl_pct: Some(30),
            max_hold_min: Some(120),
            max_open: Some(3),
            daily_cap_lamports: Some(500_000_000),
            min_source_lamports: 0,
            require_consensus: false,
            min_entities: 2,
            window_min: 10,
            live: false,
        }
    }
}

/// Where a user's trading key lives. The key itself is never stored here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradingWallet {
    pub address: String,
    /// The name of the secret in the key provider.
    pub secret_ref: String,
}
